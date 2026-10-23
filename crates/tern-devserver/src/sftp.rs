//! A small SFTP server over one folder of this computer, for the dev server
//! (rooted at your home) and the end-to-end tests (rooted at a scratch
//! folder). SFTP paths are `/`-separated from that root and can't leave it.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Component, Path, PathBuf};

use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};

pub struct FsSftp {
    root: PathBuf,
    open: HashMap<String, Opened>,
    next: u64,
}

enum Opened {
    File(std::fs::File),
    /// A folder's entries, and whether they've been sent.
    Dir(Vec<File>, bool),
}

impl FsSftp {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            open: HashMap::new(),
            next: 0,
        }
    }

    /// The folder parts of an SFTP path, with `.` and `..` resolved inside
    /// the root.
    fn parts(path: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        parts
    }

    /// Where an SFTP path is in the root. A part this computer would read
    /// as more than a name isn't there: on Windows a `\` parts folders and
    /// a drive (`C:`) replaces the root.
    fn local(&self, path: &str) -> Result<PathBuf, StatusCode> {
        Self::parts(path)
            .into_iter()
            .try_fold(self.root.clone(), |local, part| {
                let mut parts = Path::new(part).components();
                let name = matches!(
                    (parts.next(), parts.next()),
                    (Some(Component::Normal(_)), None)
                ) && !(cfg!(windows) && part.contains(':'));
                if name {
                    Ok(local.join(part))
                } else {
                    Err(StatusCode::NoSuchFile)
                }
            })
    }

    fn remember(&mut self, opened: Opened) -> String {
        self.next += 1;
        let handle = self.next.to_string();
        self.open.insert(handle.clone(), opened);
        handle
    }
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".into(),
        language_tag: "en-US".into(),
    }
}

fn code(error: std::io::Error) -> StatusCode {
    match error.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

/// What SFTP says about a file. On Unix that's its whole mode, type and
/// all, as OpenSSH sends it; russh-sftp's conversion knows only folders and
/// files, so a socket would pass for a file.
fn attributes(metadata: &std::fs::Metadata) -> FileAttributes {
    let converted = FileAttributes::from(metadata);
    FileAttributes {
        permissions: unix_mode(metadata).or(converted.permissions),
        ..converted
    }
}

#[cfg(unix)]
fn unix_mode(metadata: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    Some(metadata.mode())
}

#[cfg(not(unix))]
fn unix_mode(_: &std::fs::Metadata) -> Option<u32> {
    None
}

impl russh_sftp::server::Handler for FsSftp {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(&mut self, _: u32, _: HashMap<String, String>) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let path = format!("/{}", Self::parts(&path).join("/"));
        Ok(Name {
            id,
            files: vec![File::dummy(path)],
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let metadata = std::fs::metadata(self.local(&path)?).map_err(code)?;
        Ok(Attrs {
            id,
            attrs: attributes(&metadata),
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.stat(id, path).await
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        match self.open.get(&handle) {
            Some(Opened::File(file)) => Ok(Attrs {
                id,
                attrs: attributes(&file.metadata().map_err(code)?),
            }),
            _ => Err(StatusCode::Failure),
        }
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(self.local(&path)?).map_err(code)? {
            let entry = entry.map_err(code)?;
            if let Ok(metadata) = entry.metadata() {
                let name = entry.file_name().to_string_lossy().into_owned();
                files.push(File::new(name, attributes(&metadata)));
            }
        }
        Ok(Handle {
            id,
            handle: self.remember(Opened::Dir(files, false)),
        })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        match self.open.get_mut(&handle) {
            Some(Opened::Dir(files, sent)) if !*sent => {
                *sent = true;
                Ok(Name {
                    id,
                    files: std::mem::take(files),
                })
            }
            _ => Err(StatusCode::Eof),
        }
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        flags: OpenFlags,
        _: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let file = std::fs::OpenOptions::from(flags)
            .open(self.local(&filename)?)
            .map_err(code)?;
        Ok(Handle {
            id,
            handle: self.remember(Opened::File(file)),
        })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let Some(Opened::File(file)) = self.open.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        file.seek(SeekFrom::Start(offset)).map_err(code)?;
        let mut data = vec![0u8; len as usize];
        let read = file.read(&mut data).map_err(code)?;
        if read == 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(read);
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let Some(Opened::File(file)) = self.open.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        file.seek(SeekFrom::Start(offset)).map_err(code)?;
        file.write_all(&data).map_err(code)?;
        Ok(ok(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        self.open.remove(&handle);
        Ok(ok(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _: FileAttributes,
    ) -> Result<Status, Self::Error> {
        std::fs::create_dir(self.local(&path)?).map_err(code)?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        std::fs::remove_dir(self.local(&path)?).map_err(code)?;
        Ok(ok(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        std::fs::remove_file(self.local(&filename)?).map_err(code)?;
        Ok(ok(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        std::fs::rename(self.local(&oldpath)?, self.local(&newpath)?).map_err(code)?;
        Ok(ok(id))
    }

    /// Only permissions change. Windows has none but read-only, which
    /// follows the owner's write bit.
    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        if let Some(mode) = attrs.permissions {
            let local = self.local(&path)?;
            let mut permissions = std::fs::metadata(&local).map_err(code)?.permissions();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                permissions.set_mode(mode & 0o7777);
            }
            #[cfg(not(unix))]
            permissions.set_readonly(mode & 0o200 == 0);
            std::fs::set_permissions(&local, permissions).map_err(code)?;
        }
        Ok(ok(id))
    }
}

#[cfg(test)]
mod tests {
    use super::FsSftp;

    #[test]
    fn paths_stay_inside_the_root() {
        let sftp = FsSftp::new("/srv/root".into());
        assert_eq!(
            sftp.local("/a/./b/../c"),
            Ok(std::path::Path::new("/srv/root").join("a").join("c"))
        );
        assert_eq!(
            sftp.local("../../etc/passwd"),
            Ok(std::path::Path::new("/srv/root").join("etc").join("passwd"))
        );
        assert_eq!(FsSftp::parts("/"), Vec::<&str>::new());
        // On Windows, `\` and a drive would lead out; on Unix they're names.
        for path in [r"/..\..\etc", "/c:/Windows", r"/a/C:\Windows"] {
            assert_eq!(sftp.local(path).is_err(), cfg!(windows), "{path}");
        }
    }
}
