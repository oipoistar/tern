//! What the end-to-end tests share: a russh server running in this
//! process (password, key and certificate sign-in, an echoing shell,
//! direct-tcpip as a jump host, remote forwarding, SFTP over a folder, and
//! a host certificate) and helpers for reading a session.

// Each test file uses its own share of this.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::ssh_key::certificate::CertType;
use russh::keys::{Certificate, HashAlg, PrivateKey, PublicKey};
use russh::server::{Auth, Msg, Server as _, Session as ServerSession};
use russh::{Channel, ChannelId};
use tern_core::ssh::{ConnectRequest, Credential, Event, Session, Target, TermSize, Trusted};
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
    /// Accepts user certificates this authority signed.
    user_ca: Option<PublicKey>,
    /// Variables the client asked for, said in the shell's first output.
    env: Vec<(String, String)>,
    /// Signs in only by questions: the password, then this code.
    otp: Option<String>,
    /// How far through the questions this connection is.
    asked: u8,
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
            user_ca: self.user_ca.clone(),
            env: Vec::new(),
            otp: self.otp.clone(),
            asked: 0,
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
        // Asking questions instead, the way two-factor servers do.
        if self.otp.is_some() {
            return Ok(reject());
        }
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

    /// With an `otp`: "Password:", then "Verification code:".
    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        user: &str,
        _: &str,
        response: Option<russh::server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        let Some(otp) = self.otp.clone() else {
            return Ok(reject());
        };
        let answers: Vec<String> = response
            .map(|answers| {
                answers
                    .map(|answer| String::from_utf8_lossy(&answer).into_owned())
                    .collect()
            })
            .unwrap_or_default();
        Ok(match (self.asked, answers.as_slice()) {
            (0, _) => {
                self.asked = 1;
                Auth::Partial {
                    name: "".into(),
                    instructions: "".into(),
                    prompts: vec![("Password: ".into(), false)].into(),
                }
            }
            (1, [password]) if password == PASSWORD => {
                self.asked = 2;
                Auth::Partial {
                    name: "".into(),
                    instructions: "Use the code from your phone.".into(),
                    prompts: vec![("Verification code: ".into(), true)].into(),
                }
            }
            (2, [code]) if user == USER && *code == otp => Auth::Accept,
            _ => {
                self.asked = 0;
                reject()
            }
        })
    }

    async fn auth_openssh_certificate(
        &mut self,
        user: &str,
        certificate: &Certificate,
    ) -> Result<Auth, Self::Error> {
        let vouched = self.user_ca.as_ref().is_some_and(|ca| {
            certificate
                .validate([&ca.fingerprint(HashAlg::Sha256)])
                .is_ok()
                && certificate.cert_type() == CertType::User
                && certificate
                    .valid_principals()
                    .iter()
                    .any(|name| name == user)
        });
        Ok(if user == USER && vouched {
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
        for (name, value) in &self.env {
            session.data(channel, format!("env:{name}={value}\r\n").into_bytes())?;
        }
        session.data(channel, b"ready\r\n".to_vec())?;
        Ok(())
    }

    async fn env_request(
        &mut self,
        _: ChannelId,
        name: &str,
        value: &str,
        _: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        self.env.push((name.into(), value.into()));
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
    start_server_with(
        host_key,
        client_key,
        Setup {
            probe_agent: true,
            ..Setup::default()
        },
    )
    .await
}

/// Like [`start_server`], also serving SFTP over `sftp_root`.
pub async fn start_server_with_sftp(
    host_key: PrivateKey,
    client_key: PublicKey,
    sftp_root: Option<PathBuf>,
) -> u16 {
    start_server_with(
        host_key,
        client_key,
        Setup {
            sftp_root,
            ..Setup::default()
        },
    )
    .await
}

/// What a test server does beyond the basics.
#[derive(Default)]
pub struct Setup {
    pub sftp_root: Option<PathBuf>,
    pub probe_agent: bool,
    /// A host certificate, presented to clients that ask for one.
    pub certificate: Option<Certificate>,
    /// Sign-in with user certificates this authority signed.
    pub user_ca: Option<PublicKey>,
    /// Sign-in by questions only (no plain passwords): the password, then
    /// this one-time code.
    pub otp: Option<String>,
}

/// Like [`start_server`], doing what `setup` adds.
pub async fn start_server_with(host_key: PrivateKey, client_key: PublicKey, setup: Setup) -> u16 {
    let Setup {
        sftp_root,
        probe_agent,
        certificate,
        user_ca,
        otp,
    } = setup;
    let methods = if otp.is_some() {
        use russh::MethodKind;
        russh::MethodSet::from(&[MethodKind::PublicKey, MethodKind::KeyboardInteractive][..])
    } else {
        russh::MethodSet::server_supported()
    };
    let config = Arc::new(russh::server::Config {
        methods,
        keys: vec![host_key],
        certificates: certificate.into_iter().collect(),
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
            user_ca,
            env: Vec::new(),
            otp,
            asked: 0,
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
        proxy: None,
        size: TermSize { cols: 80, rows: 24 },
        // The test server's key, for the server, which jump hosts here are
        // too.
        trust: trust.map(|key| Trusted {
            host: "127.0.0.1".into(),
            port,
            key,
        }),
        keep_alive: true,
        forwards: Vec::new(),
        agent_forwarding: false,
        env: Vec::new(),
        shell: true,
    }
}

pub async fn next(session: &mut Session) -> Event {
    next_within(session, Duration::from_secs(10)).await
}

/// The next event, waiting up to `wait` for it.
pub async fn next_within(session: &mut Session, wait: Duration) -> Event {
    tokio::time::timeout(wait, session.next_event())
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
