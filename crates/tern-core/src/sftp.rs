//! Files over a session's connection, with SFTP: listing folders on the host
//! and on this computer, copying files either way with progress, and
//! renaming, deleting, making folders and changing permissions. A copy
//! never replaces a file; it takes the next free name instead.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use russh::{ChannelMsg, client};
use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::ssh::spawn;

/// Bytes moved per read and write.
const CHUNK: usize = 32 * 1024;
/// Folders on the host whose items are counted per listing, at most.
const COUNT_AT_MOST: usize = 64;

/// An SFTP session on a connection; clones share it.
#[derive(Clone)]
pub struct Sftp(Arc<SftpSession>);

/// One thing in a folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
    /// Bytes, for a file.
    pub size: u64,
    /// How many things a folder holds, when they were counted.
    pub items: Option<usize>,
    /// Permission bits (`0o755`), where the side reports them.
    pub mode: Option<u32>,
}

/// How far a copy has got. Clones share it, so the copy updates it and a
/// view reads it.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    done: Arc<AtomicU64>,
    total: Arc<AtomicU64>,
}

impl Progress {
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Done as a share of the total, 0 to 1; an empty file is done at once.
    pub fn fraction(&self) -> f32 {
        match self.total() {
            0 => 1.,
            total => (self.done() as f64 / total as f64).min(1.) as f32,
        }
    }

    fn start(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
        self.done.store(0, Ordering::Relaxed);
    }

    fn add(&self, bytes: usize) {
        self.done.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

/// Opens an SFTP session on `handle`'s connection, in a channel of its own.
pub(crate) async fn open<H: client::Handler>(handle: &client::Handle<H>) -> Result<Sftp, String> {
    let mut channel = handle
        .channel_open_session()
        .await
        .map_err(|error| error.to_string())?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|error| error.to_string())?;
    loop {
        match channel.wait().await {
            Some(ChannelMsg::Success) => break,
            Some(ChannelMsg::Failure) | Some(ChannelMsg::Close) | None => {
                return Err("the host doesn't offer SFTP".into());
            }
            Some(_) => {}
        }
    }
    let session = SftpSession::new(channel.into_stream())
        .await
        .map_err(|error| error.to_string())?;
    Ok(Sftp(Arc::new(session)))
}

impl Sftp {
    /// The folder the server starts in, as an absolute path.
    pub fn home(&self) -> impl Future<Output = Result<String, String>> + 'static {
        let sftp = self.0.clone();
        spawn(async move {
            sftp.canonicalize(".")
                .await
                .map_err(|error| error.to_string())
        })
    }

    /// What `dir` holds, folders first, with the items of the first
    /// folders counted.
    pub fn list(&self, dir: &str) -> impl Future<Output = Result<Vec<Entry>, String>> + 'static {
        let (sftp, dir) = (self.0.clone(), dir.to_string());
        spawn(async move {
            let read = sftp
                .read_dir(&dir)
                .await
                .map_err(|error| error.to_string())?;
            let mut entries: Vec<Entry> = read
                .map(|entry| {
                    let metadata = entry.metadata();
                    Entry {
                        name: entry.file_name(),
                        dir: metadata.is_dir(),
                        size: metadata.len(),
                        items: None,
                        mode: metadata.permissions.map(|mode| mode & 0o7777),
                    }
                })
                .filter(|entry| entry.name != "." && entry.name != "..")
                .collect();
            sort(&mut entries);
            let mut counts = tokio::task::JoinSet::new();
            for (index, entry) in entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.dir)
                .take(COUNT_AT_MOST)
            {
                let (sftp, path) = (sftp.clone(), join(&dir, &entry.name));
                counts.spawn(async move {
                    let items = sftp.read_dir(path).await.ok().map(|read| {
                        read.filter(|entry| {
                            let name = entry.file_name();
                            name != "." && name != ".."
                        })
                        .count()
                    });
                    (index, items)
                });
            }
            while let Some(Ok((index, items))) = counts.join_next().await {
                entries[index].items = items;
            }
            Ok(entries)
        })
    }

    /// Renames (or moves) `from` to `to` on the host. Refused if `to`
    /// exists, so nothing is replaced.
    pub fn rename(
        &self,
        from: &str,
        to: &str,
    ) -> impl Future<Output = Result<(), String>> + 'static {
        let (sftp, from, to) = (self.0.clone(), from.to_string(), to.to_string());
        spawn(async move {
            if sftp.try_exists(to.clone()).await.unwrap_or(false) {
                return Err(format!("{} already exists", name_of(&to)));
            }
            sftp.rename(from, to)
                .await
                .map_err(|error| error.to_string())
        })
    }

    /// Makes the folder `path` on the host.
    pub fn mkdir(&self, path: &str) -> impl Future<Output = Result<(), String>> + 'static {
        let (sftp, path) = (self.0.clone(), path.to_string());
        spawn(async move {
            if sftp.try_exists(path.clone()).await.unwrap_or(false) {
                return Err(format!("{} already exists", name_of(&path)));
            }
            sftp.create_dir(path)
                .await
                .map_err(|error| error.to_string())
        })
    }

    /// Deletes the file `path` on the host, or the folder with everything
    /// in it.
    pub fn remove(
        &self,
        path: &str,
        dir: bool,
    ) -> impl Future<Output = Result<(), String>> + 'static {
        let (sftp, path) = (self.0.clone(), path.to_string());
        spawn(async move { remove_remote(&sftp, path, dir).await })
    }

    /// Sets the permission bits of `path` on the host.
    pub fn set_mode(
        &self,
        path: &str,
        mode: u32,
    ) -> impl Future<Output = Result<(), String>> + 'static {
        let (sftp, path) = (self.0.clone(), path.to_string());
        spawn(async move {
            let attributes = russh_sftp::protocol::FileAttributes {
                permissions: Some(mode & 0o7777),
                ..Default::default()
            };
            sftp.set_metadata(path, attributes)
                .await
                .map_err(|error| error.to_string())
        })
    }

    /// Copies the file at `local` into the host's folder `remote_dir`,
    /// under its own name or the next free one. Returns where it went.
    pub fn upload(
        &self,
        local: PathBuf,
        remote_dir: &str,
        progress: Progress,
    ) -> impl Future<Output = Result<String, String>> + 'static {
        let (sftp, remote_dir) = (self.0.clone(), remote_dir.to_string());
        spawn(async move {
            let failed = |error: std::io::Error| error.to_string();
            let mut source = tokio::fs::File::open(&local).await.map_err(failed)?;
            progress.start(source.metadata().await.map_err(failed)?.len());
            let name = local
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or("that isn't a file")?;
            let mut remote = join(&remote_dir, &name);
            for n in 2.. {
                if !sftp.try_exists(&remote).await.unwrap_or(false) {
                    break;
                }
                remote = join(&remote_dir, &numbered(&name, n));
            }
            let mut target = sftp
                .create(&remote)
                .await
                .map_err(|error| error.to_string())?;
            let mut buffer = vec![0u8; CHUNK];
            loop {
                let read = source.read(&mut buffer).await.map_err(failed)?;
                if read == 0 {
                    break;
                }
                target.write_all(&buffer[..read]).await.map_err(failed)?;
                progress.add(read);
            }
            target.shutdown().await.map_err(failed)?;
            Ok(remote)
        })
    }

    /// Copies the host's file `remote` into the folder `local_dir` here,
    /// under its own name or the next free one. Returns where it went.
    pub fn download(
        &self,
        remote: &str,
        local_dir: PathBuf,
        progress: Progress,
    ) -> impl Future<Output = Result<PathBuf, String>> + 'static {
        let (sftp, remote) = (self.0.clone(), remote.to_string());
        spawn(async move {
            let failed = |error: std::io::Error| error.to_string();
            let mut source = sftp
                .open(&remote)
                .await
                .map_err(|error| error.to_string())?;
            let size = source.metadata().await.map(|m| m.len()).unwrap_or(0);
            progress.start(size);
            let name = remote.rsplit('/').next().unwrap_or(&remote).to_string();
            let local = free_local_name(&local_dir, &name);
            let mut target = tokio::fs::File::create_new(&local).await.map_err(failed)?;
            let mut buffer = vec![0u8; CHUNK];
            loop {
                let read = source.read(&mut buffer).await.map_err(failed)?;
                if read == 0 {
                    break;
                }
                target.write_all(&buffer[..read]).await.map_err(failed)?;
                progress.add(read);
            }
            target.flush().await.map_err(failed)?;
            Ok(local)
        })
    }
}

/// Deletes a folder's contents depth first, then the folder.
async fn remove_remote(sftp: &SftpSession, path: String, dir: bool) -> Result<(), String> {
    let failed = |error: russh_sftp::client::error::Error| error.to_string();
    if !dir {
        return sftp.remove_file(path).await.map_err(failed);
    }
    // Iteratively, folders still to empty on a stack, so deep trees don't
    // need recursion in an async fn.
    let mut pending = vec![(path, false)];
    while let Some((folder, emptied)) = pending.pop() {
        if emptied {
            sftp.remove_dir(folder).await.map_err(failed)?;
            continue;
        }
        pending.push((folder.clone(), true));
        for entry in sftp.read_dir(folder.clone()).await.map_err(failed)? {
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let child = join(&folder, &name);
            if entry.metadata().is_dir() {
                pending.push((child, false));
            } else {
                sftp.remove_file(child).await.map_err(failed)?;
            }
        }
    }
    Ok(())
}

/// The last part of an SFTP path.
fn name_of(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

/// Renames `from` to `to` on this computer; refused if `to` exists.
pub fn rename_local(from: &Path, to: &Path) -> Result<(), String> {
    if to.exists() {
        let name = to.file_name().unwrap_or_default().to_string_lossy();
        return Err(format!("{name} already exists"));
    }
    std::fs::rename(from, to).map_err(|error| error.to_string())
}

/// Makes the folder `path` on this computer.
pub fn mkdir_local(path: &Path) -> Result<(), String> {
    std::fs::create_dir(path).map_err(|error| error.to_string())
}

/// Deletes the file `path` on this computer, or the folder with everything
/// in it.
pub fn remove_local(path: &Path, dir: bool) -> Result<(), String> {
    if dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
    .map_err(|error| error.to_string())
}

/// A file or folder name as typed for renaming or a new folder: trimmed,
/// and refused if empty, `.` or `..`, or holding a path separator.
pub fn check_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("A name is required".into());
    }
    if name == "." || name == ".." {
        return Err(format!("{name} can't be a name"));
    }
    if name.contains(['/', '\\']) {
        return Err("A name can't contain / or \\".into());
    }
    Ok(name)
}

/// Permission bits as `ls` shows them: `rwxr-xr-x`.
pub fn mode_text(mode: u32) -> String {
    let mut text = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        text.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        text.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        text.push(if bits & 0o1 != 0 { 'x' } else { '-' });
    }
    text
}

/// Permission bits typed in octal (`755`, `0644`) or as `ls` shows them
/// (`rw-r--r--`).
pub fn parse_mode(text: &str) -> Result<u32, String> {
    let text = text.trim();
    let wrong = || format!("{text} isn't a mode like 755 or rwxr-xr-x");
    if text.len() == 9 && text.chars().all(|c| "rwx-".contains(c)) {
        let mut mode = 0;
        for (at, c) in text.chars().enumerate() {
            let expected = ['r', 'w', 'x'][at % 3];
            if c == expected {
                mode |= 1 << (8 - at);
            } else if c != '-' {
                return Err(wrong());
            }
        }
        return Ok(mode);
    }
    let digits = text.strip_prefix("0o").unwrap_or(text);
    if (3..=4).contains(&digits.len()) && digits.chars().all(|c| ('0'..='7').contains(&c)) {
        return u32::from_str_radix(digits, 8).map_err(|_| wrong());
    }
    Err(wrong())
}

/// What the folder `dir` on this computer holds, like [`Sftp::list`].
pub fn list_local(dir: &Path) -> Result<Vec<Entry>, String> {
    let read = std::fs::read_dir(dir).map_err(|error| error.to_string())?;
    let mut entries: Vec<Entry> = read
        .flatten()
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            let dir = metadata.is_dir();
            Some(Entry {
                name: entry.file_name().to_string_lossy().into_owned(),
                dir,
                size: if dir { 0 } else { metadata.len() },
                items: dir
                    .then(|| std::fs::read_dir(entry.path()).ok().map(Iterator::count))
                    .flatten(),
                mode: local_mode(&metadata),
            })
        })
        .collect();
    sort(&mut entries);
    Ok(entries)
}

#[cfg(unix)]
fn local_mode(metadata: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt as _;
    Some(metadata.permissions().mode() & 0o7777)
}

/// Windows has no permission bits to show.
#[cfg(not(unix))]
fn local_mode(_: &std::fs::Metadata) -> Option<u32> {
    None
}

/// Folders first, then by name ignoring case.
fn sort(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// `name` in the SFTP folder `dir`.
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// The folder above an SFTP path; `/` stays `/`.
pub fn parent(path: &str) -> String {
    match path.trim_end_matches('/').rsplit_once('/') {
        Some(("", _)) | None => "/".into(),
        Some((parent, _)) => parent.into(),
    }
}

/// `name` with ` (n)` before its extension: `notes (2).txt`.
fn numbered(name: &str, n: u32) -> String {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => format!("{stem} ({n}).{extension}"),
        _ => format!("{name} ({n})"),
    }
}

/// `dir/name`, or the first `name (n)` there that doesn't exist yet.
fn free_local_name(dir: &Path, name: &str) -> PathBuf {
    std::iter::once(name.to_string())
        .chain((2..).map(|n| numbered(name, n)))
        .map(|candidate| dir.join(candidate))
        .find(|path| !path.exists())
        .expect("a free name")
}

/// A size in its largest whole unit: `512 B`, `12 KB`, `3.4 MB`.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.;
    let mut unit = 0;
    while value >= 1024. && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    if value < 10. {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_names_and_reads_modes() {
        assert_eq!(check_name("  notes.txt "), Ok("notes.txt"));
        assert!(check_name("").is_err());
        assert!(check_name("..").is_err());
        assert!(check_name("a/b").unwrap_err().contains('/'));
        assert!(check_name("a\\b").is_err());

        assert_eq!(mode_text(0o755), "rwxr-xr-x");
        assert_eq!(mode_text(0o640), "rw-r-----");
        assert_eq!(parse_mode("755"), Ok(0o755));
        assert_eq!(parse_mode("0644"), Ok(0o644));
        assert_eq!(parse_mode("rw-r-----"), Ok(0o640));
        assert_eq!(parse_mode(&mode_text(0o751)), Ok(0o751));
        assert!(parse_mode("999").is_err());
        assert!(parse_mode("rwxrwxrwz").is_err());
        assert!(parse_mode("xw-r--r--").is_err());
    }

    #[test]
    fn joins_and_climbs_sftp_paths() {
        assert_eq!(join("/", "etc"), "/etc");
        assert_eq!(join("/home/me", "notes.txt"), "/home/me/notes.txt");
        assert_eq!(parent("/home/me/"), "/home");
        assert_eq!(parent("/home"), "/");
        assert_eq!(parent("/"), "/");
    }

    #[test]
    fn numbers_names_and_sizes_files() {
        assert_eq!(numbered("notes.txt", 2), "notes (2).txt");
        assert_eq!(numbered("Makefile", 3), "Makefile (3)");
        assert_eq!(numbered(".bashrc", 2), ".bashrc (2)");
        assert_eq!(size(512), "512 B");
        assert_eq!(size(12 * 1024 + 100), "12 KB");
        assert_eq!(size(3_565_158), "3.4 MB");
    }

    #[test]
    fn lists_local_folders_first_with_counts() {
        let dir = std::env::temp_dir().join(format!("tern-sftp-local-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Beta")).unwrap();
        std::fs::write(dir.join("Beta").join("one"), "1").unwrap();
        std::fs::write(dir.join("alpha.txt"), "hello").unwrap();
        let entries = list_local(&dir).unwrap();
        assert_eq!(
            entries,
            [
                Entry {
                    name: "Beta".into(),
                    dir: true,
                    size: 0,
                    items: Some(1),
                    mode: entries[0].mode,
                },
                Entry {
                    name: "alpha.txt".into(),
                    dir: false,
                    size: 5,
                    items: None,
                    mode: entries[1].mode,
                },
            ]
        );
        // A free name never replaces a file.
        assert_eq!(
            free_local_name(&dir, "alpha.txt"),
            dir.join("alpha (2).txt")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
