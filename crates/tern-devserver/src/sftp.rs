//! A small SFTP server over one folder of this computer, for the dev server
//! (rooted at your home) and the end-to-end tests (rooted at a scratch
//! folder). SFTP paths are `/`-separated from that root and can't leave it.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::PathBuf;

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

    fn local(&self, path: &str) -> PathBuf {
        Self::parts(path)
            .into_iter()
            .fold(self.root.clone(), |local, part| local.join(part))
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
        let metadata = std::fs::metadata(self.local(&path)).map_err(code)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        self.stat(id, path).await
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        match self.open.get(&handle) {
            Some(Opened::File(file)) => Ok(Attrs {
                id,
                attrs: FileAttributes::from(&file.metadata().map_err(code)?),
            }),
            _ => Err(StatusCode::Failure),
        }
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(self.local(&path)).map_err(code)? {
            let entry = entry.map_err(code)?;
            if let Ok(metadata) = entry.metadata() {
                let name = entry.file_name().to_string_lossy().into_owned();
                files.push(File::new(name, FileAttributes::from(&metadata)));
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
            .open(self.local(&filename))
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
        std::fs::create_dir(self.local(&path)).map_err(code)?;
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        std::fs::remove_dir(self.local(&path)).map_err(code)?;
        Ok(ok(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        std::fs::remove_file(self.local(&filename)).map_err(code)?;
        Ok(ok(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        std::fs::rename(self.local(&oldpath), self.local(&newpath)).map_err(code)?;
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
            std::path::Path::new("/srv/root").join("a").join("c")
        );
        assert_eq!(
            sftp.local("../../etc/passwd"),
            std::path::Path::new("/srv/root").join("etc").join("passwd")
        );
        assert_eq!(FsSftp::parts("/"), Vec::<&str>::new());
    }
}
