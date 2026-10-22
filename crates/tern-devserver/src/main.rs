//! A local SSH server with a real shell, for trying Tern end to end without a
//! remote machine: `cargo run -p tern-devserver`.
//!
//! It listens on 127.0.0.1 only and accepts one user, `tern`, with either the
//! generated test key or the generated password. Keys and password live in
//! `dev/test-ssh/` (git-ignored) and are created on first run.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use rand::RngExt as _;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{Auth, Handle, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use tokio::net::TcpListener;

const USER: &str = "tern";
const PORT: u16 = 2222;

#[tokio::main]
async fn main() -> Result<()> {
    let dir = PathBuf::from("dev/test-ssh");
    std::fs::create_dir_all(&dir)?;
    let host_key = load_or_create_key(&dir.join("host_ed25519"))?;
    let client_key = load_or_create_key(&dir.join("id_ed25519"))?;
    let password = load_or_create_password(&dir.join("password"))?;

    println!("Tern dev SSH server on 127.0.0.1:{PORT}, user `{USER}`.");
    println!(
        "  key:      ssh -p {PORT} -i {} {USER}@127.0.0.1",
        dir.join("id_ed25519").display()
    );
    println!("  password: in {}", dir.join("password").display());
    println!(
        "Host key {}",
        host_key.public_key().fingerprint(Default::default())
    );

    let config = Arc::new(russh::server::Config {
        keys: vec![host_key],
        auth_rejection_time: std::time::Duration::from_millis(300),
        auth_rejection_time_initial: Some(std::time::Duration::ZERO),
        ..Default::default()
    });
    let listener = TcpListener::bind(("127.0.0.1", PORT)).await?;
    let server = DevServer {
        client_key: client_key.public_key().clone(),
        password: Arc::new(password),
    };
    server.clone().run_on_socket(config, &listener).await?;
    Ok(())
}

fn load_or_create_key(path: &Path) -> Result<PrivateKey> {
    if path.exists() {
        return russh::keys::load_secret_key(path, None)
            .with_context(|| format!("can't read {}", path.display()));
    }
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    std::fs::write(path, key.to_openssh(LineEnding::LF)?.as_bytes())?;
    owner_only(path)?;
    std::fs::write(path.with_extension("pub"), key.public_key().to_openssh()?)?;
    Ok(key)
}

/// OpenSSH refuses private keys that other accounts can read.
fn owner_only(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    {
        let user = std::env::var("USERNAME").context("USERNAME isn't set")?;
        let status = std::process::Command::new("icacls")
            .arg(path)
            .args(["/inheritance:r", "/grant:r", &format!("{user}:F")])
            .stdout(std::process::Stdio::null())
            .status()?;
        anyhow::ensure!(
            status.success(),
            "icacls couldn't restrict {}",
            path.display()
        );
    }
    Ok(())
}

fn load_or_create_password(path: &Path) -> Result<String> {
    if let Ok(password) = std::fs::read_to_string(path) {
        return Ok(password.trim().to_string());
    }
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    let password: String = (0..20)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect();
    std::fs::write(path, &password)?;
    Ok(password)
}

#[derive(Clone)]
struct DevServer {
    client_key: PublicKey,
    password: Arc<String>,
}

impl russh::server::Server for DevServer {
    type Handler = Connection;
    fn new_client(&mut self, peer: Option<std::net::SocketAddr>) -> Connection {
        println!("connection from {peer:?}");
        Connection {
            server: self.clone(),
            size: PtySize::default(),
            pty: Arc::new(Mutex::new(None)),
        }
    }
}

/// The shell behind one SSH channel.
struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
}

struct Connection {
    server: DevServer,
    size: PtySize,
    pty: Arc<Mutex<Option<Pty>>>,
}

impl russh::server::Handler for Connection {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(
            if user == USER && password == self.server.password.as_str() {
                Auth::Accept
            } else {
                reject()
            },
        )
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        Ok(if user == USER && key == &self.server.client_key {
            Auth::Accept
        } else {
            reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        _: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        cols: u32,
        rows: u32,
        _: u32,
        _: u32,
        _: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.size = PtySize {
            cols: cols as u16,
            rows: rows as u16,
            ..Default::default()
        };
        session.channel_success(channel)?;
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match spawn_shell(self.size, channel, session.handle(), self.pty.clone()) {
            Ok(pty) => {
                *self.pty.lock().unwrap() = Some(pty);
                session.channel_success(channel)?;
            }
            Err(error) => {
                eprintln!("can't start a shell: {error:#}");
                session.channel_failure(channel)?;
            }
        }
        Ok(())
    }

    async fn data(
        &mut self,
        _: ChannelId,
        data: &[u8],
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(pty) = self.pty.lock().unwrap().as_mut() {
            let _ = pty.writer.write_all(data);
        }
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        _: ChannelId,
        cols: u32,
        rows: u32,
        _: u32,
        _: u32,
        _: &mut Session,
    ) -> Result<(), Self::Error> {
        self.size = PtySize {
            cols: cols as u16,
            rows: rows as u16,
            ..Default::default()
        };
        if let Some(pty) = self.pty.lock().unwrap().as_ref() {
            let _ = pty.master.resize(self.size);
        }
        Ok(())
    }
}

fn reject() -> Auth {
    Auth::Reject {
        proceed_with_methods: None,
        partial_success: false,
    }
}

/// Starts the platform's shell on a PTY and pumps its output to the channel
/// until it exits.
fn spawn_shell(
    size: PtySize,
    channel: ChannelId,
    handle: Handle,
    slot: Arc<Mutex<Option<Pty>>>,
) -> Result<Pty> {
    let pair = native_pty_system().openpty(size)?;
    let shell = if cfg!(windows) {
        "powershell.exe".to_string()
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    };
    let mut command = CommandBuilder::new(shell);
    command.env("TERM", "xterm-256color");
    if let Some(home) = std::env::home_dir() {
        command.cwd(home);
    }
    let mut child = pair.slave.spawn_command(command)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0
                || runtime
                    .block_on(handle.data(channel, buf[..n].to_vec()))
                    .is_err()
            {
                break;
            }
        }
        let status = child.wait().map(|status| status.exit_code()).unwrap_or(1);
        slot.lock().unwrap().take();
        runtime.block_on(async {
            let _ = handle.exit_status_request(channel, status).await;
            let _ = handle.eof(channel).await;
            let _ = handle.close(channel).await;
        });
    });
    Ok(Pty {
        master: pair.master,
        writer,
    })
}
