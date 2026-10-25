//! Host files edited in an editor here: a copy in a folder of its own,
//! watched, and written back over the host's file each time it's saved;
//! if the host's file changed meanwhile, the user says first.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::sftp::Stamp;

/// A host's file being edited here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    /// The host's file.
    pub remote: String,
    /// The copy here.
    pub local: PathBuf,
    /// The host's file when the copies last matched.
    pub theirs: Stamp,
    /// The copy here then.
    pub ours: Stamp,
}

impl Edit {
    /// Whether the copy here changed since the copies last matched: it was
    /// saved.
    pub fn changed_here(&self, now: &Stamp) -> bool {
        *now != self.ours
    }

    /// Whether the host's file changed since then too, so writing over it
    /// would lose that change.
    pub fn changed_there(&self, now: &Stamp) -> bool {
        *now != self.theirs
    }
}

/// This computer's file `path` as it is now. Its time is in nanoseconds,
/// as precise as the file system keeps it.
pub fn stamp(path: &Path) -> std::io::Result<Stamp> {
    let metadata = std::fs::metadata(path)?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_nanos() as u64);
    Ok(Stamp {
        size: metadata.len(),
        modified,
    })
}

/// A new folder in `base` for one edited copy, so copies of files with the
/// same name never meet. It's this user's alone: a shared temporary folder
/// (Linux's `/tmp`) would let others read the copies.
pub fn folder(base: &Path) -> std::io::Result<PathBuf> {
    let folder = base.join(format!("tern-edit-{}", crate::host::new_id()));
    crate::paths::private_dir(&folder)?;
    Ok(folder)
}

/// Whether this computer would run a file called `name` when asked to
/// open it, rather than show it in an editor: scripts, programs, shortcuts
/// and installers on Windows, with the scripts of interpreters a developer's
/// computer has (Python's launcher runs a `.py`, Git Bash a `.sh`), and
/// what connects elsewhere (`.rdp`) or mounts a disk; Terminal's command
/// files and Python's launcher on macOS; launchers on Linux.
pub fn runs_when_opened(name: &str) -> bool {
    let Some((_, extension)) = name.rsplit_once('.') else {
        return false;
    };
    let extension = extension.to_ascii_lowercase();
    let runs: &[&str] = if cfg!(windows) {
        &[
            "ahk",
            "appinstaller",
            "application",
            "appref-ms",
            "appx",
            "appxbundle",
            "bash",
            "bat",
            "chm",
            "cmd",
            "com",
            "cpl",
            "deskthemepack",
            "diagcab",
            "exe",
            "gadget",
            "hta",
            "ica",
            "img",
            "inf",
            "iso",
            "jar",
            "js",
            "jse",
            "library-ms",
            "lnk",
            "msc",
            "msi",
            "msix",
            "msixbundle",
            "msp",
            "msu",
            "pif",
            "pl",
            "ppkg",
            "ps1",
            "py",
            "pyc",
            "pyw",
            "pyz",
            "pyzw",
            "rb",
            "rdp",
            "reg",
            "scf",
            "scr",
            "sct",
            "search-ms",
            "searchconnector-ms",
            "settingcontent-ms",
            "sh",
            "theme",
            "themepack",
            "url",
            "vbe",
            "vbs",
            "vhd",
            "vhdx",
            "vsto",
            "website",
            "wsb",
            "wsc",
            "wsf",
            "wsh",
            "xbap",
            "xll",
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "action",
            "app",
            "command",
            "fileloc",
            "inetloc",
            "jar",
            "mobileconfig",
            "mpkg",
            "pkg",
            "py",
            "terminal",
            "tool",
            "webloc",
            "workflow",
        ]
    } else {
        &["appimage", "desktop", "jar"]
    };
    runs.contains(&extension.as_str())
}

/// The copy at `local` as it's safe to open: renamed `<name>.txt` when
/// this computer would run it, so it opens in a text editor instead. Saves
/// still go back to the host's file.
pub fn openable(local: PathBuf) -> std::io::Result<PathBuf> {
    let name = local
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !runs_when_opened(&name) {
        return Ok(local);
    }
    let renamed = local.with_file_name(format!("{name}.txt"));
    std::fs::rename(&local, &renamed)?;
    Ok(renamed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_that_would_run_open_as_text() {
        let base = std::env::temp_dir();
        let folder = folder(&base).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&folder).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        let script = if cfg!(windows) {
            "deploy.CMD"
        } else if cfg!(target_os = "macos") {
            "deploy.command"
        } else {
            "deploy.desktop"
        };
        assert!(runs_when_opened(script));
        assert!(!runs_when_opened("notes.txt") && !runs_when_opened("Makefile"));
        // Python's launcher runs a script it's asked to open; on Windows Git
        // Bash runs a shell script, and Remote Desktop connects where an
        // .rdp file says.
        assert_eq!(
            runs_when_opened("deploy.PY"),
            cfg!(any(windows, target_os = "macos"))
        );
        assert_eq!(runs_when_opened("deploy.sh"), cfg!(windows));
        assert_eq!(runs_when_opened("jump.rdp"), cfg!(windows));
        let local = folder.join(script);
        std::fs::write(&local, "echo hi").unwrap();
        let opened = openable(local).unwrap();
        assert_eq!(
            opened.file_name().unwrap(),
            format!("{script}.txt").as_str()
        );
        assert_eq!(std::fs::read_to_string(&opened).unwrap(), "echo hi");
        let plain = folder.join("notes.md");
        std::fs::write(&plain, "x").unwrap();
        assert_eq!(openable(plain.clone()).unwrap(), plain);
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_save_here_goes_back_unless_the_host_changed_too() {
        let at = |size, modified| Stamp {
            size,
            modified: Some(modified),
        };
        let edit = Edit {
            remote: "/etc/motd".into(),
            local: PathBuf::from("motd"),
            theirs: at(10, 100),
            ours: at(10, 5_000),
        };
        assert!(!edit.changed_here(&at(10, 5_000)));
        assert!(edit.changed_here(&at(10, 6_000)));
        assert!(edit.changed_here(&at(11, 5_000)));
        assert!(!edit.changed_there(&at(10, 100)));
        assert!(edit.changed_there(&at(10, 101)));
        assert!(edit.changed_there(&at(12, 100)));
    }

    #[test]
    fn copies_get_folders_of_their_own_and_saves_show() {
        let base = std::env::temp_dir().join(format!("tern-edits-test-{}", std::process::id()));
        let (one, two) = (folder(&base).unwrap(), folder(&base).unwrap());
        assert_ne!(one, two);
        assert!(one.is_dir() && two.is_dir());
        let copy = one.join("motd");
        std::fs::write(&copy, "hello").unwrap();
        let before = stamp(&copy).unwrap();
        assert_eq!(before.size, 5);
        std::fs::write(&copy, "hello, world").unwrap();
        assert_ne!(stamp(&copy).unwrap(), before);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
