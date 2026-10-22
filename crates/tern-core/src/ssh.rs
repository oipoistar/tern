//! Interactive SSH sessions over russh.
//!
//! Sessions run on Tern's own Tokio runtime. Callers talk to them through
//! channels, so any executor (GPUI's included) can drive a session without
//! knowing about Tokio.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate, known_hosts};
use russh::{ChannelMsg, client};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::paths;

/// A server's host key, as offered for the user to trust.
pub type HostKey = PublicKey;

/// Terminal type announced to the server.
const TERM: &str = "xterm-256color";

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
    pub size: TermSize,
    /// A host key the user has just accepted for this target. It's saved to
    /// Tern's known hosts once the connection succeeds.
    pub trust: Option<PublicKey>,
}

/// Why a connection didn't get to a shell.
#[derive(Clone, Debug)]
pub enum ConnectError {
    /// The server's key is in no known_hosts file. Ask the user, then retry
    /// with [`ConnectRequest::trust`] set to `key`.
    UnknownHostKey {
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
            Self::UnknownHostKey { fingerprint, .. } => write!(f, "unknown host key {fingerprint}"),
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
}

enum Input {
    Data(Vec<u8>),
    Resize(TermSize),
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

    pub fn close(&self) {
        let _ = self.0.send(Input::Close);
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

    pub fn close(&self) {
        self.input.close();
    }

    /// Splits the session so input and events can live in different places.
    pub fn into_parts(self) -> (SessionInput, SessionEvents) {
        (self.input, SessionEvents(self.events))
    }
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
        size,
        trust,
    } = request;
    let verdict = Arc::new(Mutex::new(None));
    let handler = Verifier {
        host: target.host.clone(),
        port: target.port,
        trust,
        verdict: verdict.clone(),
    };
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });

    let connected = client::connect(config, (target.host.as_str(), target.port), handler).await;
    let mut handle = match connected {
        Ok(handle) => handle,
        // A rejected host key surfaces as a generic russh error; report ours.
        Err(error) => {
            return Err(verdict
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| other(error)));
        }
    };

    authenticate(&mut handle, &target.user, &credentials).await?;
    let _ = events.send(Event::Connected);

    let channel = handle.channel_open_session().await.map_err(other)?;
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
                Some(Input::Close) | None => {
                    let _ = writer.close().await;
                    break;
                }
            },
        }
    }
    let _ = handle
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await;
    let _ = events.send(Event::Closed { exit_status });
    Ok(())
}

fn other(error: impl std::fmt::Display) -> ConnectError {
    ConnectError::Other(error.to_string())
}

/// Checks the server's host key against `~/.ssh/known_hosts` (read only) and
/// Tern's own known hosts, learning a key the user explicitly trusted.
struct Verifier {
    host: String,
    port: u16,
    trust: Option<PublicKey>,
    verdict: Arc<Mutex<Option<ConnectError>>>,
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
}

/// The policy behind [`Verifier`], kept separate so it can be tested.
pub fn check_host_key(
    host: &str,
    port: u16,
    key: &PublicKey,
    trust: Option<&PublicKey>,
) -> Result<(), ConnectError> {
    let ours = paths::known_hosts();
    let files: Vec<PathBuf> = paths::system_known_hosts()
        .into_iter()
        .chain([ours.clone()])
        .collect();
    for file in &files {
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
        key: Box::new(key.clone()),
        fingerprint: fingerprint(key),
    })
}

pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
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
}
