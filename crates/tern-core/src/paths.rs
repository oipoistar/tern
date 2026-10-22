//! Where Tern keeps its own files.

use std::path::PathBuf;

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

/// Host keys the user accepted in Tern. Kept apart from `~/.ssh/known_hosts`,
/// which Tern reads but never writes.
pub fn known_hosts() -> PathBuf {
    data_dir().join("known_hosts")
}

/// OpenSSH's per-user known hosts file. `TERN_SYSTEM_KNOWN_HOSTS` overrides
/// it so tests never depend on the developer's own file.
pub fn system_known_hosts() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("TERN_SYSTEM_KNOWN_HOSTS") {
        return Some(path.into());
    }
    std::env::home_dir().map(|home| home.join(".ssh").join("known_hosts"))
}
