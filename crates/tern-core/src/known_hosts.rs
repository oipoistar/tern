//! The host keys the user trusts: OpenSSH's `~/.ssh/known_hosts` and Tern's
//! own file, read as OpenSSH reads them to check a host's key, and listed
//! for the Known hosts screen. Entries of a file Tern trusts keys in can be
//! forgotten: its own, and on Linux and macOS OpenSSH's too. On Windows
//! OpenSSH's is only read.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::{Algorithm, PublicKey};
use sha1::Sha1;

use crate::paths;
use crate::randomart::Randomart;
use crate::ssh::{fingerprint, key_art, key_kind};

/// What the known_hosts files say of the key a host showed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A line trusts it for the host.
    Trusted,
    /// No line names the host.
    Unknown,
    /// Lines name the host with other keys, possibly of another type:
    /// possibly an attack. The first of them, counted from 1.
    Changed { file: PathBuf, line: usize },
    /// A `@revoked` line names the key.
    Revoked { file: PathBuf, line: usize },
}

/// A known_hosts line's marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    None,
    /// `@revoked`: never trust the key.
    Revoked,
    /// `@cert-authority`: trust host certificates it signed.
    Authority,
}

/// One known_hosts line: its marker, host patterns and key.
struct Entry<'a> {
    marker: Marker,
    patterns: &'a str,
    key: PublicKey,
}

/// `line` read as an entry; `None` for a blank line, a comment, a marker
/// OpenSSH doesn't have or a key Tern can't read.
fn entry(line: &str) -> Option<Entry<'_>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (marker, rest) = match line.split_once(char::is_whitespace) {
        Some(("@revoked", rest)) => (Marker::Revoked, rest.trim_start()),
        Some(("@cert-authority", rest)) => (Marker::Authority, rest.trim_start()),
        _ if line.starts_with('@') => return None,
        _ => (Marker::None, line),
    };
    let (patterns, key) = rest.split_once(char::is_whitespace)?;
    let key = PublicKey::from_openssh(key.trim()).ok()?;
    Some(Entry {
        marker,
        patterns,
        key,
    })
}

/// The entries of each of `files` that take `host:port`, with where they
/// are. A missing file has none; one that can't be read is an error.
fn entries_for(
    files: &[PathBuf],
    host: &str,
    port: u16,
) -> Result<Vec<(Marker, PublicKey, PathBuf, usize)>, String> {
    let mut found = Vec::new();
    for file in files {
        let text = match std::fs::read_to_string(file) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("can't read {}: {error}", file.display())),
        };
        for (index, line) in text.lines().enumerate() {
            if let Some(entry) = entry(line).filter(|entry| takes(entry.patterns, host, port)) {
                found.push((entry.marker, entry.key, file.clone(), index + 1));
            }
        }
    }
    Ok(found)
}

/// What `files` say of `key`, shown by `host:port`, as OpenSSH reads them:
/// a `@revoked` line for the key wins over any that trusts it, and any
/// other key for the host is a change, whatever its type. Connections ask
/// for the types already known first ([`known_types`]), so a server that
/// still has its known key shows that one.
pub fn verdict(
    files: &[PathBuf],
    host: &str,
    port: u16,
    key: &PublicKey,
) -> Result<Verdict, String> {
    let mut trusted = false;
    let mut changed = None;
    for (marker, known, file, line) in entries_for(files, host, port)? {
        let same = known.key_data() == key.key_data();
        match marker {
            Marker::Revoked if same => return Ok(Verdict::Revoked { file, line }),
            Marker::None if same => trusted = true,
            Marker::None => {
                changed.get_or_insert(Verdict::Changed { file, line });
            }
            Marker::Revoked | Marker::Authority => {}
        }
    }
    Ok(if trusted {
        Verdict::Trusted
    } else {
        changed.unwrap_or(Verdict::Unknown)
    })
}

/// The `@revoked` line of `files` that names `key` for `host:port`, if
/// one does: how a revoked certificate authority is told too.
pub fn revoked(
    files: &[PathBuf],
    host: &str,
    port: u16,
    key: &PublicKey,
) -> Option<(PathBuf, usize)> {
    entries_for(files, host, port)
        .ok()?
        .into_iter()
        .find(|(marker, known, ..)| {
            *marker == Marker::Revoked && known.key_data() == key.key_data()
        })
        .map(|(_, _, file, line)| (file, line))
}

/// The types of the keys `files` trust for `host:port`, which a connection
/// asks the server for first.
pub fn known_types(files: &[PathBuf], host: &str, port: u16) -> Vec<Algorithm> {
    let mut types: Vec<Algorithm> = Vec::new();
    for (marker, key, ..) in entries_for(files, host, port).unwrap_or_default() {
        if marker == Marker::None && !types.contains(&key.algorithm()) {
            types.push(key.algorithm());
        }
    }
    types
}

/// Whether OpenSSH's comma-separated host `patterns` take `host:port`:
/// `*` and `?` wildcards, `[host]:port` for ports other than 22, hashed
/// names (`|1|salt|hash`), and `!` to exclude. Names compare ignoring case.
pub(crate) fn takes(patterns: &str, host: &str, port: u16) -> bool {
    let name = |host: &str| {
        if port == 22 {
            host.to_string()
        } else {
            format!("[{host}]:{port}")
        }
    };
    let (given, lower) = (name(host), name(&host.to_lowercase()));
    let mut matched = false;
    for pattern in patterns.split(',') {
        let (excluded, pattern) = match pattern.strip_prefix('!') {
            Some(pattern) => (true, pattern),
            None => (false, pattern),
        };
        // OpenSSH hashes the name in lower case; a name written as typed
        // may not be.
        let hit = match pattern.strip_prefix("|1|") {
            Some(hashed) => hashes(hashed, &lower) || hashes(hashed, &given),
            None => crate::certs::wildcard(&pattern.to_lowercase(), &lower),
        };
        if hit {
            if excluded {
                return false;
            }
            matched = true;
        }
    }
    matched
}

/// Whether a hashed name, `salt|hash` after its `|1|`, is `name`'s.
fn hashes(hashed: &str, name: &str) -> bool {
    let engine = base64::engine::general_purpose::STANDARD;
    let Some((salt, hash)) = hashed.split_once('|') else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (engine.decode(salt), engine.decode(hash)) else {
        return false;
    };
    Hmac::<Sha1>::new_from_slice(&salt)
        .is_ok_and(|mac| mac.chain_update(name).verify_slice(&hash).is_ok())
}

/// Adds a line trusting `key` for `host:port` to the known_hosts file at
/// `path`, as OpenSSH writes one: `host` on port 22, else `[host]:port`.
/// A name that would read as more than one host there (several names,
/// patterns, a marker or a comment) is refused rather than written.
pub fn learn(path: &Path, host: &str, port: u16, key: &PublicKey) -> Result<(), String> {
    let plain = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%'));
    if !plain {
        return Err(format!("{host} isn't a host name a key can be trusted for"));
    }
    let name = if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    };
    let key = key.to_openssh().map_err(|error| error.to_string())?;
    // Only the type and key: a comment from the server has no place here.
    let key = key.split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    // A last line without its newline gets one first.
    let mut last = [0u8; 1];
    let ends_line = file.seek(SeekFrom::End(-1)).is_err()
        || (file.read_exact(&mut last).is_ok() && last[0] == b'\n');
    let line = format!("{}{name} {key}\n", if ends_line { "" } else { "\n" });
    file.write_all(line.as_bytes())
        .map_err(|error| error.to_string())
}

/// One trusted key, as a line of a known_hosts file gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownHost {
    /// Who the key is for, `host` or `host:port`, several joined by " / ".
    /// `None` when OpenSSH stored the names hashed.
    pub hosts: Option<String>,
    pub kind: String,
    pub fingerprint: String,
    pub art: Randomart,
    pub file: PathBuf,
    /// Counted from 1, as editors and OpenSSH's messages do.
    pub line: usize,
    /// In a file Tern writes, so it can be forgotten.
    pub ours: bool,
}

impl KnownHost {
    /// Whether every whitespace-separated term of `query` appears
    /// (case-insensitively) in the hosts, key type or fingerprint.
    pub fn matches(&self, query: &str) -> bool {
        let haystack = format!(
            "{} {} {}",
            self.hosts.as_deref().unwrap_or_default(),
            self.kind,
            self.fingerprint
        )
        .to_lowercase();
        query
            .split_whitespace()
            .all(|term| haystack.contains(&term.to_lowercase()))
    }
}

/// The known_hosts files, each with whether Tern writes it: OpenSSH's first
/// (written on Linux and macOS), then Tern's.
pub fn files() -> Vec<(PathBuf, bool)> {
    let mut files: Vec<(PathBuf, bool)> = paths::system_known_hosts()
        .into_iter()
        .map(|file| (file, cfg!(unix)))
        .collect();
    files.push((paths::known_hosts(), true));
    files
}

/// Every key `files` trust, in order. A missing file has none; lines that
/// hold no usable key (comments, `@revoked`, `@cert-authority`, unknown key
/// types) are left out.
pub fn list_in(files: &[(PathBuf, bool)]) -> Vec<KnownHost> {
    let mut known = Vec::new();
    for (file, ours) in files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            if let Some((hosts, key)) = parse_line(line) {
                known.push(KnownHost {
                    hosts,
                    kind: key_kind(&key),
                    fingerprint: fingerprint(&key),
                    art: key_art(&key),
                    file: file.clone(),
                    line: index + 1,
                    ours: *ours,
                });
            }
        }
    }
    known
}

/// The keys the known_hosts file at `path` holds for each of `places`
/// (address and port), reading it once for all of them, as a host card's
/// art needs for every host. Names match as they do for a connection:
/// `host`, `[host]:port` off port 22, or either hashed (`|1|`).
pub fn keys_for(places: &[(String, u16)], path: &Path) -> HashMap<(String, u16), Vec<PublicKey>> {
    let mut found: HashMap<(String, u16), Vec<PublicKey>> = HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return found;
    };
    // The name each place has in a known_hosts file.
    let names: Vec<(String, &(String, u16))> = places
        .iter()
        .map(|place| {
            let (host, port) = place;
            let name = if *port == 22 {
                host.clone()
            } else {
                format!("[{host}]:{port}")
            };
            (name, place)
        })
        .collect();
    let by_name: HashMap<&str, &(String, u16)> = names
        .iter()
        .map(|(name, place)| (name.as_str(), *place))
        .collect();
    let engine = base64::engine::general_purpose::STANDARD;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('@') {
            continue;
        }
        let Some((patterns, key)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let mut matched: Vec<&(String, u16)> = Vec::new();
        for pattern in patterns.split(',') {
            if let Some(hashed) = pattern.strip_prefix("|1|") {
                let Some((salt, hash)) = hashed.split_once('|') else {
                    continue;
                };
                let (Ok(salt), Ok(hash)) = (engine.decode(salt), engine.decode(hash)) else {
                    continue;
                };
                let Ok(mac) = Hmac::<Sha1>::new_from_slice(&salt) else {
                    continue;
                };
                for (name, place) in &names {
                    if mac.clone().chain_update(name).verify_slice(&hash).is_ok() {
                        matched.push(place);
                    }
                }
            } else if let Some(place) = by_name.get(pattern) {
                matched.push(place);
            }
        }
        if matched.is_empty() {
            continue;
        }
        let Ok(key) = PublicKey::from_openssh(key.trim()) else {
            continue;
        };
        for place in matched {
            found.entry(place.clone()).or_default().push(key.clone());
        }
    }
    found
}

/// The hosts and key on one known_hosts line: `patterns keytype key
/// [comment]`.
fn parse_line(line: &str) -> Option<(Option<String>, PublicKey)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('@') {
        return None;
    }
    let (patterns, key) = line.split_once(char::is_whitespace)?;
    let key = PublicKey::from_openssh(key.trim()).ok()?;
    let hosts: Vec<String> = patterns
        .split(',')
        .filter(|pattern| !pattern.starts_with("|1|"))
        .map(shown_pattern)
        .collect();
    Some(((!hosts.is_empty()).then(|| hosts.join(" / ")), key))
}

/// `[host]:port` as `host:port`; anything else as written.
fn shown_pattern(pattern: &str) -> String {
    pattern
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("]:"))
        .map_or_else(
            || pattern.to_string(),
            |(host, port)| format!("{host}:{port}"),
        )
}

/// Removes `entry` from its file, as `ssh-keygen -R` would, provided its line
/// still holds the same key and Tern writes that file.
pub fn forget(entry: &KnownHost) -> Result<(), String> {
    if !entry.ours {
        return Err("OpenSSH's known_hosts is only read".into());
    }
    forget_in(&entry.file, entry.line, &entry.fingerprint)
}

fn forget_in(file: &Path, line: usize, expected: &str) -> Result<(), String> {
    // A linked file (dotfiles kept elsewhere) changes where it lives, and
    // the link stays.
    let file = std::fs::canonicalize(file).map_err(|error| error.to_string())?;
    let text = std::fs::read_to_string(&file).map_err(|error| error.to_string())?;
    let mut lines: Vec<&str> = text.lines().collect();
    let found = line
        .checked_sub(1)
        .and_then(|index| lines.get(index))
        .and_then(|line| parse_line(line))
        .is_some_and(|(_, key)| fingerprint(&key) == expected);
    if !found {
        return Err("the file changed; open the screen again".into());
    }
    lines.remove(line - 1);
    let mut rest = lines.join("\n");
    if !rest.is_empty() {
        rest.push('\n');
    }
    paths::write_atomically(&file, rest.as_bytes()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::{Algorithm, PrivateKey};

    fn key() -> PublicKey {
        key_of(Algorithm::Ed25519)
    }

    fn key_of(algorithm: Algorithm) -> PublicKey {
        PrivateKey::random(&mut rand::rng(), algorithm)
            .unwrap()
            .public_key()
            .clone()
    }

    /// `name` hashed as OpenSSH's `HashKnownHosts` writes it.
    fn hashed(name: &str) -> String {
        let salt = [7u8; 20];
        let mac = Hmac::<Sha1>::new_from_slice(&salt)
            .unwrap()
            .chain_update(name)
            .finalize()
            .into_bytes();
        let engine = base64::engine::general_purpose::STANDARD;
        format!("|1|{}|{}", engine.encode(salt), engine.encode(mac))
    }

    #[test]
    fn host_patterns_work_as_in_openssh() {
        assert!(takes("*.example.com", "db.example.com", 22));
        assert!(takes("db.EXAMPLE.com", "DB.example.com", 22));
        assert!(takes("a,b,c?", "c1", 22));
        assert!(!takes(
            "*.example.com,!db.example.com",
            "db.example.com",
            22
        ));
        assert!(!takes("*.example.com", "example.org", 22));
        // Other ports go by `[host]:port`.
        assert!(!takes("*.example.com", "db.example.com", 2222));
        assert!(takes("[*.example.com]:2222", "db.example.com", 2222));
        assert!(takes("*", "db.example.com", 2222));
        // Hashed, as OpenSSH writes the name: in lower case.
        assert!(takes(&hashed("db.example.com"), "DB.example.com", 22));
        assert!(takes(&hashed("[db]:2222"), "db", 2222));
        assert!(!takes(&hashed("db.example.com"), "web.example.com", 22));
    }

    #[test]
    fn checks_a_hosts_key_as_openssh_does() {
        let dir = std::env::temp_dir().join(format!("tern-verdict-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("known_hosts");
        let ecdsa = key_of(Algorithm::Ecdsa {
            curve: russh::keys::ssh_key::EcdsaCurve::NistP256,
        });
        let (ed, other, gone) = (key(), key(), key());
        let line = |key: &PublicKey| key.to_openssh().unwrap();
        let text = [
            "# a comment".to_string(),
            String::new(),
            format!("db.example.com {}", line(&ed)),
            format!("web.example.com {}", line(&other)),
            format!("*.lab,!secret.lab {}", line(&ed)),
            format!("@revoked * {}", line(&gone)),
            format!("@cert-authority *.example.com {}", line(&ecdsa)),
            format!("twice.example.com {}", line(&other)),
            format!("twice.example.com {} a comment", line(&ed)),
            format!("{} {}", hashed("hashed.example.com"), line(&ed)),
        ];
        std::fs::write(&file, text.join("\n")).unwrap();
        let files = [file.clone(), dir.join("missing")];
        let check = |host: &str, key: &PublicKey| verdict(&files, host, 22, key).unwrap();
        let changed = |line| Verdict::Changed {
            file: file.clone(),
            line,
        };

        assert_eq!(check("db.example.com", &ed), Verdict::Trusted);
        assert_eq!(check("hashed.example.com", &ed), Verdict::Trusted);
        assert_eq!(check("nowhere.example.com", &ed), Verdict::Unknown);
        // Another key for the host, at its line counting comments and gaps.
        assert_eq!(check("web.example.com", &ed), changed(4));
        // Another type of key for a known host is a change too, not new.
        assert_eq!(check("db.example.com", &ecdsa), changed(3));
        // Patterns, and the hosts they leave out.
        assert_eq!(check("box.lab", &ed), Verdict::Trusted);
        assert_eq!(check("secret.lab", &ed), Verdict::Unknown);
        // Any line with the key will do, comment or not.
        assert_eq!(check("twice.example.com", &ed), Verdict::Trusted);
        // An authority isn't a host key.
        assert_eq!(check("api.example.com", &ecdsa), Verdict::Unknown);
        // Revoked wins, whatever trusts the key.
        std::fs::write(
            &file,
            format!("{}\ndb.example.com {}\n", text[5], line(&gone)),
        )
        .unwrap();
        let revoked_line = Verdict::Revoked {
            file: file.clone(),
            line: 1,
        };
        assert_eq!(check("db.example.com", &gone), revoked_line);
        assert_eq!(
            revoked(&files, "elsewhere", 2222, &gone),
            Some((file.clone(), 1))
        );
        assert_eq!(revoked(&files, "elsewhere", 22, &ed), None);
        assert_eq!(
            known_types(&files, "db.example.com", 22),
            [Algorithm::Ed25519]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn learns_keys_for_plain_names_only() {
        let dir = std::env::temp_dir().join(format!("tern-learn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("known_hosts");
        let (old, a, b) = (key(), key(), key());
        // A last line without its newline.
        std::fs::write(
            &file,
            format!("old.example.com {}", old.to_openssh().unwrap()),
        )
        .unwrap();
        learn(&file, "db.example.com", 22, &a).unwrap();
        learn(&file, "10.0.0.5", 2222, &b).unwrap();
        learn(&file, "fe80::1%eth0", 22, &b).unwrap();
        for name in [
            "",
            "a,b",
            "a b",
            "*",
            "db?",
            "!db",
            "#db",
            "@revoked",
            "|1|x|y",
            "[db]:22",
            "db\nevil.example.com",
        ] {
            assert!(learn(&file, name, 22, &a).is_err(), "{name:?}");
        }
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                format!("old.example.com {}", old.to_openssh().unwrap()),
                format!("db.example.com {}", a.to_openssh().unwrap()),
                format!("[10.0.0.5]:2222 {}", b.to_openssh().unwrap()),
                format!("fe80::1%eth0 {}", b.to_openssh().unwrap()),
            ]
        );
        let files = [file];
        assert_eq!(
            verdict(&files, "10.0.0.5", 2222, &b).unwrap(),
            Verdict::Trusted
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_every_hosts_keys_in_one_read() {
        let dir = std::env::temp_dir().join(format!("tern-keys-for-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_hosts");
        let (plain, hashed, ported, other) = (key(), key(), key(), key());
        let salt = [7u8; 20];
        let mac = Hmac::<Sha1>::new_from_slice(&salt)
            .unwrap()
            .chain_update("db.example.com")
            .finalize()
            .into_bytes();
        let engine = base64::engine::general_purpose::STANDARD;
        let lines = [
            "# comment".to_string(),
            format!("web.example.com,10.0.0.5 {}", plain.to_openssh().unwrap()),
            format!(
                "|1|{}|{} {}",
                engine.encode(salt),
                engine.encode(mac),
                hashed.to_openssh().unwrap()
            ),
            format!("[10.0.0.5]:2222 {}", ported.to_openssh().unwrap()),
            format!("@revoked web.example.com {}", other.to_openssh().unwrap()),
            format!("elsewhere.example.com {}", other.to_openssh().unwrap()),
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let places: Vec<(String, u16)> = [
            ("web.example.com", 22),
            ("10.0.0.5", 22),
            ("db.example.com", 22),
            ("10.0.0.5", 2222),
            ("nowhere.example.com", 22),
        ]
        .map(|(host, port)| (host.to_string(), port))
        .into();
        let found = keys_for(&places, &path);
        let keys = |host: &str, port: u16| found.get(&(host.to_string(), port)).cloned();
        assert_eq!(keys("web.example.com", 22), Some(vec![plain.clone()]));
        assert_eq!(keys("10.0.0.5", 22), Some(vec![plain]));
        assert_eq!(keys("db.example.com", 22), Some(vec![hashed]));
        assert_eq!(keys("10.0.0.5", 2222), Some(vec![ported]));
        assert_eq!(keys("nowhere.example.com", 22), None);
        // As a connection's check reads the same file, a host at a time.
        for (host, port) in &places {
            let one: Vec<PublicKey> =
                russh::keys::known_hosts::known_host_keys_path(host, *port, &path)
                    .unwrap()
                    .into_iter()
                    .map(|(_, key)| key)
                    .collect();
            assert_eq!(keys(host, *port).unwrap_or_default(), one, "{host}:{port}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lists_both_files_with_readable_names() {
        let dir = std::env::temp_dir().join(format!("tern-known-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, c) = (key(), key(), key());
        let system = dir.join("system");
        let text = format!(
            "# comment\n\
             github.com,140.82.121.4 {a}\n\
             |1|c2FsdA==|aGFzaA== {b}\n\
             @revoked bad.example {c}\n\
             broken line\n",
            a = a.to_openssh().unwrap(),
            b = b.to_openssh().unwrap(),
            c = c.to_openssh().unwrap(),
        );
        std::fs::write(&system, text).unwrap();
        let ours = dir.join("ours");
        std::fs::write(
            &ours,
            format!("[127.0.0.1]:2222 {}\n", c.to_openssh().unwrap()),
        )
        .unwrap();

        let known = list_in(&[
            (system.clone(), false),
            (ours, true),
            (dir.join("none"), true),
        ]);
        let shown: Vec<(Option<&str>, usize, bool)> = known
            .iter()
            .map(|known| (known.hosts.as_deref(), known.line, known.ours))
            .collect();
        assert_eq!(
            shown,
            [
                (Some("github.com / 140.82.121.4"), 2, false),
                (None, 3, false),
                (Some("127.0.0.1:2222"), 1, true),
            ]
        );
        assert_eq!(known[0].kind, "ed25519");
        assert_eq!(known[0].fingerprint, fingerprint(&a));
        assert_eq!(known[2].art, key_art(&c));

        // Search looks at hosts, type and fingerprint.
        assert!(known[0].matches("GITHUB ed25519"));
        assert!(known[2].matches(":2222"));
        assert!(known[1].matches(&known[1].fingerprint[7..15]));
        assert!(!known[1].matches("github"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn forgets_only_from_terns_file_and_only_the_same_key() {
        let dir = std::env::temp_dir().join(format!("tern-known-forget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (key(), key());
        let ours = dir.join("known_hosts");
        let text = format!(
            "one {}\ntwo {}\n",
            a.to_openssh().unwrap(),
            b.to_openssh().unwrap()
        );
        std::fs::write(&ours, &text).unwrap();
        let known = list_in(&[(ours.clone(), true)]);

        let system = KnownHost {
            ours: false,
            ..known[0].clone()
        };
        assert!(forget(&system).is_err());
        assert_eq!(std::fs::read_to_string(&ours).unwrap(), text);

        forget(&known[0]).unwrap();
        let left = list_in(&[(ours.clone(), true)]);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].hosts.as_deref(), Some("two"));
        // The list is stale now: line 2 is gone, so nothing is removed.
        assert!(forget(&known[1]).is_err());
        assert_eq!(list_in(&[(ours, true)]).len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn forgets_in_a_linked_file_and_keeps_the_link() {
        let dir = std::env::temp_dir().join(format!("tern-known-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (key(), key());
        let kept = dir.join("dotfiles-known_hosts");
        let text = format!(
            "one {}\ntwo {}\n",
            a.to_openssh().unwrap(),
            b.to_openssh().unwrap()
        );
        std::fs::write(&kept, text).unwrap();
        let link = dir.join("known_hosts");
        std::os::unix::fs::symlink(&kept, &link).unwrap();

        forget(&list_in(&[(link.clone(), true)])[0]).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        let left = list_in(&[(kept, true)]);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].hosts.as_deref(), Some("two"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
