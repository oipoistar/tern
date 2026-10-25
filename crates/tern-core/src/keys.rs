//! SSH key pairs for the Keychain screen: those in `~/.ssh` and those in
//! Tern's own `keys` folder. Keys Tern generates, imports or receives go into
//! `~/.ssh` on Linux and macOS, as `ssh-keygen` does, and into its own folder
//! on Windows. It deletes only keys it put there itself.

use std::path::{Path, PathBuf};

use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey, PublicKey};

use crate::host::{Auth, Host};
use crate::paths;
use crate::randomart::Randomart;
use crate::ssh::{fingerprint, key_art, key_kind};
use crate::vault::Vault;

/// RSA keys shorter than this are weak by today's advice.
const RSA_MIN_BITS: u32 = 3072;
/// The file in Tern's own folder noting the keys it put in `~/.ssh`, a line
/// of fingerprint and name each: those are Tern's to delete there.
const MADE: &str = ".made";
/// Files in `~/.ssh` that are never keys.
const NOT_KEYS: [&str; 5] = [
    "known_hosts",
    "known_hosts.old",
    "authorized_keys",
    "config",
    "environment",
];

/// A private key file and its public half.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyPair {
    /// The private key's file name.
    pub name: String,
    pub path: PathBuf,
    /// The public key as an `authorized_keys` line: `type base64 comment`.
    pub public: String,
    pub kind: String,
    pub fingerprint: String,
    pub art: Randomart,
    /// Lives on a security key (`sk-*`), so signing needs the device.
    pub hardware: bool,
    /// RSA under 3072 bits, or DSA: worth replacing.
    pub weak: bool,
    /// Tern's, so it can be deleted: in Tern's own folder, or put in
    /// `~/.ssh` by Tern.
    pub ours: bool,
}

impl KeyPair {
    /// Whether every whitespace-separated term of `query` appears
    /// (case-insensitively) in the name, type or fingerprint.
    pub fn matches(&self, query: &str) -> bool {
        let haystack = format!("{} {} {}", self.name, self.kind, self.fingerprint).to_lowercase();
        query
            .split_whitespace()
            .all(|term| haystack.contains(&term.to_lowercase()))
    }

    /// Saved hosts that sign in with this key.
    pub fn users<'a>(&self, vault: &'a Vault) -> Vec<&'a Host> {
        let mine = std::fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
        vault
            .hosts()
            .filter(|host| match &host.auth {
                Auth::Key(path) => {
                    std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()) == mine
                }
                _ => false,
            })
            .collect()
    }
}

/// Where keys are looked for, each with whether it is Tern's own folder.
pub fn dirs() -> Vec<(PathBuf, bool)> {
    let mut dirs: Vec<(PathBuf, bool)> = paths::ssh_dir()
        .map(|dir| (dir, false))
        .into_iter()
        .collect();
    dirs.push((paths::keys(), true));
    dirs
}

/// Where the keys Tern makes, imports or receives go: `~/.ssh` on Linux and
/// macOS, so ssh and git find them too; Tern's own folder on Windows, or
/// when `dirs` has no `~/.ssh`.
pub fn home(dirs: &[(PathBuf, bool)]) -> Option<PathBuf> {
    let find = |ours: bool| {
        dirs.iter()
            .find(|(_, is_ours)| *is_ours == ours)
            .map(|(dir, _)| dir.clone())
    };
    if cfg!(unix) {
        find(false).or_else(|| find(true))
    } else {
        find(true)
    }
}

/// Tern's own key folder among `dirs`.
fn own(dirs: &[(PathBuf, bool)]) -> Option<&Path> {
    dirs.iter()
        .find(|(_, ours)| *ours)
        .map(|(dir, _)| dir.as_path())
}

/// The keys noted as put in `~/.ssh` by Tern: fingerprint and name.
fn made(dirs: &[(PathBuf, bool)]) -> Vec<(String, String)> {
    let Some(own) = own(dirs) else {
        return Vec::new();
    };
    std::fs::read_to_string(own.join(MADE))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(fingerprint, name)| (fingerprint.to_string(), name.to_string()))
        .collect()
}

fn write_made(dirs: &[(PathBuf, bool)], made: &[(String, String)]) -> Result<(), String> {
    let Some(own) = own(dirs) else {
        return Ok(());
    };
    let text: String = made
        .iter()
        .map(|(fingerprint, name)| format!("{fingerprint} {name}\n"))
        .collect();
    paths::write_atomically(&own.join(MADE), text.as_bytes()).map_err(|error| error.to_string())
}

/// `pair`, just written by Tern, as Tern's: noted as such when it went
/// somewhere other than Tern's own folder.
fn claim(dirs: &[(PathBuf, bool)], mut pair: KeyPair) -> Result<KeyPair, String> {
    pair.ours = true;
    if pair.path.parent() != own(dirs) {
        let mut made = made(dirs);
        made.push((pair.fingerprint.clone(), pair.name.clone()));
        write_made(dirs, &made)?;
    }
    Ok(pair)
}

/// Every key pair in `dirs`, by folder, then by name.
pub fn list_in(dirs: &[(PathBuf, bool)]) -> Vec<KeyPair> {
    let made = made(dirs);
    let mut pairs = Vec::new();
    for (dir, ours) in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut found: Vec<KeyPair> = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .filter_map(|entry| read_pair(&entry.path(), *ours))
            .map(|mut pair| {
                pair.ours |= made.iter().any(|(fingerprint, name)| {
                    *fingerprint == pair.fingerprint && *name == pair.name
                });
                pair
            })
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        pairs.extend(found);
    }
    pairs
}

/// The key pair whose private key is `path`, if it is one: its public half
/// comes from the `.pub` file beside it, or from the key itself (OpenSSH
/// keeps the public half readable even in an encrypted key).
fn read_pair(path: &Path, ours: bool) -> Option<KeyPair> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    if name.ends_with(".pub") || name.ends_with(".tmp") || NOT_KEYS.contains(&name.as_str()) {
        return None;
    }
    let from_pub = || {
        let text = std::fs::read_to_string(public_path(path)).ok()?;
        PublicKey::from_openssh(text.trim()).ok()
    };
    let from_private = || {
        // Keys are small; anything big is something else.
        if std::fs::metadata(path).ok()?.len() > 64 * 1024 {
            return None;
        }
        let text = std::fs::read_to_string(path).ok()?;
        Some(
            PrivateKey::from_openssh(text.trim())
                .ok()?
                .public_key()
                .clone(),
        )
    };
    // The private key must exist and look like one either way.
    let head = std::fs::read(path).ok()?;
    if !head.starts_with(b"-----BEGIN") {
        return None;
    }
    let key = from_pub().or_else(from_private)?;
    Some(KeyPair {
        name,
        path: path.to_path_buf(),
        public: key.to_openssh().ok()?,
        kind: key_kind(&key),
        fingerprint: fingerprint(&key),
        art: key_art(&key),
        hardware: matches!(
            key.algorithm(),
            Algorithm::SkEd25519 | Algorithm::SkEcdsaSha2NistP256
        ),
        weak: weak(&key),
        ours,
    })
}

/// RSA under 3072 bits, or DSA.
pub fn weak(key: &PublicKey) -> bool {
    match key.algorithm() {
        Algorithm::Dsa => true,
        Algorithm::Rsa { .. } => key
            .key_data()
            .rsa()
            .is_some_and(|rsa| rsa.key_size() < RSA_MIN_BITS),
        _ => false,
    }
}

fn public_path(private: &Path) -> PathBuf {
    let mut public = private.as_os_str().to_owned();
    public.push(".pub");
    PathBuf::from(public)
}

/// Why `name` can't be a new key's file name in `dir`, if it can't.
pub fn check_name(dir: &Path, name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("Name the key".into());
    }
    if name.contains(['/', '\\', ':']) || name.starts_with('.') || name.ends_with(".pub") {
        return Err("A key's name can't contain / \\ or :, start with a dot or end in .pub".into());
    }
    if dir.join(name).exists() || public_path(&dir.join(name)).exists() {
        return Err("A key by that name already exists".into());
    }
    Ok(())
}

/// `base`, or `base-2`, `base-3`… whichever is free in `dir` first.
pub fn free_name(dir: &Path, base: &str) -> String {
    (1..)
        .map(|n| match n {
            1 => base.to_string(),
            n => format!("{base}-{n}"),
        })
        .find(|name| check_name(dir, name).is_ok())
        .expect("a free name")
}

/// `name` made fit for a key's file name, so a free one can be found from
/// it: no path separators, colons or leading dots, and no `.pub` ending.
fn usable_name(name: &str) -> String {
    let name = name.trim().replace(['/', '\\', ':'], "-");
    let name = name.trim_start_matches('.');
    let name = name.strip_suffix(".pub").unwrap_or(name);
    if name.is_empty() {
        "key".into()
    } else {
        name.into()
    }
}

/// Makes a new Ed25519 key pair where Tern puts keys ([`home`]), encrypted
/// when a passphrase is given. The private key is readable by its owner
/// only.
pub fn generate(
    dirs: &[(PathBuf, bool)],
    name: &str,
    passphrase: Option<&str>,
    comment: &str,
) -> Result<KeyPair, String> {
    let dir = home(dirs).ok_or("There's no folder for keys")?;
    check_name(&dir, name)?;
    let mut rng = rand::rng();
    let mut key =
        PrivateKey::random(&mut rng, Algorithm::Ed25519).map_err(|error| error.to_string())?;
    key.set_comment(comment);
    let public = key
        .public_key()
        .to_openssh()
        .map_err(|error| error.to_string())?;
    let key = match passphrase.filter(|passphrase| !passphrase.is_empty()) {
        Some(passphrase) => key
            .encrypt(&mut rng, passphrase)
            .map_err(|error| error.to_string())?,
        None => key,
    };
    let text = key
        .to_openssh(LineEnding::LF)
        .map_err(|error| error.to_string())?;
    let path = dir.join(name.trim());
    write_pair(&path, text.as_bytes(), &public)?;
    let pair = read_pair(&path, true).ok_or("the new key can't be read back")?;
    claim(dirs, pair)
}

/// Copies the private key at `from` (and its public half) to where Tern
/// puts keys, under a name not taken yet.
pub fn import(dirs: &[(PathBuf, bool)], from: &Path) -> Result<KeyPair, String> {
    let original =
        read_pair(from, true).ok_or("That file isn't an SSH private key Tern can read")?;
    let dir = home(dirs).ok_or("There's no folder for keys")?;
    let canonical = |path: &Path| std::fs::canonicalize(path).ok();
    if from.parent().and_then(canonical) == canonical(&dir) {
        return Err(format!("{} is already in {}", original.name, dir.display()));
    }
    let private = std::fs::read(from).map_err(|error| error.to_string())?;
    add(dirs, &original.name, &private, &original.public)
}

/// Writes a key pair where Tern puts keys ([`home`]), as Tern's, under
/// `name` or the first free name after it.
pub fn add(
    dirs: &[(PathBuf, bool)],
    name: &str,
    private: &[u8],
    public: &str,
) -> Result<KeyPair, String> {
    let dir = home(dirs).ok_or("There's no folder for keys")?;
    let path = dir.join(free_name(&dir, &usable_name(name)));
    write_pair(&path, private, public)?;
    let pair = read_pair(&path, true).ok_or("the key can't be read back")?;
    claim(dirs, pair)
}

/// Copies a key pair into the folder `to`, never over an existing file.
/// Returns where the private key went.
pub fn export(pair: &KeyPair, to: &Path) -> Result<PathBuf, String> {
    let path = to.join(&pair.name);
    if path.exists() || public_path(&path).exists() {
        return Err(format!("{} already has a {}", to.display(), pair.name));
    }
    let private = std::fs::read(&pair.path).map_err(|error| error.to_string())?;
    write_private(&path, &private)?;
    std::fs::write(public_path(&path), format!("{}\n", pair.public))
        .map_err(|error| error.to_string())?;
    Ok(path)
}

/// Deletes a key pair that's Tern's ([`KeyPair::ours`]). Other keys in
/// `~/.ssh` are never deleted.
pub fn delete(dirs: &[(PathBuf, bool)], pair: &KeyPair) -> Result<(), String> {
    if !pair.ours {
        return Err("Tern deletes only keys it added".into());
    }
    std::fs::remove_file(&pair.path).map_err(|error| error.to_string())?;
    let public = public_path(&pair.path);
    if public.exists() {
        std::fs::remove_file(public).map_err(|error| error.to_string())?;
    }
    let mut made = made(dirs);
    let noted = made.len();
    made.retain(|(fingerprint, name)| *fingerprint != pair.fingerprint || *name != pair.name);
    if made.len() != noted {
        write_made(dirs, &made)?;
    }
    Ok(())
}

/// A POSIX shell command that adds `public` to the remote user's
/// `authorized_keys` unless it's already there, as `ssh-copy-id` does.
pub fn install_command(public: &str) -> String {
    let quoted = format!("'{}'", public.trim().replace('\'', r"'\''"));
    format!(
        "umask 077; mkdir -p ~/.ssh && touch ~/.ssh/authorized_keys && \
         (grep -qxF {quoted} ~/.ssh/authorized_keys || echo {quoted} >> ~/.ssh/authorized_keys)"
    )
}

/// Writes a key pair: the private key readable by its owner only, the
/// public half beside it.
fn write_pair(path: &Path, private: &[u8], public: &str) -> Result<(), String> {
    write_private(path, private)?;
    std::fs::write(public_path(path), format!("{}\n", public.trim()))
        .map_err(|error| error.to_string())
}

/// Writes a private key so only its owner can read it, as OpenSSH insists.
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write as _;

    if let Some(dir) = path.parent() {
        paths::private_dir(dir).map_err(|error| error.to_string())?;
    }
    paths::private_file(path)
        .and_then(|mut file| file.write_all(bytes))
        .map_err(|error| error.to_string())?;
    restrict_to_owner(path).map_err(|error| format!("can't restrict {}: {error}", path.display()))
}

fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").map_err(std::io::Error::other)?;
        let status = std::process::Command::new(paths::system_program("icacls.exe"))
            .arg(path)
            .args(["/inheritance:r", "/grant:r", &format!("{user}:F")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other("icacls failed"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tern-keys-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const RSA_2048: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDCx/wi6QiCzaJnLo5+/N5/gFeq8j3snfGj74PO9kwQHot1iA70yIoXHhzXQ7JhkeTWLVsoAXU2QYbLCbZPTK2FtBUp7sYNCedtyyA/tj8DNXm3CWP/kotBULqeKuCAVNcmGAWolHjzDPbfK9J4rIeMErZ2FH4l9M7be3RCW0xvpsTTbK6FbRDdfmivyJ0WzlAb9QZ1snQrbSwxoxlask1OgGL8bX3WisKFY92lalJV2M1WFDwUwAs6EGJNp5+16JzjhtrpJuc49OPRXEKBhd8WI+9zYaM2lPbgkh+ylx/y5kat7EDg/yXOMWrQQ5o/8Qlzzke91FfnVX7+EK/iSoW/";

    /// Just `dir`, as Tern's own folder: keys made there go nowhere else.
    fn only(dir: &Path) -> [(PathBuf, bool); 1] {
        [(dir.to_path_buf(), true)]
    }

    #[test]
    fn generates_lists_and_deletes_terns_keys() {
        let (ssh, ours) = (scratch("list-ssh"), scratch("list-ours"));
        let plain = generate(&only(&ours), "id_tern", None, "me@laptop").unwrap();
        let locked = generate(&only(&ours), "locked", Some("hunter2"), "me@laptop").unwrap();
        assert_eq!(plain.kind, "ed25519");
        assert!(plain.public.ends_with("me@laptop"));
        assert!(!plain.weak && !plain.hardware && plain.ours);
        // The passphrase protects the key; its public half still lists.
        assert!(russh::keys::load_secret_key(&locked.path, None).is_err());
        assert!(russh::keys::load_secret_key(&locked.path, Some("hunter2")).is_ok());

        // A key in ~/.ssh with only its private half, a weak RSA public
        // key with a PEM private key beside it, and files that aren't keys.
        let lone = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        std::fs::write(
            ssh.join("id_lone"),
            lone.to_openssh(LineEnding::LF).unwrap(),
        )
        .unwrap();
        std::fs::write(ssh.join("id_rsa"), "-----BEGIN RSA PRIVATE KEY-----\n...").unwrap();
        std::fs::write(ssh.join("id_rsa.pub"), RSA_2048).unwrap();
        std::fs::write(ssh.join("config"), "Host *\n").unwrap();
        std::fs::write(ssh.join("known_hosts"), "").unwrap();
        std::fs::write(ssh.join("notes.txt"), "hello").unwrap();

        let dirs = [(ssh.clone(), false), (ours.clone(), true)];
        let pairs = list_in(&dirs);
        let names: Vec<(&str, bool)> = pairs
            .iter()
            .map(|pair| (pair.name.as_str(), pair.ours))
            .collect();
        assert_eq!(
            names,
            [
                ("id_lone", false),
                ("id_rsa", false),
                ("id_tern", true),
                ("locked", true)
            ]
        );
        assert!(pairs[1].weak && pairs[1].kind == "rsa");
        assert_eq!(pairs[0].fingerprint, fingerprint(lone.public_key()));

        assert!(delete(&dirs, &pairs[0]).is_err(), "not Tern's");
        assert!(ssh.join("id_lone").exists());
        delete(&dirs, &pairs[2]).unwrap();
        assert!(!ours.join("id_tern").exists() && !ours.join("id_tern.pub").exists());
        for dir in [ssh, ours] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn names_imports_and_exports_never_overwrite() {
        let (ours, elsewhere) = (scratch("names-ours"), scratch("names-elsewhere"));
        assert!(check_name(&ours, " ").is_err());
        assert!(check_name(&ours, "../evil").is_err());
        assert!(check_name(&ours, "id.pub").is_err());
        let first = generate(&only(&ours), "work", None, "").unwrap();
        assert_eq!(
            check_name(&ours, "work"),
            Err("A key by that name already exists".into())
        );
        assert!(generate(&only(&ours), "work", None, "").is_err());

        // Importing the same file twice keeps both copies apart.
        let outside = generate(&only(&elsewhere), "work", None, "").unwrap();
        let imported = import(&only(&ours), &outside.path).unwrap();
        assert_eq!(imported.name, "work-2");
        assert_eq!(imported.fingerprint, outside.fingerprint);
        assert!(import(&only(&ours), &elsewhere.join("work.pub")).is_err());
        // A key already where keys go isn't copied beside itself.
        assert!(import(&only(&ours), &first.path).is_err());

        // A name from elsewhere that can't be a file name here still finds
        // one, as a synced key's might.
        let private = std::fs::read(&first.path).unwrap();
        let odd = add(&only(&ours), "work:prod/x.pub", &private, &first.public).unwrap();
        assert_eq!(odd.name, "work-prod-x");
        assert_eq!(usable_name(" ci:prod/key "), "ci-prod-key");
        assert_eq!(usable_name("..hidden.pub"), "hidden");
        assert_eq!(usable_name("."), "key");

        let out = scratch("names-out");
        assert_eq!(export(&first, &out).unwrap(), out.join("work"));
        assert!(out.join("work.pub").exists());
        assert!(export(&first, &out).is_err(), "never over an existing file");
        for dir in [ours, elsewhere, out] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn makes_keys_where_openssh_looks_and_deletes_only_its_own() {
        let (ssh, ours, elsewhere) = (
            scratch("home-ssh"),
            scratch("home-ours"),
            scratch("home-elsewhere"),
        );
        let dirs = [(ssh.clone(), false), (ours.clone(), true)];
        // In ~/.ssh on Linux and macOS, as ssh-keygen would; in Tern's own
        // folder on Windows.
        let home = if cfg!(unix) { &ssh } else { &ours };
        assert_eq!(super::home(&dirs).as_ref(), Some(home));
        let made = generate(&dirs, "id_made", None, "").unwrap();
        let outside = generate(&only(&elsewhere), "id_outside", None, "").unwrap();
        let imported = import(&dirs, &outside.path).unwrap();
        for pair in [&made, &imported] {
            assert!(pair.ours && pair.path.starts_with(home), "{pair:?}");
        }
        // The user's own key beside them stays theirs.
        generate(&only(&ssh), "id_theirs", None, "").unwrap();
        let listed = list_in(&dirs);
        let find = |name: &str| listed.iter().find(|pair| pair.name == name).unwrap();
        assert!(find("id_made").ours && find("id_outside").ours);
        assert!(!find("id_theirs").ours);
        assert!(delete(&dirs, find("id_theirs")).is_err());
        assert!(ssh.join("id_theirs").exists());

        delete(&dirs, find("id_made")).unwrap();
        assert!(!made.path.exists());
        // Another key by that name later is the user's, not Tern's.
        generate(&only(&ssh), "id_made", None, "").unwrap();
        let again = list_in(&dirs)
            .into_iter()
            .find(|pair| pair.name == "id_made" && pair.path.starts_with(&ssh))
            .unwrap();
        assert!(!again.ours);
        for dir in [ssh, ours, elsewhere] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn knows_which_hosts_use_a_key() {
        let ours = scratch("users");
        let pair = generate(&only(&ours), "deploy", None, "").unwrap();
        let mut vault = Vault::sample();
        let mut db = vault.find("prod-db-01").unwrap().clone();
        db.auth = Auth::Key(pair.path.clone());
        vault.save(Some("prod-db-01"), db, "Production");
        let users: Vec<&str> = pair
            .users(&vault)
            .iter()
            .map(|host| host.name.as_str())
            .collect();
        assert_eq!(users, ["prod-db-01"]);
        std::fs::remove_dir_all(ours).unwrap();
    }

    #[test]
    fn the_install_command_quotes_the_key() {
        let command = install_command("ssh-ed25519 AAAA me's key\n");
        assert!(command.contains(r"grep -qxF 'ssh-ed25519 AAAA me'\''s key'"));
        assert!(command.contains(r">> ~/.ssh/authorized_keys)"));
    }
}
