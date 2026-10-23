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
/// same name never meet.
pub fn folder(base: &Path) -> std::io::Result<PathBuf> {
    let folder = base.join("tern-edits").join(crate::host::new_id());
    std::fs::create_dir_all(&folder)?;
    Ok(folder)
}

#[cfg(test)]
mod tests {
    use super::*;

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
