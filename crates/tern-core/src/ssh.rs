//! Interactive SSH sessions over russh.
//!
//! Sessions run on Tern's own Tokio runtime. Callers talk to them through
//! channels, so any executor (GPUI's included) can drive a session without
//! knowing about Tokio.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{
    Algorithm, HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate, known_hosts,
};
use russh::{ChannelMsg, ChannelStream, client};
use tokio::io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::forward::{Forward, ForwardState, Forwarder, RemoteTargets, serve_remote};
use crate::host::Auth;
use crate::paths;
use crate::randomart::Randomart;
use crate::sftp::{self, Sftp};

/// A server's host key, as offered for the user to trust.
pub type HostKey = PublicKey;

/// Terminal type announced to the server.
const TERM: &str = "xterm-256color";
/// How often an idle connection is pinged when keep-alive is on.
const KEEP_ALIVE: Duration = Duration::from_secs(30);
/// How long a reachability check waits for each step.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Who to connect to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub user: String,
}

/// One way to prove who we are, tried in the order given.
#[derive(Clone, Debug)]
pub enum Credential {
    /// Keys held by the running SSH agent (OpenSSH agent, Pageant).
    Agent,
    KeyFile {
        path: PathBuf,
        passphrase: Option<String>,
    },
    Password(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermSize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Clone, Debug)]
pub struct ConnectRequest {
    pub target: Target,
    pub credentials: Vec<Credential>,
    /// Sign in to each of these in turn, first hop first, and reach the
    /// target through them (`ssh -J a,b`).
    pub jumps: Vec<(Target, Vec<Credential>)>,
    pub size: TermSize,
    /// A host key the user has just accepted, for the target or the jump
    /// host. It's saved to Tern's known hosts once that host is verified.
    pub trust: Option<PublicKey>,
    /// Ping idle connections so routers and firewalls keep them open.
    pub keep_alive: bool,
    /// Port forwarding rules to start once signed in.
    pub forwards: Vec<Forward>,
    /// Let the server use this computer's SSH agent.
    pub agent_forwarding: bool,
}

/// A host to go through, and how to sign in there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Jump {
    pub target: Target,
    pub auth: Auth,
}

/// Everything needed to open a session: from a saved host
/// (`Vault::connection`) or a typed command ([`Connection::typed`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    /// The saved host this opens, if it is one.
    pub host: Option<String>,
    /// That host's id, which its saved password is kept under.
    pub host_id: Option<String>,
    pub target: Target,
    pub auth: Auth,
    /// The hosts to go through, first hop first.
    pub jumps: Vec<Jump>,
    pub keep_alive: bool,
    /// Let the server use this computer's SSH agent.
    pub agent_forwarding: bool,
    /// Typed into the shell once it starts.
    pub on_login: Option<String>,
    /// Port forwarding rules switched on for the host.
    pub forwards: Vec<Forward>,
}

impl Connection {
    /// A destination typed or passed on the command line, with defaults.
    pub fn typed(target: Target, identity: Option<PathBuf>) -> Self {
        Self {
            host: None,
            host_id: None,
            target,
            auth: identity.map_or(Auth::Agent, Auth::Key),
            jumps: Vec::new(),
            keep_alive: true,
            agent_forwarding: false,
            on_login: None,
            forwards: Vec::new(),
        }
    }

    /// The request that opens this connection. `password` is one the user
    /// typed for the target; jump hosts never get a password.
    pub fn request(
        &self,
        size: TermSize,
        password: Option<&str>,
        trust: Option<HostKey>,
    ) -> ConnectRequest {
        ConnectRequest {
            target: self.target.clone(),
            credentials: credentials_for(&self.auth, password),
            jumps: self
                .jumps
                .iter()
                .map(|jump| (jump.target.clone(), credentials_for(&jump.auth, None)))
                .collect(),
            size,
            trust,
            keep_alive: self.keep_alive,
            forwards: self.forwards.clone(),
            agent_forwarding: self.agent_forwarding,
        }
    }
}

/// What to try, in order, to sign in with `auth`. A key file comes first;
/// a typed password replaces the agent and default keys, which already
/// failed by the time one is asked for. Password sign-in with no password
/// yet tries nothing, so the session asks for one.
pub fn credentials_for(auth: &Auth, password: Option<&str>) -> Vec<Credential> {
    let mut credentials = Vec::new();
    if let Auth::Key(path) = auth {
        credentials.push(Credential::KeyFile {
            path: path.clone(),
            passphrase: None,
        });
    }
    match (auth, password) {
        (_, Some(password)) => credentials.push(Credential::Password(password.into())),
        (Auth::Password, None) => {}
        _ => credentials.extend(default_credentials()),
    }
    credentials
}

/// Why a connection didn't get to a shell.
#[derive(Clone, Debug)]
pub enum ConnectError {
    /// The key of `host` (the target or a jump host) is in no known_hosts
    /// file. Ask the user, then retry with [`ConnectRequest::trust`] set to
    /// `key`.
    UnknownHostKey {
        host: String,
        port: u16,
        key: Box<PublicKey>,
        fingerprint: String,
    },
    /// The server's key differs from the recorded one: possibly an attack.
    HostKeyChanged {
        fingerprint: String,
        file: PathBuf,
        line: usize,
    },
    /// Every credential was rejected.
    AuthFailed {
        tried: Vec<String>,
    },
    Other(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownHostKey {
                host, fingerprint, ..
            } => write!(f, "unknown host key {fingerprint} for {host}"),
            Self::HostKeyChanged {
                fingerprint,
                file,
                line,
            } => write!(
                f,
                "host key changed to {fingerprint}; it doesn't match {}:{line}",
                file.display()
            ),
            Self::AuthFailed { tried } if tried.is_empty() => write!(f, "no credentials to try"),
            Self::AuthFailed { tried } => {
                write!(f, "authentication failed (tried {})", tried.join(", "))
            }
            Self::Other(message) => f.write_str(message),
        }
    }
}

/// What a session reports back.
#[derive(Debug)]
pub enum Event {
    /// Authenticated, shell requested.
    Connected,
    /// Bytes for the terminal (stdout and stderr interleaved, as a PTY does).
    Output(Vec<u8>),
    /// The shell ended or the connection dropped.
    Closed {
        exit_status: Option<u32>,
    },
    Failed(ConnectError),
    /// A port forwarding rule started, stopped or couldn't start.
    Forwarding {
        forward: Forward,
        state: ForwardState,
    },
}

enum Input {
    Data(Vec<u8>),
    Resize(TermSize),
    /// Start (`true`) or stop a port forwarding rule.
    Forward(Forward, bool),
    /// Open an SFTP session on the connection.
    OpenSftp(oneshot::Sender<Result<Sftp, String>>),
    Close,
}

/// A running (or starting) session. Dropping it closes the connection.
pub struct Session {
    input: SessionInput,
    events: mpsc::UnboundedReceiver<Event>,
}

/// The sending side of a session, for UIs that read events elsewhere. The
/// connection closes once every clone is dropped.
#[derive(Clone)]
pub struct SessionInput(mpsc::UnboundedSender<Input>);

impl SessionInput {
    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        let _ = self.0.send(Input::Data(bytes.into()));
    }

    pub fn resize(&self, size: TermSize) {
        let _ = self.0.send(Input::Resize(size));
    }

    /// Starts or stops a port forwarding rule; an
    /// [`Event::Forwarding`] says how it went.
    pub fn forward(&self, forward: Forward, on: bool) {
        let _ = self.0.send(Input::Forward(forward, on));
    }

    pub fn close(&self) {
        let _ = self.0.send(Input::Close);
    }

    /// An SFTP session on this session's connection, once it's signed in.
    pub fn open_sftp(&self) -> impl Future<Output = Result<Sftp, String>> + 'static {
        let (reply, opened) = oneshot::channel();
        let _ = self.0.send(Input::OpenSftp(reply));
        async move {
            opened
                .await
                .unwrap_or_else(|_| Err("the session has ended".into()))
        }
    }
}

/// Events from a session, readable from any async executor.
pub struct SessionEvents(mpsc::UnboundedReceiver<Event>);

impl SessionEvents {
    pub async fn next(&mut self) -> Option<Event> {
        self.0.recv().await
    }
}

impl Session {
    /// Starts connecting in the background and returns at once; progress
    /// arrives through [`Session::next_event`].
    pub fn open(request: ConnectRequest) -> Self {
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        runtime().spawn(async move {
            if let Err(error) = run(request, input_rx, &event_tx).await {
                let _ = event_tx.send(Event::Failed(error));
            }
        });
        Self {
            input: SessionInput(input_tx),
            events: event_rx,
        }
    }

    /// Waits for the next event; `None` once the session task has finished.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        self.input.write(bytes);
    }

    pub fn resize(&self, size: TermSize) {
        self.input.resize(size);
    }

    pub fn forward(&self, forward: Forward, on: bool) {
        self.input.forward(forward, on);
    }

    pub fn close(&self) {
        self.input.close();
    }

    /// Splits the session so input and events can live in different places.
    pub fn into_parts(self) -> (SessionInput, SessionEvents) {
        (self.input, SessionEvents(self.events))
    }
}

/// Runs `work` on Tern's runtime, to be awaited from any executor.
pub(crate) fn spawn<T: Send + 'static>(
    work: impl Future<Output = Result<T, String>> + Send + 'static,
) -> impl Future<Output = Result<T, String>> + 'static {
    let (tx, rx) = oneshot::channel();
    runtime().spawn(async move {
        let _ = tx.send(work.await);
    });
    async move { rx.await.unwrap_or_else(|_| Err("the work stopped".into())) }
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("tern-ssh")
            .enable_all()
            .build()
            .expect("failed to start the SSH runtime")
    })
}

async fn run(
    request: ConnectRequest,
    mut input: mpsc::UnboundedReceiver<Input>,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<(), ConnectError> {
    let ConnectRequest {
        target,
        credentials,
        jumps,
        size,
        trust,
        keep_alive,
        forwards,
        agent_forwarding,
    } = request;
    let config = Arc::new(client::Config {
        keepalive_interval: keep_alive.then_some(KEEP_ALIVE),
        ..Default::default()
    });

    // Through jump hosts: sign in to each, then run the next one's SSH over
    // a channel it forwards, and so on to the target. Every jump
    // connection must outlive the session.
    let mut via = Vec::new();
    let mut tunnel = None;
    for (at, (jump, jump_credentials)) in jumps.iter().enumerate() {
        let mut jump_handle = open(
            config.clone(),
            jump,
            trust.clone(),
            tunnel.take(),
            RemoteTargets::default(),
            false,
        )
        .await?;
        authenticate(&mut jump_handle, &jump.user, jump_credentials)
            .await
            .map_err(|error| other(format!("can't sign in to {}: {error}", jump.host)))?;
        let next = jumps.get(at + 1).map_or(&target, |(next, _)| next);
        let channel = jump_handle
            .channel_open_direct_tcpip(next.host.clone(), next.port.into(), "127.0.0.1", 0)
            .await
            .map_err(|error| {
                other(format!(
                    "{} can't reach {}:{}: {error}",
                    jump.host, next.host, next.port
                ))
            })?;
        tunnel = Some(channel.into_stream());
        via.push(jump_handle);
    }
    // Where the host sends connections for remote forwarding rules.
    let remote = RemoteTargets::default();
    let mut handle = open(
        config,
        &target,
        trust,
        tunnel,
        remote.clone(),
        agent_forwarding,
    )
    .await?;

    authenticate(&mut handle, &target.user, &credentials).await?;
    let _ = events.send(Event::Connected);
    let handle = Arc::new(handle);
    let mut forwarder = Forwarder::new(handle.clone(), remote, events.clone());
    for forward in forwards {
        forwarder.set(forward, true).await;
    }

    let channel = handle.channel_open_session().await.map_err(other)?;
    if agent_forwarding {
        channel.agent_forward(false).await.map_err(other)?;
    }
    channel
        .request_pty(false, TERM, size.cols.into(), size.rows.into(), 0, 0, &[])
        .await
        .map_err(other)?;
    channel.request_shell(true).await.map_err(other)?;
    let (mut reader, writer) = channel.split();

    let mut exit_status = None;
    loop {
        tokio::select! {
            message = reader.wait() => match message {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    let _ = events.send(Event::Output(data.to_vec()));
                }
                Some(ChannelMsg::ExitStatus { exit_status: status }) => exit_status = Some(status),
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                Some(_) => {}
            },
            command = input.recv() => match command {
                Some(Input::Data(bytes)) => {
                    if writer.data(&bytes[..]).await.is_err() {
                        break;
                    }
                }
                Some(Input::Resize(size)) => {
                    let _ = writer.window_change(size.cols.into(), size.rows.into(), 0, 0).await;
                }
                Some(Input::Forward(forward, on)) => forwarder.set(forward, on).await,
                Some(Input::OpenSftp(reply)) => {
                    let handle = handle.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(sftp::open(&handle).await);
                    });
                }
                Some(Input::Close) | None => {
                    let _ = writer.close().await;
                    break;
                }
            },
        }
    }
    forwarder.stop_all();
    let _ = handle
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await;
    // The hop nearest the target first.
    for jump in via.iter().rev() {
        let _ = jump
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await;
    }
    let _ = events.send(Event::Closed { exit_status });
    Ok(())
}

/// Connects to `target` and verifies its host key, over TCP or, through
/// jump hosts, over `tunnel`.
async fn open(
    config: Arc<client::Config>,
    target: &Target,
    trust: Option<PublicKey>,
    tunnel: Option<ChannelStream<client::Msg>>,
    remote: RemoteTargets,
    agent_forwarding: bool,
) -> Result<client::Handle<Verifier>, ConnectError> {
    let verdict = Arc::new(Mutex::new(None));
    let handler = Verifier {
        host: target.host.clone(),
        port: target.port,
        trust,
        verdict: verdict.clone(),
        remote,
        agent_forwarding,
    };
    let connected = match tunnel {
        Some(stream) => client::connect_stream(config, stream, handler).await,
        None => client::connect(config, (target.host.as_str(), target.port), handler).await,
    };
    // A rejected host key surfaces as a generic russh error; report ours.
    connected.map_err(|error| {
        verdict
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| other(error))
    })
}

fn other(error: impl std::fmt::Display) -> ConnectError {
    ConnectError::Other(error.to_string())
}

/// Checks the server's host key against `~/.ssh/known_hosts` (read only) and
/// Tern's own known hosts, learning a key the user explicitly trusted. Also
/// answers the channels the server opens for remote forwarding rules.
struct Verifier {
    host: String,
    port: u16,
    trust: Option<PublicKey>,
    verdict: Arc<Mutex<Option<ConnectError>>>,
    remote: RemoteTargets,
    /// Agent channels from the server are answered only when this is on.
    agent_forwarding: bool,
}

impl client::Handler for Verifier {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = server else {
            *self.verdict.lock().unwrap() = Some(ConnectError::Other(
                "host certificates aren't supported yet".into(),
            ));
            return Ok(false);
        };
        match check_host_key(&self.host, self.port, key, self.trust.as_ref()) {
            Ok(()) => Ok(true),
            Err(error) => {
                *self.verdict.lock().unwrap() = Some(error);
                Ok(false)
            }
        }
    }

    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<client::Msg>,
        _: &str,
        connected_port: u32,
        _: &str,
        _: u32,
        reply: client::ChannelOpenHandle,
        _: &mut client::Session,
    ) -> Result<(), Self::Error> {
        serve_remote(&self.remote, connected_port, channel, reply);
        Ok(())
    }

    async fn server_channel_open_agent_forward(
        &mut self,
        channel: russh::Channel<client::Msg>,
        reply: client::ChannelOpenHandle,
        _: &mut client::Session,
    ) -> Result<(), Self::Error> {
        crate::agent::serve(self.agent_forwarding, channel, reply);
        Ok(())
    }
}

/// The policy behind [`Verifier`], kept separate so it can be tested.
pub fn check_host_key(
    host: &str,
    port: u16,
    key: &PublicKey,
    trust: Option<&PublicKey>,
) -> Result<(), ConnectError> {
    let ours = paths::known_hosts();
    for file in &known_hosts_files() {
        match known_hosts::check_known_hosts_path(host, port, key, file) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(russh::keys::Error::KeyChanged { line }) => {
                return Err(ConnectError::HostKeyChanged {
                    fingerprint: fingerprint(key),
                    file: file.clone(),
                    line,
                });
            }
            Err(error) => return Err(other(format!("can't read {}: {error}", file.display()))),
        }
    }
    if trust == Some(key) {
        known_hosts::learn_known_hosts_path(host, port, key, &ours).map_err(|error| {
            other(format!(
                "can't save the host key to {}: {error}",
                ours.display()
            ))
        })?;
        return Ok(());
    }
    Err(ConnectError::UnknownHostKey {
        host: host.into(),
        port,
        key: Box::new(key.clone()),
        fingerprint: fingerprint(key),
    })
}

/// The OpenSSH user's file (read only), then Tern's own.
fn known_hosts_files() -> Vec<PathBuf> {
    paths::system_known_hosts()
        .into_iter()
        .chain([paths::known_hosts()])
        .collect()
}

/// The key a known_hosts file already holds for `host:port`, if any. Reads
/// only. When several are listed, picks the type Tern asks servers for first
/// (Ed25519, then ECDSA, then RSA), which is the key a connection would see.
pub fn known_host_key(host: &str, port: u16) -> Option<HostKey> {
    known_host_key_in(&known_hosts_files(), host, port)
}

fn known_host_key_in(files: &[PathBuf], host: &str, port: u16) -> Option<HostKey> {
    files
        .iter()
        // An unreadable or malformed file only means no key to show here;
        // connecting reports the real error.
        .flat_map(|file| known_hosts::known_host_keys_path(host, port, file).unwrap_or_default())
        .map(|(_, key)| key)
        .min_by_key(|key| match key.algorithm() {
            Algorithm::Ed25519 => 0,
            Algorithm::Ecdsa { .. } => 1,
            Algorithm::Rsa { .. } => 2,
            _ => 3,
        })
}

pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// The drunken-bishop picture of a key's SHA-256 fingerprint.
pub fn key_art(key: &PublicKey) -> Randomart {
    Randomart::new(key.fingerprint(HashAlg::Sha256).as_bytes())
}

/// What a private key file holds, for showing it: its kind, fingerprint and
/// art.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInfo {
    pub kind: String,
    pub fingerprint: String,
    pub art: Randomart,
}

/// Reads the `.pub` file next to a private key, or the key itself when it
/// has no passphrase. `None` if neither works.
pub fn key_file_info(path: &Path) -> Option<KeyInfo> {
    let mut public = path.as_os_str().to_owned();
    public.push(".pub");
    let key = std::fs::read_to_string(public)
        .ok()
        .and_then(|text| PublicKey::from_openssh(text.trim()).ok())
        .or_else(|| {
            russh::keys::load_secret_key(path, None)
                .ok()
                .map(|key| key.public_key().clone())
        })?;
    Some(KeyInfo {
        kind: key_kind(&key),
        fingerprint: fingerprint(&key),
        art: key_art(&key),
    })
}

/// A key's type in a word or two: `ed25519`, `ecdsa nistp256`, `rsa`.
pub fn key_kind(key: &PublicKey) -> String {
    match key.algorithm() {
        Algorithm::Ed25519 => "ed25519".to_string(),
        Algorithm::SkEd25519 => "ed25519-sk".to_string(),
        Algorithm::Ecdsa { curve } => format!("ecdsa {curve}"),
        Algorithm::Rsa { .. } => "rsa".to_string(),
        other => other.to_string(),
    }
}

/// An SSH server that answered a reachability check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reached {
    /// The TCP handshake's round trip, rounded up.
    pub latency_ms: u32,
    /// The server's greeting, like `SSH-2.0-OpenSSH_9.6`.
    pub banner: String,
}

/// Checks that an SSH server answers at `host:port` without signing in: a
/// TCP connection, timed, then the server's greeting. Runs on Tern's
/// runtime; await it from any executor. Errors are a few lowercase words.
pub fn probe(host: &str, port: u16) -> impl Future<Output = Result<Reached, String>> + 'static {
    let host = host.to_string();
    spawn(async move { probe_now(&host, port).await })
}

async fn probe_now(host: &str, port: u16) -> Result<Reached, String> {
    let started = Instant::now();
    let connect = tokio::net::TcpStream::connect((host, port));
    let stream = tokio::time::timeout(PROBE_TIMEOUT, connect)
        .await
        .map_err(|_| "no answer".to_string())?
        .map_err(|error| describe(&error))?;
    let latency_ms = started.elapsed().as_micros().div_ceil(1000).max(1) as u32;
    // Servers may send other lines before the version line (RFC 4253 4.2).
    let mut lines = BufReader::new(stream).lines();
    for _ in 0..5 {
        let line = tokio::time::timeout(PROBE_TIMEOUT, lines.next_line())
            .await
            .map_err(|_| "no SSH greeting".to_string())?
            .map_err(|error| describe(&error))?
            .ok_or("closed without an SSH greeting")?;
        if line.starts_with("SSH-") {
            return Ok(Reached {
                latency_ms,
                banner: line.trim().into(),
            });
        }
    }
    Err("not an SSH server".into())
}

/// An I/O error in a few words, without the OS's error number.
pub(crate) fn describe(error: &std::io::Error) -> String {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::ConnectionRefused => "connection refused".into(),
        ErrorKind::TimedOut => "no answer".into(),
        ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable => "unreachable".into(),
        _ => {
            let text = error.to_string();
            let text = text.split(" (os error").next().unwrap_or(&text);
            text.trim_end_matches('.').to_lowercase()
        }
    }
}

async fn authenticate(
    handle: &mut client::Handle<Verifier>,
    user: &str,
    credentials: &[Credential],
) -> Result<(), ConnectError> {
    let mut tried = Vec::new();
    for credential in credentials {
        let outcome = match credential {
            Credential::Agent => {
                tried.push("agent".to_string());
                with_agent(handle, user).await
            }
            Credential::KeyFile { path, passphrase } => {
                tried.push(format!("key {}", path.display()));
                with_key_file(handle, user, path, passphrase.as_deref()).await
            }
            Credential::Password(password) => {
                tried.push("password".to_string());
                handle
                    .authenticate_password(user, password)
                    .await
                    .map(|result| result.success())
                    .map_err(Into::into)
            }
        };
        // A credential that can't be used (no agent, unreadable key) just
        // moves on to the next one, like OpenSSH does.
        if let Ok(true) = outcome {
            return Ok(());
        }
    }
    Err(ConnectError::AuthFailed { tried })
}

async fn with_key_file(
    handle: &mut client::Handle<Verifier>,
    user: &str,
    path: &Path,
    passphrase: Option<&str>,
) -> Result<bool> {
    let key = russh::keys::load_secret_key(path, passphrase)
        .with_context(|| format!("can't load {}", path.display()))?;
    let hash = handle.best_supported_rsa_hash().await?.flatten();
    let result = handle
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
        .await?;
    Ok(result.success())
}

async fn with_agent(handle: &mut client::Handle<Verifier>, user: &str) -> Result<bool> {
    #[cfg(unix)]
    {
        let agent = AgentClient::connect_env().await?;
        agent_auth(handle, user, agent).await
    }
    #[cfg(windows)]
    {
        let pipe =
            std::env::var("SSH_AUTH_SOCK").unwrap_or_else(|_| r"\\.\pipe\openssh-ssh-agent".into());
        if let Ok(agent) = AgentClient::connect_named_pipe(&pipe).await
            && agent_auth(handle, user, agent).await?
        {
            return Ok(true);
        }
        let agent = AgentClient::connect_pageant().await?;
        agent_auth(handle, user, agent).await
    }
}

async fn agent_auth<S>(
    handle: &mut client::Handle<Verifier>,
    user: &str,
    mut agent: AgentClient<S>,
) -> Result<bool>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let hash = handle.best_supported_rsa_hash().await?.flatten();
    for identity in agent.request_identities().await? {
        let AgentIdentity::PublicKey { key, .. } = identity else {
            continue;
        };
        if handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await?
            .success()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Default credentials when the user gave none: the agent, then OpenSSH's
/// usual key files that exist.
pub fn default_credentials() -> Vec<Credential> {
    let mut credentials = vec![Credential::Agent];
    if let Some(home) = std::env::home_dir() {
        for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
            let path = home.join(".ssh").join(name);
            if path.exists() {
                credentials.push(Credential::KeyFile {
                    path,
                    passphrase: None,
                });
            }
        }
    }
    credentials
}

/// The destination in text typed into search, if it is one: an `ssh …`
/// command or anything with `user@`. Plain words stay searches.
pub fn quick_destination(input: &str) -> Option<(Target, Option<PathBuf>)> {
    let input = input.trim();
    if !(input.starts_with("ssh ") || input.contains('@')) {
        return None;
    }
    parse_command(input).ok()
}

/// Parses a pasted `ssh` command or bare `user@host[:port]`. Understands
/// `-p port`, `-l user` and `-i keyfile`; ignores other flags.
pub fn parse_command(input: &str) -> Result<(Target, Option<PathBuf>)> {
    let mut words = input.split_whitespace().peekable();
    if words.peek() == Some(&"ssh") {
        words.next();
    }
    let (mut port, mut user, mut key, mut destination) = (None, None, None, None);
    while let Some(word) = words.next() {
        match word {
            "-p" => {
                port = Some(
                    words
                        .next()
                        .context("-p needs a port")?
                        .parse::<u16>()
                        .context("bad port")?,
                )
            }
            "-l" => user = Some(words.next().context("-l needs a user")?.to_string()),
            "-i" => key = Some(PathBuf::from(words.next().context("-i needs a key file")?)),
            // Other OpenSSH flags that take an argument: skip both.
            "-B" | "-b" | "-c" | "-D" | "-E" | "-e" | "-F" | "-I" | "-J" | "-L" | "-m" | "-O"
            | "-o" | "-Q" | "-R" | "-S" | "-W" | "-w" => {
                words.next();
            }
            flag if flag.starts_with('-') => {}
            word if destination.is_none() => destination = Some(word.to_string()),
            word => bail!("unexpected `{word}`; commands to run aren't supported"),
        }
    }
    let destination = destination.context("no host given")?;
    let (login, host_port) = match destination.rsplit_once('@') {
        Some((login, rest)) => (Some(login.to_string()), rest.to_string()),
        None => (None, destination),
    };
    let (host, inline_port) = match host_port.rsplit_once(':') {
        Some((host, p)) if !host.contains(':') => (
            host.to_string(),
            Some(p.parse::<u16>().context("bad port")?),
        ),
        _ => (host_port, None),
    };
    let user = user
        .or(login)
        .or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("USERNAME").ok())
        .filter(|user| !user.is_empty())
        .context("no user given")?;
    anyhow::ensure!(!host.is_empty(), "no host given");
    Ok((
        Target {
            host,
            port: port.or(inline_port).unwrap_or(22),
            user,
        },
        key,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_destination() {
        let (target, key) = parse_command("deploy@10.0.4.21").unwrap();
        assert_eq!(
            target,
            Target {
                host: "10.0.4.21".into(),
                port: 22,
                user: "deploy".into()
            }
        );
        assert_eq!(key, None);
    }

    #[test]
    fn parses_ssh_command_with_flags() {
        let (target, key) =
            parse_command("ssh -p 2222 -i ~/k -o StrictHostKeyChecking=no admin@bastion").unwrap();
        assert_eq!(
            target,
            Target {
                host: "bastion".into(),
                port: 2222,
                user: "admin".into()
            }
        );
        assert_eq!(key, Some(PathBuf::from("~/k")));
    }

    #[test]
    fn parses_inline_port_and_login_flag() {
        let (target, _) = parse_command("-l pi 192.168.1.40:2200").unwrap();
        assert_eq!(
            target,
            Target {
                host: "192.168.1.40".into(),
                port: 2200,
                user: "pi".into()
            }
        );
    }

    #[test]
    fn rejects_remote_commands() {
        assert!(parse_command("ssh host uptime").is_err());
    }

    #[test]
    fn quick_destination_only_for_destinations() {
        assert!(quick_destination("prod").is_none());
        assert!(quick_destination("postgres eu-west").is_none());
        assert!(quick_destination("deploy@").is_none());
        let (target, _) = quick_destination(" deploy@10.0.4.21:2200 ").unwrap();
        assert_eq!((target.user.as_str(), target.port), ("deploy", 2200));
        let (target, key) = quick_destination("ssh -i k -l pi homelab").unwrap();
        assert_eq!(
            (target.user.as_str(), target.host.as_str()),
            ("pi", "homelab")
        );
        assert_eq!(key, Some(PathBuf::from("k")));
    }

    #[test]
    fn credentials_follow_the_sign_in_method() {
        let kinds = |credentials: Vec<Credential>| -> Vec<&'static str> {
            credentials
                .iter()
                .map(|credential| match credential {
                    Credential::Agent => "agent",
                    Credential::KeyFile { .. } => "key",
                    Credential::Password(_) => "password",
                })
                .collect()
        };
        let key = Auth::Key("k".into());
        assert!(credentials_for(&Auth::Password, None).is_empty());
        assert_eq!(
            kinds(credentials_for(&Auth::Password, Some("pw"))),
            ["password"]
        );
        assert_eq!(
            kinds(credentials_for(&key, Some("pw"))),
            ["key", "password"]
        );
        assert_eq!(kinds(credentials_for(&key, None))[..2], ["key", "agent"]);
        assert_eq!(kinds(credentials_for(&Auth::Agent, None))[0], "agent");
    }

    #[test]
    fn jump_hosts_never_get_the_typed_password() {
        let target = Target {
            host: "db".into(),
            port: 22,
            user: "deploy".into(),
        };
        let mut connection = Connection::typed(target.clone(), None);
        connection.auth = Auth::Password;
        connection.jumps = ["gateway", "bastion"]
            .map(|host| Jump {
                target: Target {
                    host: host.into(),
                    ..target.clone()
                },
                auth: Auth::Agent,
            })
            .to_vec();
        let request = connection.request(TermSize { cols: 80, rows: 24 }, Some("pw"), None);
        assert!(matches!(request.credentials[..], [Credential::Password(_)]));
        let hops: Vec<&str> = request
            .jumps
            .iter()
            .map(|(jump, _)| jump.host.as_str())
            .collect();
        assert_eq!(hops, ["gateway", "bastion"]);
        assert!(request.jumps.iter().all(|(_, credentials)| {
            credentials
                .iter()
                .all(|credential| !matches!(credential, Credential::Password(_)))
        }));
    }

    #[test]
    fn key_file_info_reads_the_public_half_or_the_key() {
        use russh::keys::PrivateKey;
        use russh::keys::ssh_key::LineEnding;

        let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
        let dir = std::env::temp_dir().join(format!("tern-key-info-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("id_test");
        std::fs::write(&path, key.to_openssh(LineEnding::LF).unwrap()).unwrap();

        let info = key_file_info(&path).unwrap();
        assert_eq!(info.kind, "ed25519");
        assert_eq!(info.fingerprint, fingerprint(key.public_key()));
        assert_eq!(info.art, key_art(key.public_key()));

        // The .pub file is enough, even when the key itself can't be read.
        std::fs::write(&path, "not a key").unwrap();
        assert!(key_file_info(&path).is_none());
        let public = key.public_key().to_openssh().unwrap();
        std::fs::write(dir.join("id_test.pub"), public).unwrap();
        assert_eq!(key_file_info(&path).unwrap().art, info.art);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn probe_wants_an_ssh_greeting() {
        use std::io::Write as _;

        let serve = |reply: &'static [u8]| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = stream.write_all(reply);
            });
            port
        };
        let check = |port| {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(probe("127.0.0.1", port))
        };

        let reached = check(serve(b"hello\r\nSSH-2.0-Test_1.0\r\n")).unwrap();
        assert_eq!(reached.banner, "SSH-2.0-Test_1.0");
        assert!(reached.latency_ms >= 1);
        assert_eq!(
            check(serve(b"HTTP/1.1 400 Bad Request\r\n\r\n")),
            Err("closed without an SSH greeting".into())
        );

        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        assert_eq!(check(port), Err("connection refused".into()));
    }

    #[test]
    fn known_host_key_prefers_the_type_tern_negotiates_first() {
        use russh::keys::PrivateKey;

        const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQDCx/wi6QiCzaJnLo5+/N5/gFeq8j3snfGj74PO9kwQHot1iA70yIoXHhzXQ7JhkeTWLVsoAXU2QYbLCbZPTK2FtBUp7sYNCedtyyA/tj8DNXm3CWP/kotBULqeKuCAVNcmGAWolHjzDPbfK9J4rIeMErZ2FH4l9M7be3RCW0xvpsTTbK6FbRDdfmivyJ0WzlAb9QZ1snQrbSwxoxlask1OgGL8bX3WisKFY92lalJV2M1WFDwUwAs6EGJNp5+16JzjhtrpJuc49OPRXEKBhd8WI+9zYaM2lPbgkh+ylx/y5kat7EDg/yXOMWrQQ5o/8Qlzzke91FfnVX7+EK/iSoW/";
        let ed25519 = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
            .unwrap()
            .public_key()
            .clone();
        let dir = std::env::temp_dir().join(format!("tern-known-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (system, ours) = (dir.join("system"), dir.join("ours"));
        std::fs::write(&system, format!("[db.example]:2222 {RSA}\nother {RSA}\n")).unwrap();
        std::fs::write(
            &ours,
            format!("[db.example]:2222 {}\n", ed25519.to_openssh().unwrap()),
        )
        .unwrap();
        let files = [system, ours.clone(), dir.join("missing")];

        let found = known_host_key_in(&files, "db.example", 2222).unwrap();
        assert_eq!(found, ed25519);
        assert!(known_host_key_in(&files, "db.example", 22).is_none());
        let rsa_only = known_host_key_in(&files[..1], "db.example", 2222).unwrap();
        assert!(matches!(rsa_only.algorithm(), Algorithm::Rsa { .. }));
        // The art follows the key.
        assert_ne!(key_art(&found), key_art(&rsa_only));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
