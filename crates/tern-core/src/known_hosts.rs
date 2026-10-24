//! The host keys the user trusts, for the Known hosts screen: OpenSSH's
//! `~/.ssh/known_hosts`, which Tern only reads, and Tern's own file, whose
//! entries can be forgotten.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::PublicKey;
use sha1::Sha1;

use crate::paths;
use crate::randomart::Randomart;
use crate::ssh::{fingerprint, key_art, key_kind};

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
    /// In Tern's own file, so it can be forgotten.
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

/// The known_hosts files, each with whether it is Tern's own: OpenSSH's
/// first, then Tern's.
pub fn files() -> Vec<(PathBuf, bool)> {
    let mut files: Vec<(PathBuf, bool)> = paths::system_known_hosts()
        .into_iter()
        .map(|file| (file, false))
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

/// Removes `entry` from Tern's own file, provided its line still holds the
/// same key. OpenSSH's file is never written.
pub fn forget(entry: &KnownHost) -> Result<(), String> {
    if !entry.ours {
        return Err("OpenSSH's known_hosts is only read".into());
    }
    forget_in(&entry.file, entry.line, &entry.fingerprint)
}

fn forget_in(file: &Path, line: usize, expected: &str) -> Result<(), String> {
    let text = std::fs::read_to_string(file).map_err(|error| error.to_string())?;
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
    paths::write_atomically(file, rest.as_bytes()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::{Algorithm, PrivateKey};

    fn key() -> PublicKey {
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .unwrap()
            .public_key()
            .clone()
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
}
