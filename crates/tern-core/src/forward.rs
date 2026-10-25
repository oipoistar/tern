//! Port forwarding over a session, saved per host: local (`ssh -L`), remote
//! (`ssh -R`) and dynamic SOCKS (`ssh -D`) rules. A rule switched on runs
//! whenever a session to its host is open, and can be switched on and off
//! while it is.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use russh::client;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::host::Unknown;
use crate::ssh::{Event, describe};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardKind {
    /// Connections to a port on this computer go out from the host.
    Local,
    /// Connections to a port on the host come back to this computer.
    Remote,
    /// A SOCKS proxy on this computer whose connections go out from the
    /// host, wherever each one asks.
    Dynamic,
}

/// One forwarding rule.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Forward {
    pub kind: ForwardKind,
    /// Where connections arrive: on this computer for local and dynamic
    /// rules, on the host for remote ones.
    pub listen_host: String,
    pub listen_port: u16,
    /// Where they go; dynamic rules leave it empty and ask per connection.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to_host: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub to_port: u16,
    /// Runs whenever a session to the host is open.
    #[serde(default)]
    pub on: bool,
    /// What a newer Tern added that this one doesn't know, kept as it came.
    #[serde(flatten)]
    pub unknown: Unknown,
}

fn is_zero(port: &u16) -> bool {
    *port == 0
}

/// A rule without its switch, which is how a session tells rules apart.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ForwardId(ForwardKind, String, u16, String, u16);

impl Forward {
    pub fn id(&self) -> ForwardId {
        ForwardId(
            self.kind,
            self.listen_host.clone(),
            self.listen_port,
            self.to_host.clone(),
            self.to_port,
        )
    }

    /// OpenSSH's flag for the kind: `-L`, `-R` or `-D`.
    pub fn flag(&self) -> &'static str {
        match self.kind {
            ForwardKind::Local => "-L",
            ForwardKind::Remote => "-R",
            ForwardKind::Dynamic => "-D",
        }
    }

    /// The rule in a line: `127.0.0.1:8080 → db.internal:5432`, or just
    /// the listening end for a dynamic rule.
    pub fn route(&self) -> String {
        let listen = format!("{}:{}", self.listen_host, self.listen_port);
        match self.kind {
            ForwardKind::Dynamic => listen,
            _ => format!("{listen} → {}:{}", self.to_host, self.to_port),
        }
    }

    /// A rule from the form: a port to listen on and, unless dynamic, a
    /// `host:port` to send to. Local and dynamic rules listen on this
    /// computer only; remote ones on the host's loopback. Errors are one
    /// line.
    pub fn typed(kind: ForwardKind, port: &str, to: &str) -> Result<Self, String> {
        let listen_port = port
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|&port| port > 0)
            .ok_or("The port must be a number from 1 to 65535")?;
        let (to_host, to_port) = match kind {
            ForwardKind::Dynamic => (String::new(), 0),
            _ => {
                let to = to.trim();
                let (host, port) = to
                    .rsplit_once(':')
                    .filter(|(host, _)| !host.is_empty())
                    .ok_or("Send to host:port")?;
                let port = port
                    .parse::<u16>()
                    .ok()
                    .filter(|&port| port > 0)
                    .ok_or("Send to host:port")?;
                let host = host.trim_start_matches('[').trim_end_matches(']');
                if crate::host::address_problem(host).is_some() {
                    return Err("Send to host:port".into());
                }
                (host.to_string(), port)
            }
        };
        Ok(Self {
            kind,
            listen_host: match kind {
                ForwardKind::Remote => "localhost".into(),
                _ => "127.0.0.1".into(),
            },
            listen_port,
            to_host,
            to_port,
            on: true,
            unknown: Unknown::new(),
        })
    }
}

/// How a rule is doing in a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForwardState {
    Running,
    Stopped,
    /// Why it couldn't start, in a few words.
    Failed(String),
}

/// Where remote rules' connections go, by the port they arrived on at the
/// host. The session's handler reads it when the host opens a channel.
pub(crate) type RemoteTargets = Arc<Mutex<HashMap<u32, (String, u16)>>>;

/// Runs a session's rules on its connection.
pub(crate) struct Forwarder<H: client::Handler> {
    handle: Arc<client::Handle<H>>,
    remote: RemoteTargets,
    running: HashMap<ForwardId, Running>,
    events: mpsc::UnboundedSender<Event>,
}

enum Running {
    /// A listener on this computer.
    Listening(JoinHandle<()>),
    /// A port the host listens on, as it was granted.
    Remote { host: String, port: u32 },
}

impl<H: client::Handler + 'static> Forwarder<H> {
    pub(crate) fn new(
        handle: Arc<client::Handle<H>>,
        remote: RemoteTargets,
        events: mpsc::UnboundedSender<Event>,
    ) -> Self {
        Self {
            handle,
            remote,
            running: HashMap::new(),
            events,
        }
    }

    /// Starts or stops `forward`, then reports how it stands.
    pub(crate) async fn set(&mut self, forward: Forward, on: bool) {
        let state = if on {
            match self.start(&forward).await {
                Ok(()) => ForwardState::Running,
                Err(reason) => ForwardState::Failed(reason),
            }
        } else {
            self.stop(&forward.id()).await;
            ForwardState::Stopped
        };
        let _ = self.events.send(Event::Forwarding { forward, state });
    }

    async fn start(&mut self, forward: &Forward) -> Result<(), String> {
        let id = forward.id();
        if self.running.contains_key(&id) {
            return Ok(());
        }
        let running = match forward.kind {
            ForwardKind::Local | ForwardKind::Dynamic => {
                let listener =
                    TcpListener::bind((forward.listen_host.as_str(), forward.listen_port))
                        .await
                        .map_err(|error| describe(&error))?;
                let handle = self.handle.clone();
                let to = (forward.kind == ForwardKind::Local)
                    .then(|| (forward.to_host.clone(), forward.to_port));
                Running::Listening(tokio::spawn(accept(listener, handle, to)))
            }
            ForwardKind::Remote => {
                let asked = u32::from(forward.listen_port);
                let port = self
                    .handle
                    .tcpip_forward(forward.listen_host.clone(), asked)
                    .await
                    .map_err(|_| "the host refused to listen there".to_string())?;
                // A server that picked the port tells us; one that took
                // ours replies 0.
                let port = if port == 0 { asked } else { port };
                self.remote
                    .lock()
                    .unwrap()
                    .insert(port, (forward.to_host.clone(), forward.to_port));
                Running::Remote {
                    host: forward.listen_host.clone(),
                    port,
                }
            }
        };
        self.running.insert(id, running);
        Ok(())
    }

    async fn stop(&mut self, id: &ForwardId) {
        match self.running.remove(id) {
            Some(Running::Listening(task)) => task.abort(),
            Some(Running::Remote { host, port }) => {
                self.remote.lock().unwrap().remove(&port);
                let _ = self.handle.cancel_tcpip_forward(host, port).await;
            }
            None => {}
        }
    }

    /// Stops every listener on this computer; the host drops its own with
    /// the connection.
    pub(crate) fn stop_all(&mut self) {
        for (_, running) in self.running.drain() {
            if let Running::Listening(task) = running {
                task.abort();
            }
        }
    }
}

/// Accepts connections and sends each through the host: to `to` for a
/// local rule, or wherever its SOCKS request asks for a dynamic one.
async fn accept<H: client::Handler + 'static>(
    listener: TcpListener,
    handle: Arc<client::Handle<H>>,
    to: Option<(String, u16)>,
) {
    while let Ok((mut tcp, peer)) = listener.accept().await {
        let handle = handle.clone();
        let to = to.clone();
        tokio::spawn(async move {
            let (host, port, socks) = match to {
                Some((host, port)) => (host, port, false),
                None => match socks_request(&mut tcp).await {
                    Ok((host, port)) => (host, port, true),
                    Err(_) => return,
                },
            };
            let opened = handle
                .channel_open_direct_tcpip(
                    host,
                    port.into(),
                    peer.ip().to_string(),
                    peer.port().into(),
                )
                .await;
            if socks && socks_reply(&mut tcp, opened.is_ok()).await.is_err() {
                return;
            }
            if let Ok(channel) = opened {
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
            }
        });
    }
}

/// Answers a channel the host opened for a remote rule: connects to where
/// that rule sends and pipes the two together. Unknown ports are refused.
pub(crate) fn serve_remote(
    targets: &RemoteTargets,
    port: u32,
    channel: russh::Channel<client::Msg>,
    reply: client::ChannelOpenHandle,
) {
    let Some((host, to_port)) = targets.lock().unwrap().get(&port).cloned() else {
        // Dropping `reply` refuses the channel.
        return;
    };
    tokio::spawn(async move {
        match TcpStream::connect((host.as_str(), to_port)).await {
            Ok(mut tcp) => {
                reply.accept().await;
                let mut stream = channel.into_stream();
                let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
            }
            Err(_) => drop(reply),
        }
    });
}

/// Reads a SOCKS5 greeting and CONNECT request and returns where the
/// client wants to go. Only "no authentication" is offered.
async fn socks_request<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
) -> std::io::Result<(String, u16)> {
    let refuse = |what: &str| std::io::Error::other(format!("SOCKS: {what}"));
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await?;
    if head[0] != 5 {
        return Err(refuse("only version 5 is spoken"));
    }
    let mut methods = vec![0u8; usize::from(head[1])];
    stream.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        stream.write_all(&[5, 0xFF]).await?;
        return Err(refuse("the client wants authentication"));
    }
    stream.write_all(&[5, 0]).await?;

    let mut request = [0u8; 4];
    stream.read_exact(&mut request).await?;
    if request[1] != 1 {
        // Command not supported.
        stream.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Err(refuse("only CONNECT is supported"));
    }
    let host = match request[3] {
        1 => {
            let mut ip = [0u8; 4];
            stream.read_exact(&mut ip).await?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        3 => {
            let mut length = [0u8; 1];
            stream.read_exact(&mut length).await?;
            let mut name = vec![0u8; usize::from(length[0])];
            stream.read_exact(&mut name).await?;
            String::from_utf8(name).map_err(|_| refuse("the host name isn't text"))?
        }
        4 => {
            let mut ip = [0u8; 16];
            stream.read_exact(&mut ip).await?;
            std::net::Ipv6Addr::from(ip).to_string()
        }
        _ => return Err(refuse("unknown address type")),
    };
    let mut port = [0u8; 2];
    stream.read_exact(&mut port).await?;
    Ok((host, u16::from_be_bytes(port)))
}

/// Tells a SOCKS5 client whether its connection went through.
async fn socks_reply<S: AsyncWrite + Unpin>(stream: &mut S, ok: bool) -> std::io::Result<()> {
    let status = if ok { 0 } else { 5 };
    stream.write_all(&[5, status, 0, 1, 0, 0, 0, 0, 0, 0]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_rules_from_the_form() {
        let local = Forward::typed(ForwardKind::Local, " 8080 ", "db.internal:5432").unwrap();
        assert_eq!(local.route(), "127.0.0.1:8080 → db.internal:5432");
        assert_eq!(local.flag(), "-L");
        let remote = Forward::typed(ForwardKind::Remote, "9000", "[::1]:3000").unwrap();
        assert_eq!(remote.route(), "localhost:9000 → ::1:3000");
        let dynamic = Forward::typed(ForwardKind::Dynamic, "1080", "ignored").unwrap();
        assert_eq!(
            (dynamic.route(), dynamic.to_port),
            ("127.0.0.1:1080".into(), 0)
        );

        assert!(Forward::typed(ForwardKind::Local, "0", "a:1").is_err());
        assert!(Forward::typed(ForwardKind::Local, "80", "nowhere").is_err());
        assert!(Forward::typed(ForwardKind::Local, "80", ":22").is_err());
        assert!(Forward::typed(ForwardKind::Remote, "80", "a:99999").is_err());

        // The switch isn't part of which rule it is.
        let off = Forward {
            on: false,
            ..local.clone()
        };
        assert_eq!(off.id(), local.id());
    }

    #[tokio::test]
    async fn speaks_enough_socks5() {
        let (mut client, mut server) = tokio::io::duplex(64);
        let asked = tokio::spawn(async move { socks_request(&mut server).await });
        // Offers "no auth", then CONNECT to example.com:443 by name.
        client.write_all(&[5, 1, 0]).await.unwrap();
        let mut choice = [0u8; 2];
        client.read_exact(&mut choice).await.unwrap();
        assert_eq!(choice, [5, 0]);
        let mut connect = vec![5, 1, 0, 3, 11];
        connect.extend_from_slice(b"example.com");
        connect.extend_from_slice(&443u16.to_be_bytes());
        client.write_all(&connect).await.unwrap();
        assert_eq!(asked.await.unwrap().unwrap(), ("example.com".into(), 443));

        // An IPv4 address, and a client that insists on a password.
        let (mut client, mut server) = tokio::io::duplex(64);
        let asked = tokio::spawn(async move { socks_request(&mut server).await });
        client
            .write_all(&[5, 1, 0, 5, 1, 0, 1, 10, 0, 0, 7, 0, 22])
            .await
            .unwrap();
        assert_eq!(asked.await.unwrap().unwrap(), ("10.0.0.7".into(), 22));
        let (mut client, mut server) = tokio::io::duplex(64);
        let asked = tokio::spawn(async move { socks_request(&mut server).await });
        client.write_all(&[5, 1, 2]).await.unwrap();
        assert!(asked.await.unwrap().is_err());
    }
}
