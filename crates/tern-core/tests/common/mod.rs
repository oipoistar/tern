//! What the end-to-end tests share: a russh server running in this
//! process (password and key sign-in, an echoing shell, direct-tcpip as a
//! jump host, remote forwarding, and SFTP over a folder) and helpers for
//! reading a session.

// Each test file uses its own share of this.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::{PrivateKey, PublicKey};
use russh::server::{Auth, Msg, Server as _, Session as ServerSession};
use russh::{Channel, ChannelId};
use tern_core::ssh::{ConnectRequest, Credential, Event, Session, Target, TermSize};
use tern_devserver::sftp::FsSftp;
use tokio::net::TcpListener;

pub const USER: &str = "tern";
pub const PASSWORD: &str = "correct horse battery";

pub struct TestServer {
    client_key: PublicKey,
    /// This connection's shell channels. Only these echo; a forwarded
    /// channel carries another SSH connection and must pass through intact.
    shells: Vec<ChannelId>,
    /// Where SFTP is served from, if it is.
    sftp_root: Option<PathBuf>,
    /// Session channels until they ask for a subsystem; shells drop theirs.
    channels: HashMap<ChannelId, Channel<Msg>>,
    /// Try the client's agent on every shell, offered or not, and say how
    /// it went in the shell's output.
    probe_agent: bool,
    agent_offered: bool,
}

impl Clone for TestServer {
    fn clone(&self) -> Self {
        Self {
            client_key: self.client_key.clone(),
            shells: self.shells.clone(),
            sftp_root: self.sftp_root.clone(),
            channels: HashMap::new(),
            probe_agent: self.probe_agent,
            agent_offered: false,
        }
    }
}

impl russh::server::Server for TestServer {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        self.clone()
    }
}

impl russh::server::Handler for TestServer {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == USER && password == PASSWORD {
            Auth::Accept
        } else {
            reject()
        })
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        Ok(if user == USER && key == &self.client_key {
            Auth::Accept
        } else {
            reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        match (self.channels.remove(&channel), &self.sftp_root) {
            (Some(opened), Some(root)) if name == "sftp" => {
                session.channel_success(channel)?;
                russh_sftp::server::run(opened.into_stream(), FsSftp::new(root.clone())).await;
            }
            _ => session.channel_failure(channel)?,
        }
        Ok(())
    }

    /// Acts as a jump host: forwards the channel to the requested address.
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _: &str,
        _: u32,
        reply: russh::server::ChannelOpenHandle,
        _: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        // Dropping `reply` without accepting refuses the channel.
        if let Ok(mut tcp) = tokio::net::TcpStream::connect((host, port as u16)).await {
            reply.accept().await;
            tokio::spawn(async move {
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut stream, &mut tcp).await;
            });
        }
        Ok(())
    }

    /// The client offers its agent: list what it holds, as `agent:<the
    /// first key's comment>` in the shell's output.
    async fn agent_request(
        &mut self,
        channel: ChannelId,
        session: &mut ServerSession,
    ) -> Result<bool, Self::Error> {
        self.agent_offered = true;
        let handle = session.handle();
        tokio::spawn(async move {
            let said = match handle.channel_open_agent().await {
                Ok(agent) => {
                    let mut client =
                        russh::keys::agent::client::AgentClient::connect(agent.into_stream());
                    match client.request_identities().await {
                        Ok(identities) => match identities.first() {
                            Some(russh::keys::agent::AgentIdentity::PublicKey {
                                comment, ..
                            }) => {
                                format!("agent:{comment}")
                            }
                            _ => "agent:empty".to_string(),
                        },
                        Err(_) => "agent:unreadable".to_string(),
                    }
                }
                Err(_) => "agent:refused".to_string(),
            };
            let _ = handle.data(channel, said.into_bytes()).await;
        });
        Ok(true)
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        self.channels.remove(&channel);
        // A server mustn't reach an agent it wasn't offered.
        if self.probe_agent && !self.agent_offered {
            let handle = session.handle();
            tokio::spawn(async move {
                let said = match handle.channel_open_agent().await {
                    Ok(_) => "agent:opened",
                    Err(_) => "agent:refused",
                };
                let _ = handle.data(channel, said.as_bytes().to_vec()).await;
            });
        }
        self.shells.push(channel);
        session.channel_success(channel)?;
        session.data(channel, b"ready\r\n".to_vec())?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        if !self.shells.contains(&channel) {
            return Ok(());
        }
        if data == b"exit" {
            session.exit_status_request(channel, 7)?;
            session.close(channel)?;
        } else {
            session.data(channel, [b"echo:", data].concat())?;
        }
        Ok(())
    }

    /// Listens where the client asks and opens a forwarded-tcpip channel
    /// back to it for each connection, as `sshd` does for `ssh -R`.
    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        session: &mut ServerSession,
    ) -> Result<bool, Self::Error> {
        let Ok(listener) = TcpListener::bind((address, *port as u16)).await else {
            return Ok(false);
        };
        *port = u32::from(listener.local_addr().unwrap().port());
        let (handle, address, bound) = (session.handle(), address.to_string(), *port);
        tokio::spawn(async move {
            while let Ok((mut tcp, peer)) = listener.accept().await {
                let (handle, address) = (handle.clone(), address.clone());
                tokio::spawn(async move {
                    let opened = handle
                        .channel_open_forwarded_tcpip(
                            address,
                            bound,
                            peer.ip().to_string(),
                            u32::from(peer.port()),
                        )
                        .await;
                    if let Ok(channel) = opened {
                        let mut stream = channel.into_stream();
                        let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
                    }
                });
            }
        });
        Ok(true)
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        cols: u32,
        rows: u32,
        _: u32,
        _: u32,
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        session.data(channel, format!("size:{cols}x{rows}").into_bytes())?;
        Ok(())
    }
}

fn reject() -> Auth {
    Auth::Reject {
        proceed_with_methods: None,
        partial_success: false,
    }
}

/// Starts the server on a free local port and returns that port.
pub async fn start_server(host_key: PrivateKey, client_key: PublicKey) -> u16 {
    start_server_with_sftp(host_key, client_key, None).await
}

/// Like [`start_server`], also trying each client's agent (see
/// `TestServer::probe_agent`).
pub async fn start_server_probing_agent(host_key: PrivateKey, client_key: PublicKey) -> u16 {
    serve(host_key, client_key, None, true).await
}

/// Like [`start_server`], also serving SFTP over `sftp_root`.
pub async fn start_server_with_sftp(
    host_key: PrivateKey,
    client_key: PublicKey,
    sftp_root: Option<PathBuf>,
) -> u16 {
    serve(host_key, client_key, sftp_root, false).await
}

async fn serve(
    host_key: PrivateKey,
    client_key: PublicKey,
    sftp_root: Option<PathBuf>,
    probe_agent: bool,
) -> u16 {
    let config = Arc::new(russh::server::Config {
        keys: vec![host_key],
        auth_rejection_time: Duration::from_millis(10),
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Default::default()
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = TestServer {
            client_key,
            shells: Vec::new(),
            sftp_root,
            channels: HashMap::new(),
            probe_agent,
            agent_offered: false,
        }
        .run_on_socket(config, &listener)
        .await;
    });
    port
}

pub fn request(
    port: u16,
    credentials: Vec<Credential>,
    trust: Option<PublicKey>,
) -> ConnectRequest {
    ConnectRequest {
        target: Target {
            host: "127.0.0.1".into(),
            port,
            user: USER.into(),
        },
        credentials,
        jumps: Vec::new(),
        size: TermSize { cols: 80, rows: 24 },
        trust,
        keep_alive: true,
        forwards: Vec::new(),
        agent_forwarding: false,
    }
}

pub async fn next(session: &mut Session) -> Event {
    tokio::time::timeout(Duration::from_secs(10), session.next_event())
        .await
        .expect("timed out waiting for the session")
        .expect("session ended without an event")
}

/// Collects output until `needle` shows up.
pub async fn expect_output(session: &mut Session, needle: &str) {
    let mut seen = String::new();
    while !seen.contains(needle) {
        match next(session).await {
            Event::Output(bytes) => seen.push_str(&String::from_utf8_lossy(&bytes)),
            other => panic!("expected output containing {needle:?}, got {other:?} after {seen:?}"),
        }
    }
}

/// Waits for the first event `wanted` picks, skipping the rest (shell
/// output, other rules).
pub async fn wait_for<T>(session: &mut Session, mut wanted: impl FnMut(Event) -> Option<T>) -> T {
    loop {
        if let Some(found) = wanted(next(session).await) {
            return found;
        }
    }
}

/// Points Tern's files at a scratch folder for this test binary. Each
/// binary runs one test, so the process-wide environment isn't shared.
pub fn scratch(name: &str) -> std::path::PathBuf {
    let scratch = std::env::temp_dir().join(format!("tern-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    // SAFETY: called first in the binary's only test, before any session
    // (and so any other thread reading these) exists.
    unsafe {
        std::env::set_var("TERN_DATA_DIR", scratch.join("data"));
        std::env::set_var("TERN_SYSTEM_KNOWN_HOSTS", scratch.join("no-such-file"));
    }
    scratch
}
