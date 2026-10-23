//! Files over a session's connection, with SFTP: listing folders on the host
//! and on this computer, copying files either way with progress, and
//! renaming, deleting, making folders and changing permissions. A copy
//! never replaces a file; it takes the next free name instead. A name that
//! comes from the other side has to be one name where it lands, never a
//! path out of the folder.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use russh::{ChannelMsg, client};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::ssh::spawn;

/// Bytes moved per read and write.
const CHUNK: usize = 32 * 1024;
/// Folders on the host whose items are counted per listing, at most.
const COUNT_AT_MOST: usize = 64;
/// Where permission bits keep a file's type (`S_IFMT`).
const TYPE: u32 = 0o170000;
/// A folder's type there (`S_IFDIR`).
const FOLDER: u32 = 0o040000;

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

/// A file as it was at a moment: its size and when it last changed (in
/// whatever unit its side keeps), to tell whether it changed since.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub size: u64,
    pub modified: Option<u64>,
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
                        dir: is_folder(metadata.permissions),
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
    /// exists, so nothing is replaced, unless `to` is `from` itself in
    /// another case, on a host whose folders ignore case.
    pub fn rename(
        &self,
        from: &str,
        to: &str,
    ) -> impl Future<Output = Result<(), String>> + 'static {
        let (sftp, from, to) = (self.0.clone(), from.to_string(), to.to_string());
        spawn(async move {
            let failed = |error: russh_sftp::client::error::Error| error.to_string();
            if from == to {
                return Ok(());
            }
            if !sftp.try_exists(to.clone()).await.unwrap_or(false) {
                return sftp.rename(from, to).await.map_err(failed);
            }
            if !is_case_change(&sftp, &from, &to).await {
                return Err(format!("{} already exists", name_of(&to)));
            }
            // OpenSSH won't rename onto a name that exists, and `to` does:
            // it's `from`. So the name changes by way of a free one.
            let aside = free_remote_name(&sftp, &parent(&from), name_of(&from)).await;
            sftp.rename(from.clone(), aside.clone())
                .await
                .map_err(failed)?;
            if let Err(error) = sftp.rename(aside.clone(), to).await {
                let _ = sftp.rename(aside, from).await;
                return Err(failed(error));
            }
            Ok(())
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
            let name = local
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or("that isn't a file")?;
            // A `\` is legal in a name here on Unix, but a host on Windows
            // would read it as a folder.
            one_name(&name, &['/', '\\'])?;
            let mut source = tokio::fs::File::open(&local).await.map_err(failed)?;
            progress.start(source.metadata().await.map_err(failed)?.len());
            let remote = free_remote_name(&sftp, &remote_dir, &name).await;
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

    /// The host's small file `path` as text, at most a mebibyte of it: for
    /// files like `/proc`'s, whose size says nothing.
    pub fn read_text(&self, path: &str) -> impl Future<Output = Result<String, String>> + 'static {
        let (sftp, path) = (self.0.clone(), path.to_string());
        spawn(async move {
            let mut file = sftp.open(path).await.map_err(|error| error.to_string())?;
            let mut text = Vec::new();
            (&mut file)
                .take(1 << 20)
                .read_to_end(&mut text)
                .await
                .map_err(|error| error.to_string())?;
            Ok(String::from_utf8_lossy(&text).into_owned())
        })
    }

    /// The share of the file system holding `path` in use, 0 to 1, when
    /// the server says (OpenSSH's statvfs extension).
    pub fn disk_use(
        &self,
        path: &str,
    ) -> impl Future<Output = Result<Option<f32>, String>> + 'static {
        let (sftp, path) = (self.0.clone(), path.to_string());
        spawn(async move {
            let info = sftp
                .fs_info(path)
                .await
                .map_err(|error| error.to_string())?;
            Ok(info.and_then(|info| {
                crate::probe::disk(info.blocks, info.blocks_free, info.blocks_avail)
            }))
        })
    }

    /// The host's file `path` as it is now; its time is in seconds.
    pub fn stamp(&self, path: &str) -> impl Future<Output = Result<Stamp, String>> + 'static {
        let (sftp, path) = (self.0.clone(), path.to_string());
        spawn(async move {
            let metadata = sftp
                .metadata(path)
                .await
                .map_err(|error| error.to_string())?;
            Ok(Stamp {
                size: metadata.len(),
                modified: metadata.mtime.map(u64::from),
            })
        })
    }

    /// Writes this computer's file `local` over the host's file `remote`,
    /// keeping its permissions: how an edited copy goes back. Returns the
    /// host's file as it is afterwards.
    pub fn replace(
        &self,
        local: PathBuf,
        remote: &str,
        progress: Progress,
    ) -> impl Future<Output = Result<Stamp, String>> + 'static {
        let (sftp, remote) = (self.0.clone(), remote.to_string());
        spawn(async move {
            let failed = |error: std::io::Error| error.to_string();
            let mut source = tokio::fs::File::open(&local).await.map_err(failed)?;
            progress.start(source.metadata().await.map_err(failed)?.len());
            let flags = OpenFlags::WRITE | OpenFlags::TRUNCATE | OpenFlags::CREATE;
            let mut target = sftp
                .open_with_flags(&remote, flags)
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
            let metadata = sftp
                .metadata(remote)
                .await
                .map_err(|error| error.to_string())?;
            Ok(Stamp {
                size: metadata.len(),
                modified: metadata.mtime.map(u64::from),
            })
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
            // The host names the file, so the name could be a path here
            // (`..\Startup\run.bat`, `c:run.bat` on Windows).
            let name = remote.rsplit('/').next().unwrap_or(&remote).to_string();
            local_name(&name)?;
            let mut source = sftp
                .open(&remote)
                .await
                .map_err(|error| error.to_string())?;
            let size = source.metadata().await.map(|m| m.len()).unwrap_or(0);
            progress.start(size);
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
            // The folder itself, its parent, and a name with a `/` that
            // would be somewhere else: none of them are this folder's.
            if name == "." || name == ".." || name.contains('/') {
                continue;
            }
            let child = join(&folder, &name);
            if is_folder(entry.metadata().permissions) {
                pending.push((child, false));
            } else {
                sftp.remove_file(child).await.map_err(failed)?;
            }
        }
    }
    Ok(())
}

/// Whether permission bits from the host are a folder's. The whole type is
/// compared: a socket's and a block device's include a folder's bit.
fn is_folder(mode: Option<u32>) -> bool {
    mode.is_some_and(|mode| mode & TYPE == FOLDER)
}

/// The first of `dir/name`, `dir/name (2)`… on the host that doesn't exist.
async fn free_remote_name(sftp: &SftpSession, dir: &str, name: &str) -> String {
    let mut path = join(dir, name);
    for n in 2.. {
        if !sftp.try_exists(path.clone()).await.unwrap_or(false) {
            break;
        }
        path = join(dir, &numbered(name, n));
    }
    path
}

/// Whether `to`, which exists on the host, is `from` in another case; see
/// [`only_case_differs`].
async fn is_case_change(sftp: &SftpSession, from: &str, to: &str) -> bool {
    let folder = parent(to);
    if parent(from) != folder {
        return false;
    }
    let Ok(listing) = sftp.read_dir(folder).await else {
        return false;
    };
    let names: Vec<String> = listing.map(|entry| entry.file_name()).collect();
    only_case_differs(name_of(from), name_of(to), names.iter().map(String::as_str))
}

/// Whether renaming `from` to `to`, a name that exists in the same folder,
/// changes only its case: the two differ only in case, and nothing else in
/// the folder (`names`) goes by `to`, so the folder ignores case and `to`
/// is `from` itself.
fn only_case_differs<'a>(from: &str, to: &str, names: impl IntoIterator<Item = &'a str>) -> bool {
    let folded = to.to_lowercase();
    from.to_lowercase() == folded
        && names
            .into_iter()
            .all(|name| name == from || name.to_lowercase() != folded)
}

/// The last part of an SFTP path.
fn name_of(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

/// Renames `from` to `to` on this computer. Refused if `to` exists, so
/// nothing is replaced, unless `to` is `from` itself in another case, in a
/// folder that ignores case; and refused if `to` doesn't end in a name this
/// computer keeps in one folder.
pub fn rename_local(from: &Path, to: &Path) -> Result<(), String> {
    local_target(to)?;
    if from == to {
        return Ok(());
    }
    if to.symlink_metadata().is_ok() && !is_local_case_change(from, to) {
        let name = to.file_name().unwrap_or_default().to_string_lossy();
        return Err(format!("{name} already exists"));
    }
    std::fs::rename(from, to).map_err(|error| error.to_string())
}

/// Whether `to`, which exists on this computer, is `from` in another case;
/// see [`only_case_differs`].
fn is_local_case_change(from: &Path, to: &Path) -> bool {
    let (Some(folder), Some(from_name), Some(to_name)) = (
        to.parent(),
        from.file_name().and_then(|name| name.to_str()),
        to.file_name().and_then(|name| name.to_str()),
    ) else {
        return false;
    };
    if from.parent() != Some(folder) {
        return false;
    }
    let Ok(listing) = std::fs::read_dir(folder) else {
        return false;
    };
    let names: Vec<String> = listing
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    only_case_differs(from_name, to_name, names.iter().map(String::as_str))
}

/// Makes the folder `path` on this computer; refused if `path` doesn't end
/// in a name this computer keeps in one folder.
pub fn mkdir_local(path: &Path) -> Result<(), String> {
    local_target(path)?;
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
/// and refused if empty, `.` or `..`, or holding a path separator. It suits
/// either side; this computer has rules of its own, which
/// [`rename_local`] and [`mkdir_local`] keep.
pub fn check_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    one_name(name, &['/', '\\'])?;
    Ok(name)
}

/// Refuses an empty name, `.`, `..`, and a name holding one of
/// `separators`.
fn one_name(name: &str, separators: &[char]) -> Result<(), String> {
    if name.is_empty() {
        return Err("A name is required".into());
    }
    if name == "." || name == ".." {
        return Err(format!("{name} can't be a name"));
    }
    if name.contains(separators) {
        return Err("A name can't contain / or \\".into());
    }
    Ok(())
}

/// Refuses a name this computer wouldn't keep as one entry of a folder: on
/// Windows, a `\` and what [`windows_name`] refuses too.
fn local_name(name: &str) -> Result<(), String> {
    if cfg!(windows) {
        one_name(name, &['/', '\\'])?;
        windows_name(name)
    } else {
        one_name(name, &['/'])
    }
}

/// Refuses what Windows forbids in a name, which a path there reads as
/// something else: a drive (`d:notes`), a file's hidden stream
/// (`notes:old`), a device (`CON`, `nul.txt`), wildcards, control
/// characters, and a name ending in a dot or a space, which it drops.
fn windows_name(name: &str) -> Result<(), String> {
    if let Some(c) = name.chars().find(|c| r#"<>:"|?*"#.contains(*c)) {
        return Err(format!("A name on Windows can't contain {c}"));
    }
    if name.chars().any(|c| c < ' ') {
        return Err("A name on Windows can't contain control characters".into());
    }
    if name.ends_with(['.', ' ']) {
        return Err("A name on Windows can't end in a dot or a space".into());
    }
    // Before any extension, and ignoring spaces at its end.
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    let numbered_device = ["COM", "LPT"].into_iter().any(|device| {
        let mut number = stem.strip_prefix(device).unwrap_or_default().chars();
        matches!(
            (number.next(), number.next()),
            (Some('0'..='9' | '¹' | '²' | '³'), None)
        )
    });
    if numbered_device || ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str()) {
        return Err(format!("{name} is reserved on Windows"));
    }
    Ok(())
}

/// Refuses a path on this computer that doesn't end in a name it keeps in
/// one folder ([`local_name`]). A name with a drive replaces the folder
/// it's joined to (`d:notes` lands in drive D's current folder), so a path
/// that has a drive but no root is all name.
fn local_target(path: &Path) -> Result<(), String> {
    let drive_only =
        matches!(path.components().next(), Some(Component::Prefix(_))) && !path.has_root();
    let name = if drive_only {
        path.as_os_str()
    } else {
        path.components()
            .next_back()
            .map(Component::as_os_str)
            .unwrap_or_default()
    };
    local_name(&name.to_string_lossy())
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
        // A host on Unix keeps any other character.
        assert_eq!(check_name("10:30 <draft>?"), Ok("10:30 <draft>?"));

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
    fn keeps_names_from_the_other_side_in_their_folder() {
        assert_eq!(local_name("notes.txt"), Ok(()));
        assert!(local_name("").is_err());
        assert!(local_name("..").unwrap_err().contains(".."));
        // Windows reads both slashes as folders and `:` as a drive.
        for name in [r"..\..\Startup\run.bat", r"C:\run.bat", "c:run.bat"] {
            assert_eq!(local_name(name).is_err(), cfg!(windows), "{name}");
        }

        // Windows' own rules, on every system.
        assert_eq!(windows_name("notes (2).txt"), Ok(()));
        assert!(windows_name("d:notes").unwrap_err().contains(':'));
        assert!(windows_name("notes.txt:old").unwrap_err().contains(':'));
        for name in ["a<b", "a>b", "a\"b", "a|b", "a?b", "a*b"] {
            assert!(windows_name(name).is_err(), "{name}");
        }
        assert!(windows_name("tab\there").unwrap_err().contains("control"));
        assert!(windows_name("notes.").unwrap_err().contains("end in"));
        assert!(windows_name("notes ").unwrap_err().contains("end in"));
        for name in [
            "CON",
            "nul.txt",
            "Aux.tar.gz",
            "com1",
            "LPT9.log",
            "COM¹",
            "prn .txt",
        ] {
            assert!(
                windows_name(name).unwrap_err().contains("reserved"),
                "{name}"
            );
        }
        for name in ["CONSOLE", "com10", "lpt", "nul-device.txt", "notes.con"] {
            assert_eq!(windows_name(name), Ok(()), "{name}");
        }
    }

    #[test]
    fn keeps_typed_names_in_their_folder() {
        let folder = std::env::temp_dir().join("tern-names");
        assert_eq!(local_target(&folder.join("notes")), Ok(()));
        assert!(local_target(&folder.join("..")).is_err());
        // Joined to a folder, a name with a drive replaces it on Windows.
        let drive = folder.join("d:notes");
        assert_eq!(local_target(&drive).is_err(), cfg!(windows), "{drive:?}");
        let stream = folder.join("notes.txt:old");
        assert_eq!(local_target(&stream).is_err(), cfg!(windows), "{stream:?}");
    }

    #[test]
    fn tells_folders_by_their_whole_type() {
        assert!(is_folder(Some(0o040755)));
        assert!(!is_folder(Some(0o140755)), "a socket");
        assert!(!is_folder(Some(0o060660)), "a block device");
        assert!(!is_folder(Some(0o100644)));
        assert!(!is_folder(Some(0o120777)), "a link");
        assert!(!is_folder(Some(0o755)), "no type at all");
        assert!(!is_folder(None));
    }

    #[test]
    fn tells_a_change_of_case_from_another_file() {
        // A folder that ignores case holds the one file under either name.
        assert!(only_case_differs(
            "readme.md",
            "README.md",
            ["readme.md", "notes.txt"]
        ));
        assert!(only_case_differs("Ünïcode", "üNÏCODE", ["Ünïcode"]));
        // One that minds case can hold both, and the other one stays.
        assert!(!only_case_differs(
            "readme.md",
            "README.md",
            ["readme.md", "README.md"]
        ));
        assert!(!only_case_differs(
            "readme.md",
            "Readme.md",
            ["readme.md", "README.md"]
        ));
        assert!(!only_case_differs(
            "notes.txt",
            "README.md",
            ["notes.txt", "readme.md"]
        ));
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
