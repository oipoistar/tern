//! End-to-end checks of `tern_core::ssh` against a russh server running in
//! this process: host-key trust, password and key auth, data, resize, close,
//! and connecting through a jump host.

use std::sync::Arc;
use std::time::Duration;

use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{Auth, Msg, Server as _, Session as ServerSession};
use russh::{Channel, ChannelId};
use tern_core::ssh::{
    ConnectError, ConnectRequest, Credential, Event, Session, Target, TermSize, check_host_key,
};
use tokio::net::TcpListener;

const USER: &str = "tern";
const PASSWORD: &str = "correct horse battery";

#[derive(Clone)]
struct TestServer {
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
async fn start_server(host_key: PrivateKey, client_key: PublicKey) -> u16 {
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

fn request(port: u16, credentials: Vec<Credential>, trust: Option<PublicKey>) -> ConnectRequest {
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
    }
}

async fn next(session: &mut Session) -> Event {
    tokio::time::timeout(Duration::from_secs(10), session.next_event())
        .await
        .expect("timed out waiting for the session")
        .expect("session ended without an event")
}

/// Collects output until `needle` shows up.
async fn expect_output(session: &mut Session, needle: &str) {
    let mut seen = String::new();
    while !seen.contains(needle) {
        match next(session).await {
            Event::Output(bytes) => seen.push_str(&String::from_utf8_lossy(&bytes)),
            other => panic!("expected output containing {needle:?}, got {other:?} after {seen:?}"),
        }
    }
}

// One test, so the process-wide environment it sets isn't shared with others.
#[tokio::test(flavor = "multi_thread")]
async fn session_lifecycle() {
    let scratch = std::env::temp_dir().join(format!("tern-ssh-test-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    // SAFETY: this is the only test in this binary, and it sets these before
    // any session (and so any other thread reading them) exists.
    unsafe {
        std::env::set_var("TERN_DATA_DIR", scratch.join("data"));
        std::env::set_var("TERN_SYSTEM_KNOWN_HOSTS", scratch.join("no-such-file"));
    }

    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let key_file = scratch.join("id_ed25519");
    std::fs::write(&key_file, client_key.to_openssh(LineEnding::LF).unwrap()).unwrap();
    let port = start_server(host_key.clone(), client_key.public_key().clone()).await;
    let password = || vec![Credential::Password(PASSWORD.into())];

    // An unknown host key stops the connection and is reported for review.
    let mut session = Session::open(request(port, password(), None));
    let offered = match next(&mut session).await {
        Event::Failed(ConnectError::UnknownHostKey {
            host,
            port: offered_port,
            key,
            fingerprint,
        }) => {
            assert_eq!((host.as_str(), offered_port), ("127.0.0.1", port));
            assert!(fingerprint.starts_with("SHA256:"));
            *key
        }
        other => panic!("expected an unknown host key, got {other:?}"),
    };
    assert_eq!(&offered, host_key.public_key());

    // Trusting it connects, and the shell works both ways.
    let mut session = Session::open(request(port, password(), Some(offered)));
    assert!(matches!(next(&mut session).await, Event::Connected));
    expect_output(&mut session, "ready").await;
    session.write(b"hello".to_vec());
    expect_output(&mut session, "echo:hello").await;
    session.resize(TermSize {
        cols: 120,
        rows: 40,
    });
    expect_output(&mut session, "size:120x40").await;
    session.write(b"exit".to_vec());
    loop {
        match next(&mut session).await {
            Event::Output(_) => {}
            Event::Closed { exit_status } => {
                assert_eq!(exit_status, Some(7));
                break;
            }
            other => panic!("expected the session to close, got {other:?}"),
        }
    }

    // Through a jump host (the same server, forwarding to itself): sign in
    // there, then reach the target over the forwarded channel.
    let mut through = request(port, password(), None);
    through.jump = Some((through.target.clone(), password()));
    let mut session = Session::open(through);
    let event = next(&mut session).await;
    assert!(matches!(event, Event::Connected), "{event:?}");
    expect_output(&mut session, "ready").await;
    session.write(b"via jump".to_vec());
    expect_output(&mut session, "echo:via jump").await;
    drop(session);

    // A jump host that refuses the sign-in is named in the error.
    let mut through = request(port, password(), None);
    through.jump = Some((
        through.target.clone(),
        vec![Credential::Password("wrong".into())],
    ));
    let mut session = Session::open(through);
    match next(&mut session).await {
        Event::Failed(ConnectError::Other(message)) => {
            assert!(
                message.starts_with("can't sign in to 127.0.0.1"),
                "{message}"
            )
        }
        other => panic!("expected the jump sign-in to fail, got {other:?}"),
    }

    // The trusted key was saved, so a key-file login needs no prompt; a bad
    // password is tried first and skipped.
    let credentials = vec![
        Credential::Password("wrong".into()),
        Credential::KeyFile {
            path: key_file,
            passphrase: None,
        },
    ];
    let mut session = Session::open(request(port, credentials, None));
    assert!(matches!(next(&mut session).await, Event::Connected));
    expect_output(&mut session, "ready").await;
    drop(session);

    // Nothing that works: authentication fails and says what was tried.
    let mut session = Session::open(request(
        port,
        vec![Credential::Password("wrong".into())],
        None,
    ));
    match next(&mut session).await {
        Event::Failed(ConnectError::AuthFailed { tried }) => assert_eq!(tried, ["password"]),
        other => panic!("expected authentication to fail, got {other:?}"),
    }

    // A different key for a known host is refused outright, even if "trusted".
    let impostor = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let changed = check_host_key(
        "127.0.0.1",
        port,
        impostor.public_key(),
        Some(impostor.public_key()),
    );
    assert!(
        matches!(changed, Err(ConnectError::HostKeyChanged { .. })),
        "{changed:?}"
    );

    let _ = std::fs::remove_dir_all(&scratch);
}
