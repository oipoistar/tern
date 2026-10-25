//! Interactive SSH sessions over russh.
//!
//! Sessions run on Tern's own Tokio runtime. Callers talk to them through
//! channels, so any executor (GPUI's included) can drive a session without
//! knowing about Tokio.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::pin::Pin;
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

use crate::certs;
use crate::forward::{Forward, ForwardState, Forwarder, RemoteTargets, serve_remote};
use crate::host::Auth;
use crate::paths;
use crate::proxy::{self, Proxy};
use crate::randomart::Randomart;
use crate::sftp::{self, Sftp};

/// A server's host key, as offered for the user to trust.
pub type HostKey = PublicKey;

/// A host key the user has just accepted, and the host they accepted it
/// for: it vouches for that host only, even if another host on the way
/// shows the same key (a cloned machine), as OpenSSH asks for each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trusted {
    pub host: String,
    pub port: u16,
    pub key: HostKey,
}

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
    /// Reach the first hop through this proxy.
    pub proxy: Option<Proxy>,
    pub size: TermSize,
    /// A host key the user has just accepted, for the target or a jump
    /// host. It's saved to Tern's known hosts once that host is verified.
    pub trust: Option<Trusted>,
    /// Ping idle connections so routers and firewalls keep them open.
    pub keep_alive: bool,
    /// Port forwarding rules to start once signed in.
    pub forwards: Vec<Forward>,
    /// Let the server use this computer's SSH agent.
    pub agent_forwarding: bool,
    /// Environment variables to ask the shell to start with.
    pub env: Vec<(String, String)>,
    /// Open a shell. Without one, the session only carries its port
    /// forwarding rules.
    pub shell: bool,
    /// Run this in the terminal instead of a login shell: a kept shell's
    /// attach, or a container's shell.
    pub command: Option<String>,
}

/// A host to go through, and how to sign in there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Jump {
    pub target: Target,
    pub auth: Auth,
    /// The saved host's id.
    pub host_id: Option<String>,
    /// The name its saved password is kept under in the credential store:
    /// its identity's, or its own.
    pub secret: Option<String>,
}

/// Everything needed to open a session: from a saved host
/// (`Vault::connection`) or a typed command ([`Connection::typed`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    /// The saved host this opens, if it is one.
    pub host: Option<String>,
    /// That host's id.
    pub host_id: Option<String>,
    /// The name its saved password is kept under in the credential store:
    /// its identity's, or its own.
    pub secret: Option<String>,
    pub target: Target,
    pub auth: Auth,
    /// The hosts to go through, first hop first.
    pub jumps: Vec<Jump>,
    /// The proxy the first hop is reached through.
    pub proxy: Option<Proxy>,
    pub keep_alive: bool,
    /// Let the server use this computer's SSH agent.
    pub agent_forwarding: bool,
    /// Input that doesn't come from the pane's own keyboard waits for a
    /// yes, or doesn't reach it ([`Host::guarded`](crate::Host::guarded)).
    pub guarded: bool,
    /// The shell runs in tmux or screen on the host, where it outlives the
    /// connection ([`shells`](crate::shells)).
    pub keep_shell: bool,
    /// A shell inside this container on the host instead of the host's own.
    pub container: Option<crate::containers::Container>,
    /// Environment variables the shell starts with.
    pub env: Vec<(String, String)>,
    /// Typed into the shell once it starts, at every login.
    pub on_login: Option<String>,
    /// Typed after `on_login` at the first login only: a snippet run on
    /// several hosts, or a key being installed. A reconnect doesn't type it
    /// again, and a pane split from this one doesn't get it.
    pub once: Option<String>,
    /// Port forwarding rules switched on for the host.
    pub forwards: Vec<Forward>,
    /// This computer's own shell instead of an SSH session.
    pub local: bool,
}

impl Connection {
    /// A shell on this computer ([`Session::local`]).
    pub fn local() -> Self {
        let user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_default();
        Self {
            local: true,
            keep_alive: false,
            ..Self::typed(
                Target {
                    host: "localhost".into(),
                    port: 0,
                    user,
                },
                None,
            )
        }
    }

    /// A destination typed or passed on the command line, with defaults.
    pub fn typed(target: Target, identity: Option<PathBuf>) -> Self {
        Self {
            host: None,
            host_id: None,
            secret: None,
            target,
            auth: identity.map_or(Auth::Agent, Auth::Key),
            jumps: Vec::new(),
            proxy: None,
            keep_alive: true,
            agent_forwarding: false,
            guarded: false,
            keep_shell: false,
            container: None,
            env: Vec::new(),
            on_login: None,
            once: None,
            forwards: Vec::new(),
            local: false,
        }
    }

    /// The request that opens this connection. `password` is one the user
    /// typed for the target; it never goes to a jump host, which gets only
    /// its own saved password, from `jump_password`.
    pub fn request(
        &self,
        size: TermSize,
        password: Option<&str>,
        trust: Option<Trusted>,
        jump_password: impl Fn(&Jump) -> Option<String>,
    ) -> ConnectRequest {
        ConnectRequest {
            target: self.target.clone(),
            credentials: credentials_for(&self.auth, password),
            jumps: self
                .jumps
                .iter()
                .map(|jump| {
                    let saved = (jump.auth == Auth::Password)
                        .then(|| jump_password(jump))
                        .flatten();
                    (
                        jump.target.clone(),
                        credentials_for(&jump.auth, saved.as_deref()),
                    )
                })
                .collect(),
            proxy: self.proxy.clone(),
            size,
            trust,
            keep_alive: self.keep_alive,
            forwards: self.forwards.clone(),
            agent_forwarding: self.agent_forwarding,
            env: self.env.clone(),
            shell: true,
            command: None,
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
    /// A jump host rejected every credential; the message names it. Like a
    /// refused sign-in it needs the user, so it isn't tried again.
    JumpRefused(String),
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
            Self::JumpRefused(message) | Self::Other(message) => f.write_str(message),
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
    /// The shell ended, or the server closed it.
    Closed {
        exit_status: Option<u32>,
    },
    /// The connection dropped (the network, or the server went away)
    /// while the shell was still running.
    Lost,
    Failed(ConnectError),
    /// A port forwarding rule started, stopped or couldn't start.
    Forwarding {
        forward: Forward,
        state: ForwardState,
    },
    /// The server asks questions while signing in, like a one-time code;
    /// answer with [`SessionInput::answer`].
    Prompts(Prompts),
}

/// Questions a server asks while signing in (SSH's keyboard-interactive
/// method), other than for the password Tern already has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompts {
    /// Who asks: `user@host`.
    pub from: String,
    /// What the server says about them, often nothing.
    pub instructions: String,
    /// Each question, and whether its answer may show as it's typed.
    pub questions: Vec<(String, bool)>,
}

/// How many rounds of questions a sign-in may take before Tern gives up.
const MAX_PROMPT_ROUNDS: usize = 8;

/// How long a shell's exit status may take to follow the end of its output.
const EXIT_STATUS_WAIT: Duration = Duration::from_secs(2);
/// How long a command run beside the terminal may take.
const EXEC_WAIT: Duration = Duration::from_secs(20);
/// How long closing waits for its last command.
const CLOSE_AFTER_WAIT: Duration = Duration::from_secs(3);
/// What a command run beside the terminal may print, at most.
const EXEC_OUTPUT: usize = 4 << 20;

/// Whether a hidden question asks for the account's password, rather than
/// a one-time code or a new password (an expired one asks for both).
fn asks_account_password(prompt: &str) -> bool {
    let prompt = prompt.to_lowercase();
    let words: Vec<&str> = prompt
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let other = [
        "new",
        "retype",
        "repeat",
        "again",
        "confirm",
        "verify",
        "verification",
        "otp",
        "totp",
        "oath",
        "token",
        "code",
        "pin",
    ];
    words.contains(&"password")
        && !words.iter().any(|word| other.contains(word))
        && !prompt.contains("one-time")
        && !prompt.contains("one time")
}

pub(crate) enum Input {
    Data(Vec<u8>),
    Resize(TermSize),
    /// Start (`true`) or stop a port forwarding rule.
    Forward(Forward, bool),
    /// Open an SFTP session on the connection.
    OpenSftp(oneshot::Sender<Result<Sftp, String>>),
    /// Answers to [`Event::Prompts`], in order.
    Answers(Vec<String>),
    /// Run a command on the connection, beside the terminal.
    Exec(String, oneshot::Sender<Result<Ran, String>>),
    /// Run a command on the connection, then close it.
    CloseAfter(String),
    Close,
}

/// What a command run on the connection printed, and how it ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ran {
    /// Its exit status, when the server said.
    pub status: Option<u32>,
    pub out: String,
    pub err: String,
}

/// Input for a session, with what arrived while sign-in waited for answers
/// kept for afterwards.
struct Inbox {
    input: mpsc::UnboundedReceiver<Input>,
    held: VecDeque<Input>,
}

impl Inbox {
    async fn recv(&mut self) -> Option<Input> {
        match self.held.pop_front() {
            Some(held) => Some(held),
            None => self.input.recv().await,
        }
    }

    /// The answers to questions just asked; `None` if the session closed
    /// instead.
    async fn answers(&mut self) -> Option<Vec<String>> {
        loop {
            match self.input.recv().await? {
                Input::Answers(answers) => return Some(answers),
                Input::Close => return None,
                other => self.held.push_back(other),
            }
        }
    }
}

/// Lets sign-in ask the user what the server asks.
struct Asker<'a> {
    inbox: &'a mut Inbox,
    events: &'a mpsc::UnboundedSender<Event>,
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

    /// Runs `command` on the connection, then closes it; how a shell kept
    /// running on the host ends for good. It runs even if every input is
    /// dropped right after.
    pub fn close_after(&self, command: String) {
        let _ = self.0.send(Input::CloseAfter(command));
    }

    /// Answers the questions of an [`Event::Prompts`], in order.
    pub fn answer(&self, answers: Vec<String>) {
        let _ = self.0.send(Input::Answers(answers));
    }

    /// Runs `command` on the connection, beside the terminal and without
    /// one, once it's signed in; what it printed, or why it couldn't run.
    pub fn exec(&self, command: String) -> impl Future<Output = Result<Ran, String>> + 'static {
        let (reply, ran) = oneshot::channel();
        let _ = self.0.send(Input::Exec(command, reply));
        async move {
            ran.await
                .unwrap_or_else(|_| Err("the session has ended".into()))
        }
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

    /// A shell on this computer, on a pseudo-terminal of `size`; its
    /// events and input are an SSH session's.
    pub fn local(size: TermSize) -> Self {
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("tern-local-shell".into())
            .spawn(move || crate::local::run(size, input_rx, event_tx))
            .expect("failed to start a thread for the local shell");
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

    /// Answers the questions of an [`Event::Prompts`], in order.
    pub fn answer(&self, answers: Vec<String>) {
        self.input.answer(answers);
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

pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
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
    input: mpsc::UnboundedReceiver<Input>,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<(), ConnectError> {
    let mut inbox = Inbox {
        input,
        held: VecDeque::new(),
    };
    let ConnectRequest {
        target,
        credentials,
        jumps,
        proxy,
        size,
        trust,
        keep_alive,
        forwards,
        agent_forwarding,
        env,
        shell,
        command,
    } = request;

    // Through jump hosts: sign in to each, then run the next one's SSH over
    // a channel it forwards, and so on to the target. Every jump
    // connection must outlive the session.
    let mut via = Vec::new();
    let mut tunnel = None;
    // Only the first hop is dialled from here, so only it uses the proxy.
    let mut proxy = proxy;
    for (at, (jump, jump_credentials)) in jumps.iter().enumerate() {
        let mut jump_handle = open(
            keep_alive,
            jump,
            trust.clone(),
            tunnel.take(),
            proxy.take(),
            RemoteTargets::default(),
            false,
        )
        .await?;
        let mut asker = Asker {
            inbox: &mut inbox,
            events,
        };
        authenticate(&mut jump_handle, jump, jump_credentials, &mut asker, true)
            .await
            .map_err(|error| {
                let message = format!("can't sign in to {}: {error}", jump.host);
                // A refusal won't pass by itself, and repeating a wrong
                // password can lock the account; a dropped line may pass.
                match error {
                    ConnectError::AuthFailed { .. } => ConnectError::JumpRefused(message),
                    _ => other(message),
                }
            })?;
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
        keep_alive,
        &target,
        trust,
        tunnel,
        proxy,
        remote.clone(),
        agent_forwarding,
    )
    .await?;

    let mut asker = Asker {
        inbox: &mut inbox,
        events,
    };
    authenticate(&mut handle, &target, &credentials, &mut asker, false).await?;
    let _ = events.send(Event::Connected);
    let handle = Arc::new(handle);
    let mut forwarder = Forwarder::new(handle.clone(), remote, events.clone());
    for forward in forwards {
        forwarder.set(forward, true).await;
    }

    let mut exit_status = None;
    // The connection went away without the server closing the shell.
    let mut lost = false;
    if !shell {
        // Forwarding only: no shell, until asked to stop or the connection
        // drops, which nothing else would notice without a channel open.
        let mut check = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                command = inbox.recv() => match command {
                    Some(Input::Forward(forward, on)) => forwarder.set(forward, on).await,
                    Some(Input::OpenSftp(reply)) => {
                        let handle = handle.clone();
                        tokio::spawn(async move {
                            let _ = reply.send(sftp::open(&handle).await);
                        });
                    }
                    Some(Input::Exec(command, reply)) => {
                        let handle = handle.clone();
                        tokio::spawn(async move {
                            let _ = reply.send(exec(&handle, &command, EXEC_WAIT).await);
                        });
                    }
                    Some(Input::CloseAfter(command)) => {
                        let _ = exec(&handle, &command, CLOSE_AFTER_WAIT).await;
                        break;
                    }
                    Some(Input::Close) | None => break,
                    Some(_) => {}
                },
                _ = check.tick() => {
                    if handle.is_closed() {
                        lost = true;
                        break;
                    }
                }
            }
        }
        return finish(forwarder, &handle, &via, lost, None, events).await;
    }
    let channel = handle.channel_open_session().await.map_err(other)?;
    if agent_forwarding {
        channel.agent_forward(false).await.map_err(other)?;
    }
    // No reply asked: a server that doesn't accept a variable just leaves
    // it out, as OpenSSH's client does.
    for (name, value) in &env {
        channel.set_env(false, name, value).await.map_err(other)?;
    }
    channel
        .request_pty(false, TERM, size.cols.into(), size.rows.into(), 0, 0, &[])
        .await
        .map_err(other)?;
    match &command {
        Some(command) => channel.exec(true, command.as_bytes()).await,
        None => channel.request_shell(true).await,
    }
    .map_err(other)?;
    let (mut reader, writer) = channel.split();

    // OpenSSH sends the shell's exit status after the end of its output, so
    // the end waits for it, or for the channel to close; a server that does
    // neither gets a moment.
    let mut ended: Option<Pin<Box<tokio::time::Sleep>>> = None;
    loop {
        tokio::select! {
            message = reader.wait() => match message {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    let _ = events.send(Event::Output(data.to_vec()));
                }
                Some(ChannelMsg::ExitStatus { exit_status: status }) => {
                    exit_status = Some(status);
                    if ended.is_some() {
                        break;
                    }
                }
                Some(ChannelMsg::Eof) => {
                    if exit_status.is_some() {
                        break;
                    }
                    ended = Some(Box::pin(tokio::time::sleep(EXIT_STATUS_WAIT)));
                }
                Some(ChannelMsg::Close) => break,
                None => {
                    lost = ended.is_none();
                    break;
                }
                Some(_) => {}
            },
            _ = async { ended.as_mut().expect("armed").await }, if ended.is_some() => break,
            command = inbox.recv() => match command {
                Some(Input::Data(bytes)) => {
                    if writer.data(&bytes[..]).await.is_err() {
                        lost = true;
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
                Some(Input::Exec(command, reply)) => {
                    let handle = handle.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(exec(&handle, &command, EXEC_WAIT).await);
                    });
                }
                // Signed in already: late answers have no question.
                Some(Input::Answers(_)) => {}
                Some(Input::CloseAfter(command)) => {
                    let _ = exec(&handle, &command, CLOSE_AFTER_WAIT).await;
                    let _ = writer.close().await;
                    break;
                }
                Some(Input::Close) | None => {
                    let _ = writer.close().await;
                    break;
                }
            },
        }
    }
    finish(forwarder, &handle, &via, lost, exit_status, events).await
}

/// Runs `command` on its own channel of the connection, without a
/// terminal, for at most `wait`: what it printed and its exit status.
async fn exec<H: client::Handler>(
    handle: &client::Handle<H>,
    command: &str,
    wait: Duration,
) -> Result<Ran, String> {
    let ran = async {
        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|error| error.to_string())?;
        channel
            .exec(true, command.as_bytes())
            .await
            .map_err(|error| error.to_string())?;
        let (mut out, mut err, mut status) = (Vec::new(), Vec::new(), None);
        while let Some(message) = channel.wait().await {
            match message {
                ChannelMsg::Data { data } => out.extend_from_slice(&data),
                ChannelMsg::ExtendedData { data, .. } => err.extend_from_slice(&data),
                ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                ChannelMsg::Close => break,
                _ => {}
            }
            if out.len() + err.len() > EXEC_OUTPUT {
                return Err("it printed too much".to_string());
            }
        }
        Ok(Ran {
            status,
            out: String::from_utf8_lossy(&out).into_owned(),
            err: String::from_utf8_lossy(&err).into_owned(),
        })
    };
    tokio::time::timeout(wait, ran)
        .await
        .unwrap_or_else(|_| Err("it took too long".into()))
}

/// Ends a session: its rules stop, its connections close (the hop nearest
/// the target first), and it reports how it ended. A shell that reported
/// its exit ended, however the channel went.
async fn finish(
    mut forwarder: Forwarder<Verifier>,
    handle: &client::Handle<Verifier>,
    via: &[client::Handle<Verifier>],
    lost: bool,
    exit_status: Option<u32>,
    events: &mpsc::UnboundedSender<Event>,
) -> Result<(), ConnectError> {
    forwarder.stop_all();
    let _ = handle
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await;
    for jump in via.iter().rev() {
        let _ = jump
            .disconnect(russh::Disconnect::ByApplication, "", "en")
            .await;
    }
    let _ = events.send(if lost && exit_status.is_none() {
        Event::Lost
    } else {
        Event::Closed { exit_status }
    });
    Ok(())
}

/// Connects to `target` and verifies its host key: over TCP, through
/// `proxy`, or, through jump hosts, over `tunnel`. Host certificates are
/// asked for only when a known_hosts file names an authority for the host,
/// as OpenSSH does.
async fn open(
    keep_alive: bool,
    target: &Target,
    trust: Option<Trusted>,
    tunnel: Option<ChannelStream<client::Msg>>,
    proxy: Option<Proxy>,
    remote: RemoteTargets,
    agent_forwarding: bool,
) -> Result<client::Handle<Verifier>, ConnectError> {
    let files = known_hosts_files();
    let authorities = certs::authorities(&files, &target.host, target.port);
    let mut config = client::Config {
        keepalive_interval: keep_alive.then_some(KEEP_ALIVE),
        ..Default::default()
    };
    // The key types already known for the host first, as OpenSSH asks: a
    // server that still has its known key shows that one, so any other key
    // is a change (see `known_hosts::verdict`).
    let known = crate::known_hosts::known_types(&files, &target.host, target.port);
    let (first, rest): (Vec<Algorithm>, Vec<Algorithm>) = config
        .preferred
        .key
        .iter()
        .cloned()
        .partition(|algorithm| known.iter().any(|known| same_type(known, algorithm)));
    config.preferred.key = [first, rest].concat().into();
    if !authorities.is_empty() {
        config.preferred.host_key_certificates = config.preferred.key.clone();
    }
    let config = Arc::new(config);
    let verdict = Arc::new(Mutex::new(None));
    let handler = Verifier {
        host: target.host.clone(),
        port: target.port,
        // Only the host it was accepted for.
        trust: trust
            .filter(|trusted| trusted.host == target.host && trusted.port == target.port)
            .map(|trusted| trusted.key),
        authorities,
        verdict: verdict.clone(),
        remote,
        agent_forwarding,
    };
    let connected = match (tunnel, proxy) {
        (Some(stream), _) => client::connect_stream(config, stream, handler).await,
        (None, Some(proxy)) => {
            let stream = proxy::connect(&proxy, &target.host, target.port)
                .await
                .map_err(other)?;
            client::connect_stream(config, stream, handler).await
        }
        (None, None) => client::connect(config, (target.host.as_str(), target.port), handler).await,
    };
    // A rejected host key surfaces as a generic russh error; report ours.
    // Network errors read as a few words, without the OS's error number.
    connected.map_err(|error| {
        verdict
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| match &error {
                russh::Error::IO(io) => other(describe(io)),
                _ => other(error),
            })
    })
}

fn other(error: impl std::fmt::Display) -> ConnectError {
    ConnectError::Other(error.to_string())
}

/// Checks the server's host key against `~/.ssh/known_hosts` and Tern's own
/// known hosts, learning a key the user explicitly trusted (see
/// [`paths::trusted_known_hosts`]), or its host certificate against the
/// authorities they name. Also answers the channels the server opens for
/// remote forwarding rules.
struct Verifier {
    host: String,
    port: u16,
    trust: Option<PublicKey>,
    /// The certificate authorities the known_hosts files name for the host.
    authorities: Vec<PublicKey>,
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
        let verdict = match server {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                check_host_key(&self.host, self.port, key, self.trust.as_ref())
            }
            PublicKeyOrCertificate::Certificate(cert) => {
                check_host_certificate(&self.host, self.port, cert, &self.authorities)
            }
        };
        match verdict {
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

/// Whether a key of type `known` shows as `algorithm`: RSA keys sign with
/// any of RSA's hashes.
fn same_type(known: &Algorithm, algorithm: &Algorithm) -> bool {
    matches!(
        (known, algorithm),
        (Algorithm::Rsa { .. }, Algorithm::Rsa { .. })
    ) || known == algorithm
}

/// A host certificate is good when an authority known_hosts names for the
/// host vouches for it, and neither that authority nor the key is
/// `@revoked`. Otherwise, as OpenSSH does, the key inside counts as a plain
/// host key, already trusted or not; it isn't offered for trust, because a
/// host that has an authority shouldn't need it.
fn check_host_certificate(
    host: &str,
    port: u16,
    cert: &russh::keys::Certificate,
    authorities: &[PublicKey],
) -> Result<(), ConnectError> {
    let key = PublicKey::from(cert.public_key().clone());
    let authority = PublicKey::from(cert.signature_key().clone());
    for revoked in [&key, &authority] {
        if let Some((file, line)) =
            crate::known_hosts::revoked(&known_hosts_files(), host, port, revoked)
        {
            return Err(other(format!(
                "the host certificate is revoked by {}:{line}",
                file.display()
            )));
        }
    }
    let Err(reason) = certs::check_host(cert, host, authorities, certs::now()) else {
        return Ok(());
    };
    match check_host_key(host, port, &key, None) {
        Err(ConnectError::UnknownHostKey { .. }) => {
            Err(other(format!("the host certificate {reason}")))
        }
        known => known,
    }
}

/// The policy behind [`Verifier`], kept separate so it can be tested.
pub fn check_host_key(
    host: &str,
    port: u16,
    key: &PublicKey,
    trust: Option<&PublicKey>,
) -> Result<(), ConnectError> {
    use crate::known_hosts::{Verdict, learn, verdict};
    match verdict(&known_hosts_files(), host, port, key).map_err(other)? {
        Verdict::Trusted => Ok(()),
        Verdict::Changed { file, line } => Err(ConnectError::HostKeyChanged {
            fingerprint: fingerprint(key),
            file,
            line,
        }),
        Verdict::Revoked { file, line } => Err(other(format!(
            "the host key {} is revoked by {}:{line}",
            fingerprint(key),
            file.display()
        ))),
        Verdict::Unknown if trust == Some(key) => {
            let file = paths::trusted_known_hosts();
            let saved = |error: &dyn std::fmt::Display| {
                other(format!(
                    "can't save the host key to {}: {error}",
                    file.display()
                ))
            };
            // A missing ~/.ssh is made the user's alone, as OpenSSH makes it.
            if let Some(dir) = file.parent() {
                paths::private_dir(dir).map_err(|error| saved(&error))?;
            }
            learn(&file, host, port, key).map_err(|error| saved(&error))
        }
        Verdict::Unknown => Err(ConnectError::UnknownHostKey {
            host: host.into(),
            port,
            key: Box::new(key.clone()),
            fingerprint: fingerprint(key),
        }),
    }
}

/// The OpenSSH user's file, then Tern's own.
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
        .min_by_key(preference)
}

/// The key [`known_host_key`] gives for each of `places` (address and
/// port), reading each known_hosts file once for all of them.
pub fn known_host_keys_for(places: &[(String, u16)]) -> HashMap<(String, u16), HostKey> {
    let mut best: HashMap<(String, u16), HostKey> = HashMap::new();
    for file in known_hosts_files() {
        for (place, keys) in crate::known_hosts::keys_for(places, &file) {
            for key in keys {
                match best.get(&place) {
                    Some(kept) if preference(kept) <= preference(&key) => {}
                    _ => {
                        best.insert(place.clone(), key);
                    }
                }
            }
        }
    }
    best
}

/// The order Tern asks servers for host key types in.
fn preference(key: &HostKey) -> u8 {
    match key.algorithm() {
        Algorithm::Ed25519 => 0,
        Algorithm::Ecdsa { .. } => 1,
        Algorithm::Rsa { .. } => 2,
        _ => 3,
    }
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
pub fn probe(
    host: &str,
    port: u16,
    proxy: Option<Proxy>,
) -> impl Future<Output = Result<Reached, String>> + 'static {
    let host = host.to_string();
    spawn(async move { probe_now(&host, port, proxy).await })
}

async fn probe_now(host: &str, port: u16, proxy: Option<Proxy>) -> Result<Reached, String> {
    let started = Instant::now();
    let stream = match &proxy {
        // Through a proxy, the time it takes to open the tunnel.
        Some(proxy) => tokio::time::timeout(PROBE_TIMEOUT, proxy::connect(proxy, host, port))
            .await
            .map_err(|_| "no answer".to_string())??,
        None => tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect((host, port)))
            .await
            .map_err(|_| "no answer".to_string())?
            .map_err(|error| describe(&error))?,
    };
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

/// Signs in with `credentials`, then the server's questions. A jump host
/// (`jump`) is asked a password question in the pane; the target's comes
/// from its password card instead.
async fn authenticate(
    handle: &mut client::Handle<Verifier>,
    target: &Target,
    credentials: &[Credential],
    asker: &mut Asker<'_>,
    jump: bool,
) -> Result<(), ConnectError> {
    let user = target.user.as_str();
    let mut tried = Vec::new();
    for credential in credentials {
        let outcome = match credential {
            Credential::Agent => {
                tried.push("agent".to_string());
                with_agent(handle, user).await
            }
            Credential::KeyFile {
                path,
                passphrase: None,
            } if is_encrypted(path) => {
                tried.push(format!("key {}", path.display()));
                // Once the server has said it would take the key, it waits
                // for the signature: without the passphrase, this sign-in
                // can go no further.
                Ok(with_encrypted_key(handle, target, path, asker)
                    .await
                    .map_err(other)?)
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
    // Then the server's own questions, as OpenSSH's keyboard-interactive
    // does: two-factor codes, or a password where plain password sign-in
    // is off.
    let password = credentials.iter().find_map(|credential| match credential {
        Credential::Password(password) => Some(password.as_str()),
        _ => None,
    });
    tried.push("keyboard-interactive".to_string());
    if let Ok(true) = interactive(handle, target, password, asker, jump).await {
        return Ok(());
    }
    Err(ConnectError::AuthFailed { tried })
}

/// Answers the server's questions: a password question from `password`,
/// the rest by asking the user. Without a password to give, a password
/// question fails the attempt, so the session asks for one the usual way,
/// unless `ask_password` says to ask it like the others.
async fn interactive(
    handle: &mut client::Handle<Verifier>,
    target: &Target,
    password: Option<&str>,
    asker: &mut Asker<'_>,
    ask_password: bool,
) -> Result<bool> {
    use client::KeyboardInteractiveAuthResponse as Reply;
    let mut unused = password;
    let mut reply = handle
        .authenticate_keyboard_interactive_start(target.user.as_str(), None)
        .await?;
    for _ in 0..MAX_PROMPT_ROUNDS {
        let (instructions, prompts) = match reply {
            Reply::Success => return Ok(true),
            Reply::Failure { .. } => return Ok(false),
            Reply::InfoRequest {
                instructions,
                prompts,
                ..
            } => (instructions, prompts),
        };
        // The password answers the first question for it, once: asked
        // again, it was wrong, and the user decides what to give.
        let mut answers: Vec<Option<String>> = Vec::new();
        let mut needs_password = false;
        for prompt in &prompts {
            let account = !prompt.echo && asks_account_password(&prompt.prompt);
            let answer = account.then(|| unused.take().map(String::from)).flatten();
            needs_password |= account && answer.is_none();
            answers.push(answer);
        }
        if needs_password && !ask_password {
            return Ok(false);
        }
        let questions: Vec<(String, bool)> = prompts
            .iter()
            .zip(&answers)
            .filter(|(_, answer)| answer.is_none())
            .map(|(prompt, _)| (prompt.prompt.trim().to_string(), prompt.echo))
            .collect();
        if !questions.is_empty() {
            let _ = asker.events.send(Event::Prompts(Prompts {
                from: format!("{}@{}", target.user, target.host),
                instructions: instructions.trim().to_string(),
                questions,
            }));
            let Some(given) = asker.inbox.answers().await else {
                return Ok(false);
            };
            let mut given = given.into_iter();
            for answer in answers.iter_mut().filter(|answer| answer.is_none()) {
                *answer = Some(given.next().unwrap_or_default());
            }
        }
        reply = handle
            .authenticate_keyboard_interactive_respond(
                answers.into_iter().map(Option::unwrap_or_default).collect(),
            )
            .await?;
    }
    Ok(false)
}

async fn with_key_file(
    handle: &mut client::Handle<Verifier>,
    user: &str,
    path: &Path,
    passphrase: Option<&str>,
) -> Result<bool> {
    let key = Arc::new(
        russh::keys::load_secret_key(path, passphrase)
            .with_context(|| format!("can't load {}", path.display()))?,
    );
    // With a certificate beside it, that's tried first, then the bare key.
    if let Some(cert) = certs::user_certificate(path)
        && handle
            .authenticate_openssh_cert(user, key.clone(), cert)
            .await?
            .success()
    {
        return Ok(true);
    }
    let hash = handle.best_supported_rsa_hash().await?.flatten();
    let result = handle
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(key, hash))
        .await?;
    Ok(result.success())
}

/// Whether the key file can only be read with a passphrase.
fn is_encrypted(path: &Path) -> bool {
    matches!(
        russh::keys::load_secret_key(path, None),
        Err(russh::keys::Error::KeyIsEncrypted)
    )
}

/// Signs in with an encrypted key file as ssh does: its public half is
/// offered first, and only if the server would take it is the passphrase
/// asked for (three tries). False when the server won't take it; an error
/// when it would, but there's no passphrase to sign with.
async fn with_encrypted_key(
    handle: &mut client::Handle<Verifier>,
    target: &Target,
    path: &Path,
    asker: &mut Asker<'_>,
) -> Result<bool, String> {
    // An OpenSSH key keeps its public half readable; an older one has it
    // beside it, as `.pub`.
    let public = russh::keys::ssh_key::PrivateKey::read_openssh_file(path)
        .map(|key| key.public_key().clone())
        .or_else(|_| {
            let mut beside = path.as_os_str().to_owned();
            beside.push(".pub");
            russh::keys::load_public_key(PathBuf::from(beside))
        });
    let Ok(public) = public else {
        return Ok(false);
    };
    let user = target.user.as_str();
    let hash = handle
        .best_supported_rsa_hash()
        .await
        .map_err(|error| error.to_string())?
        .flatten();
    let mut unlock = Unlock {
        path,
        target,
        asker,
        key: None,
    };
    // With a certificate beside it, that's offered first, then the bare key.
    if let Some(cert) = certs::user_certificate(path)
        && handle
            .authenticate_certificate_with(user, cert, hash, &mut unlock)
            .await
            .map_err(|error| error.to_string())?
            .success()
    {
        return Ok(true);
    }
    let result = handle
        .authenticate_publickey_with(user, public, hash, &mut unlock)
        .await
        .map_err(|error| error.to_string())?;
    Ok(result.success())
}

/// Signs for an encrypted key file once the server has said it would take
/// the key, asking for the passphrase then; it keeps the key it unlocked.
struct Unlock<'a, 'b> {
    path: &'a Path,
    target: &'a Target,
    asker: &'a mut Asker<'b>,
    key: Option<russh::keys::PrivateKey>,
}

impl Unlock<'_, '_> {
    /// The key, unlocked with a passphrase the user gives: asked up to three
    /// times, as ssh does.
    async fn key(&mut self) -> Result<&russh::keys::PrivateKey> {
        let name = self.path.file_name().map_or_else(
            || self.path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        for _ in 0..3 {
            if self.key.is_some() {
                break;
            }
            let _ = self.asker.events.send(Event::Prompts(Prompts {
                from: format!("{}@{}", self.target.user, self.target.host),
                instructions: String::new(),
                questions: vec![(format!("Passphrase for {name}:"), false)],
            }));
            let Some(answers) = self.asker.inbox.answers().await else {
                bail!("signing in was stopped");
            };
            let typed = answers.into_iter().next().unwrap_or_default();
            self.key = russh::keys::load_secret_key(self.path, Some(&typed)).ok();
        }
        self.key
            .as_ref()
            .with_context(|| format!("{name}: the passphrase wasn't right"))
    }
}

impl russh::Signer for Unlock<'_, '_> {
    type Error = anyhow::Error;

    async fn auth_sign(
        &mut self,
        _: &AgentIdentity,
        hash_alg: Option<HashAlg>,
        mut to_sign: Vec<u8>,
    ) -> Result<Vec<u8>> {
        use russh::keys::signature::Signer as _;
        use russh::keys::ssh_key::encoding::Encode as _;
        use russh::keys::ssh_key::private::KeypairData;

        let key = self.key().await?;
        // RSA signs with the hash the server agreed to; the rest have one.
        let signature: russh::keys::ssh_key::Signature = match key.key_data() {
            KeypairData::Rsa(rsa) => (rsa, hash_alg).try_sign(&to_sign)?,
            _ => key.try_sign(&to_sign)?,
        };
        // As an agent answers: the data, then the signature as a string.
        signature.encode_vec()?.as_slice().encode(&mut to_sign)?;
        Ok(to_sign)
    }
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
        let signed_in = match identity {
            AgentIdentity::PublicKey { key, .. } => handle
                .authenticate_publickey_with(user, key, hash, &mut agent)
                .await?
                .success(),
            AgentIdentity::Certificate { certificate, .. } => handle
                .authenticate_certificate_with(user, certificate, hash, &mut agent)
                .await?
                .success(),
        };
        if signed_in {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Default credentials when the user gave none: the agent, then OpenSSH's
/// usual key files that exist.
pub fn default_credentials() -> Vec<Credential> {
    let mut credentials = vec![Credential::Agent];
    if let Some(dir) = crate::paths::ssh_dir() {
        for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
            let path = dir.join(name);
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
    fn answers_only_the_account_password_question() {
        for asked in [
            "Password:",
            "Password for alice@db:",
            "(current) UNIX password:",
        ] {
            assert!(asks_account_password(asked), "{asked}");
        }
        for other in [
            "One-time password (OATH) for `alice':",
            "New password:",
            "Retype new password:",
            "Verification code:",
            "Password + OTP token:",
            "Passphrase for key:",
        ] {
            assert!(!asks_account_password(other), "{other}");
        }
    }

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
                host_id: None,
                secret: None,
                target: Target {
                    host: host.into(),
                    ..target.clone()
                },
                auth: Auth::Agent,
            })
            .to_vec();
        let size = TermSize { cols: 80, rows: 24 };
        let request = connection.request(size, Some("pw"), None, |_| None);
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

        // A jump host that signs in with a password gets its own saved one.
        connection.jumps[1].auth = Auth::Password;
        connection.jumps[1].secret = Some("password bastion-id".into());
        let saved = |jump: &Jump| {
            (jump.secret.as_deref() == Some("password bastion-id")).then(|| "jump-pw".to_string())
        };
        let request = connection.request(size, Some("pw"), None, saved);
        assert!(matches!(
            &request.jumps[1].1[..],
            [Credential::Password(password)] if password == "jump-pw"
        ));
        assert!(
            request.jumps[0]
                .1
                .iter()
                .all(|credential| !matches!(credential, Credential::Password(_)))
        );
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
                .block_on(probe("127.0.0.1", port, None))
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
