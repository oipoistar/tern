//! What the end-to-end tests share: a russh server running in this
//! process (password and key sign-in, an echoing shell, direct-tcpip as a
//! jump host, and remote forwarding) and helpers for reading a session.

// Each test file uses its own share of this.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use russh::keys::{PrivateKey, PublicKey};
use russh::server::{Auth, Msg, Server as _, Session as ServerSession};
use russh::{Channel, ChannelId};
use tern_core::ssh::{ConnectRequest, Credential, Event, Session, Target, TermSize};
use tokio::net::TcpListener;

pub const USER: &str = "tern";
pub const PASSWORD: &str = "correct horse battery";

#[derive(Clone)]
pub struct TestServer {
    client_key: PublicKey,
    /// This connection's shell channels. Only these echo; a forwarded
    /// channel carries another SSH connection and must pass through intact.
    shells: Vec<ChannelId>,
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
        _: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
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

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
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
        jump: None,
        size: TermSize { cols: 80, rows: 24 },
        trust,
        keep_alive: true,
        forwards: Vec::new(),
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
