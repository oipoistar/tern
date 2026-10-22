//! End-to-end check that a dropped connection is told apart from a shell
//! that ended: the session runs through a local relay, which is then cut.

mod common;

use common::*;
use russh::keys::{Algorithm, PrivateKey};
use tern_core::ssh::{Credential, Event, Session, check_host_key};
use tokio::net::{TcpListener, TcpStream};

#[tokio::test(flavor = "multi_thread")]
async fn reports_a_dropped_connection_as_lost() {
    let scratch = scratch("lost-test");
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let server = start_server(host_key.clone(), client_key.public_key().clone()).await;

    // A relay in front of the server, standing in for the network.
    let relay = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = relay.local_addr().unwrap().port();
    let cable = tokio::spawn(async move {
        let (mut client, _) = relay.accept().await.unwrap();
        let mut upstream = TcpStream::connect(("127.0.0.1", server)).await.unwrap();
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    });
    check_host_key(
        "127.0.0.1",
        port,
        host_key.public_key(),
        Some(host_key.public_key()),
    )
    .unwrap();

    let password = vec![Credential::Password(PASSWORD.into())];
    let mut session = Session::open(request(port, password, None));
    let event = next(&mut session).await;
    assert!(matches!(event, Event::Connected), "{event:?}");
    expect_output(&mut session, "ready").await;

    // Cut it: the shell never said it ended.
    cable.abort();
    let event = wait_for(&mut session, |event| match event {
        Event::Output(_) => None,
        other => Some(other),
    })
    .await;
    assert!(matches!(event, Event::Lost), "{event:?}");
    let _ = std::fs::remove_dir_all(&scratch);
}
