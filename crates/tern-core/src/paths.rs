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
/// On Unix, files and folders made are the user's alone (0600 and 0700):
/// a vault names hosts, users and commands.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        private_dir(dir)?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let mut file = private_file(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temp, path)
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
}
