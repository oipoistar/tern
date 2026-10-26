//! Where Tern keeps its own files, and how it writes them.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Tern's per-user data directory. `TERN_DATA_DIR` overrides it (tests and
/// portable installs).
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("TERN_DATA_DIR") {
        return dir.into();
    }
    let home = || std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(home)
            .join("Tern")
    } else if cfg!(target_os = "macos") {
        home().join("Library/Application Support/Tern")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local/share"))
            .join("tern")
    }
}

/// The vaults Tern keeps, and which is open.
pub fn vaults() -> PathBuf {
    data_dir().join("vaults.json")
}

/// Where the vault `id` keeps its own files: the data folder itself for
/// the first vault, `vaults/<id>/` in it for others.
pub fn vault_dir(id: &str) -> PathBuf {
    if id == crate::vaults::FIRST {
        data_dir()
    } else {
        data_dir().join("vaults").join(id)
    }
}

/// The saved hosts of the vault `id`.
pub fn vault(id: &str) -> PathBuf {
    vault_dir(id).join("vault.json")
}

/// How Tern looks and its terminals behave.
pub fn settings() -> PathBuf {
    data_dir().join("settings.json")
}

/// The vault `id`'s recent connections.
pub fn history(id: &str) -> PathBuf {
    vault_dir(id).join("history.json")
}

/// The tabs open when Tern last quit.
pub fn tabs() -> PathBuf {
    data_dir().join("tabs.json")
}

/// The commands waiting for the vault `id`'s hosts to come back, which
/// never sync.
pub fn waiting(id: &str) -> PathBuf {
    vault_dir(id).join("waiting.json")
}

/// The commands that fill the vault `id`'s groups here, which never sync.
pub fn sources(id: &str) -> PathBuf {
    vault_dir(id).join("sources.json")
}

/// Key pairs Tern generated or imported.
pub fn keys() -> PathBuf {
    data_dir().join("keys")
}

/// Tern's own known hosts: where it trusts host keys on Windows, kept apart
/// from `~/.ssh/known_hosts`, which it only reads there.
pub fn known_hosts() -> PathBuf {
    data_dir().join("known_hosts")
}

/// Where a host key the user trusts in Tern goes: OpenSSH's own file on
/// Linux and macOS, so `ssh` in a terminal trusts it too; Tern's on Windows,
/// or when there's no home folder.
pub fn trusted_known_hosts() -> PathBuf {
    if cfg!(unix)
        && let Some(file) = system_known_hosts()
    {
        return file;
    }
    known_hosts()
}

/// OpenSSH's per-user folder, `~/.ssh`, whose keys the Keychain lists (and
/// where Tern puts keys on Linux and macOS). `TERN_SSH_DIR` overrides it so
/// checks never show or touch the developer's keys.
pub fn ssh_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("TERN_SSH_DIR") {
        return Some(dir.into());
    }
    std::env::home_dir().map(|home| home.join(".ssh"))
}

/// OpenSSH's per-user known hosts file. `TERN_SYSTEM_KNOWN_HOSTS` overrides
/// it so tests never depend on the developer's own file.
pub fn system_known_hosts() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TERN_SYSTEM_KNOWN_HOSTS") {
        return Some(path.into());
    }
    std::env::home_dir().map(|home| home.join(".ssh").join("known_hosts"))
}

/// A program that comes with Windows, by its full path in System32 (`name`
/// may start with a folder there). Run by its name alone, a program is
/// looked for first in the folder of the program starting it: Tern's own,
/// perhaps a Downloads folder, where a file of that name could be waiting.
pub fn system_program(name: &str) -> PathBuf {
    std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("windir"))
        .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from)
        .join("System32")
        .join(name)
}

/// Whether `path` starts with two slashes, as `\\server\share\…` does:
/// Windows opens such a path over the network (or as a device), signing in
/// to the computer it names as the user. The long forms (`\\?\…`) count
/// too; no file Tern is handed needs them.
pub fn network_path(path: &Path) -> bool {
    let text = path.as_os_str().to_string_lossy();
    let mut start = text.chars();
    let slash = |c: Option<char>| matches!(c, Some('/' | '\\'));
    slash(start.next()) && slash(start.next())
}

/// Writes `bytes` to `path` through a temporary file renamed over the old
/// one, so a crash mid-write never leaves half a file. Creates the folder.
/// On Unix, files and folders made are the user's alone (0600 and 0700):
/// a vault names hosts, users and commands.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        private_dir(dir)?;
    }
    // A temporary file of its own, so writes at once (two windows, or two
    // Terns on one folder) never write into one another's.
    let (temp, mut file) = loop {
        let mut temp = path.as_os_str().to_owned();
        temp.push(format!(".{:08x}.tmp", rand::random::<u32>()));
        let temp = PathBuf::from(temp);
        match new_private_file(&temp) {
            Ok(file) => break (temp, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    let replaced = written.and_then(|()| replace(&temp, path));
    if replaced.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    replaced
}

/// Renames `from` over `to`. Windows refuses for a moment while another
/// rename over `to` is under way, so it's tried again then.
fn replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut tries = 0;
    loop {
        match std::fs::rename(from, to) {
            Err(error)
                if cfg!(windows)
                    && error.kind() == std::io::ErrorKind::PermissionDenied
                    && tries < 100 =>
            {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            done => return done,
        }
    }
}

/// Creates `dir` and the folders above it that are missing, the ones made
/// only for this user on Unix.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// Creates (or empties) the file at `path`, for this user only on Unix
/// from the start, not after a moment readable by others.
pub fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// Creates the file at `path` as [`private_file`] does, but only if
/// nothing is there yet, not even a link: it never writes through or over
/// what another put there.
pub fn new_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_whole_files_for_this_user_only() {
        let dir = std::env::temp_dir().join(format!("tern-paths-{}", std::process::id()));
        let path = dir.join("inner").join("vault.json");
        write_atomically(&path, b"{}").unwrap();
        write_atomically(&path, b"{\"groups\":[]}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"groups\":[]}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(&dir.join("inner")), 0o700);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_at_once_never_share_a_temporary_file() {
        let dir = std::env::temp_dir().join(format!("tern-paths-at-once-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("vault.json");
        let start = std::sync::Arc::new(std::sync::Barrier::new(16));
        let writers: Vec<_> = (0..16u8)
            .map(|n| {
                let (start, path) = (start.clone(), path.clone());
                std::thread::spawn(move || {
                    start.wait();
                    write_atomically(&path, &[b'a' + n; 1 << 20])
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap().unwrap();
        }
        // One of them, whole, and nothing left over.
        let written = std::fs::read(&path).unwrap();
        assert_eq!(written.len(), 1 << 20);
        assert!(written.iter().all(|&byte| byte == written[0]));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tells_network_paths_from_files_here() {
        for network in [
            r"\\server\share\id_ed25519",
            "//server/share/id_ed25519",
            r"\/server\share",
            r"\\?\UNC\server\share\key",
            r"\\.\pipe\agent",
        ] {
            assert!(network_path(Path::new(network)), "{network}");
        }
        for here in [
            r"C:\Users\me\.ssh\id_ed25519",
            "/home/me/.ssh/id_ed25519",
            r"\Users\me\key",
            "~/.ssh/key",
            "key",
        ] {
            assert!(!network_path(Path::new(here)), "{here}");
        }
    }
}
