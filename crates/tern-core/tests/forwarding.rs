//! End-to-end checks of port forwarding against the in-process server:
//! local (`-L`), dynamic SOCKS (`-D`) and remote (`-R`) rules carry bytes
//! to an echo server, stop when switched off, and say why when their port
//! is taken.

mod common;

use common::*;
use russh::keys::{Algorithm, PrivateKey};
use tern_core::forward::{Forward, ForwardKind, ForwardState};
use tern_core::ssh::{Credential, Event, Session, check_host_key};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// A port nothing listens on right now.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// A server on 127.0.0.1 that sends back whatever it gets.
async fn echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            tokio::spawn(async move {
                let (mut read, mut write) = tcp.split();
                let _ = tokio::io::copy(&mut read, &mut write).await;
            });
        }
    });
    port
}

/// Sends `word` down `stream` and expects it back.
async fn round_trip(stream: &mut TcpStream, word: &[u8]) {
    stream.write_all(word).await.unwrap();
    let mut back = vec![0u8; word.len()];
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stream.read_exact(&mut back),
    )
    .await
    .expect("the echo came back in time")
    .unwrap();
    assert_eq!(back, word);
}

fn rule(kind: ForwardKind, listen: u16, to: u16) -> Forward {
    Forward {
        kind,
        listen_host: "127.0.0.1".into(),
        listen_port: listen,
        to_host: if kind == ForwardKind::Dynamic {
            String::new()
        } else {
            "127.0.0.1".into()
        },
        to_port: if kind == ForwardKind::Dynamic { 0 } else { to },
        on: true,
    }
}

/// Waits until `forward` reports a state, and returns it.
async fn state_of(session: &mut Session, forward: &Forward) -> ForwardState {
    let id = forward.id();
    wait_for(session, |event| match event {
        Event::Forwarding { forward, state } if forward.id() == id => Some(state),
        _ => None,
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn forwards_ports_three_ways() {
    let scratch = scratch("forward-test");
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let port = start_server(host_key.clone(), client_key.public_key().clone()).await;
    check_host_key(
        "127.0.0.1",
        port,
        host_key.public_key(),
        Some(host_key.public_key()),
    )
    .unwrap();
    let echo = echo_server().await;

    // A local rule starts with the session.
    let local = rule(ForwardKind::Local, free_port(), echo);
    let mut request = request(port, vec![Credential::Password(PASSWORD.into())], None);
    request.forwards = vec![local.clone()];
    let mut session = Session::open(request);
    assert!(matches!(next(&mut session).await, Event::Connected));
    assert_eq!(state_of(&mut session, &local).await, ForwardState::Running);
    let mut through = TcpStream::connect(("127.0.0.1", local.listen_port))
        .await
        .unwrap();
    round_trip(&mut through, b"over -L").await;

    // A dynamic rule speaks SOCKS5 and goes wherever each client asks.
    let dynamic = rule(ForwardKind::Dynamic, free_port(), 0);
    session.forward(dynamic.clone(), true);
    assert_eq!(
        state_of(&mut session, &dynamic).await,
        ForwardState::Running
    );
    let mut socks = TcpStream::connect(("127.0.0.1", dynamic.listen_port))
        .await
        .unwrap();
    socks.write_all(&[5, 1, 0]).await.unwrap();
    let mut choice = [0u8; 2];
    socks.read_exact(&mut choice).await.unwrap();
    assert_eq!(choice, [5, 0]);
    let mut connect = vec![5, 1, 0, 1, 127, 0, 0, 1];
    connect.extend_from_slice(&echo.to_be_bytes());
    socks.write_all(&connect).await.unwrap();
    let mut reply = [0u8; 10];
    socks.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply[..2], [5, 0], "the SOCKS connection went through");
    round_trip(&mut socks, b"over -D").await;

    // A remote rule: the server listens and sends connections back here.
    let remote = rule(ForwardKind::Remote, free_port(), echo);
    session.forward(remote.clone(), true);
    assert_eq!(state_of(&mut session, &remote).await, ForwardState::Running);
    let mut back = TcpStream::connect(("127.0.0.1", remote.listen_port))
        .await
        .unwrap();
    round_trip(&mut back, b"over -R").await;

    // Switched off, the local port closes; a taken port says so.
    session.forward(local.clone(), false);
    assert_eq!(state_of(&mut session, &local).await, ForwardState::Stopped);
    let mut refused = false;
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", local.listen_port))
            .await
            .is_err()
        {
            refused = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(refused, "nothing listens once the rule is off");
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let clash = rule(ForwardKind::Local, taken.local_addr().unwrap().port(), echo);
    session.forward(clash.clone(), true);
    assert!(matches!(
        state_of(&mut session, &clash).await,
        ForwardState::Failed(_)
    ));

    drop(session);

    // Rules can run without a shell: the session only carries them.
    let alone = rule(ForwardKind::Local, free_port(), echo);
    let mut headless = common::request(port, vec![Credential::Password(PASSWORD.into())], None);
    headless.forwards = vec![alone.clone()];
    headless.shell = false;
    let mut session = Session::open(headless);
    assert!(matches!(next(&mut session).await, Event::Connected));
    assert_eq!(state_of(&mut session, &alone).await, ForwardState::Running);
    let mut through = TcpStream::connect(("127.0.0.1", alone.listen_port))
        .await
        .unwrap();
    round_trip(&mut through, b"no shell needed").await;
    session.close();
    let ended = wait_for(&mut session, |event| match event {
        Event::Closed { exit_status } => Some(exit_status),
        Event::Lost => Some(Some(u32::MAX)),
        _ => None,
    })
    .await;
    assert_eq!(ended, None, "closed, not lost");

    let _ = std::fs::remove_dir_all(&scratch);
}
