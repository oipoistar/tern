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

/// The saved hosts.
pub fn vault() -> PathBuf {
    data_dir().join("vault.json")
}

/// Recent connections.
pub fn history() -> PathBuf {
    data_dir().join("history.json")
}

/// Key pairs Tern generated or imported.
pub fn keys() -> PathBuf {
    data_dir().join("keys")
}

/// Host keys the user accepted in Tern. Kept apart from `~/.ssh/known_hosts`,
/// which Tern reads but never writes.
pub fn known_hosts() -> PathBuf {
    data_dir().join("known_hosts")
}

/// OpenSSH's per-user folder, `~/.ssh`, whose keys the Keychain lists.
/// `TERN_SSH_DIR` overrides it so checks never show the developer's keys.
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

/// Writes `bytes` to `path` through a temporary file renamed over the old
/// one, so a crash mid-write never leaves half a file. Creates the folder.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut file = std::fs::File::create(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temp, path)
}
