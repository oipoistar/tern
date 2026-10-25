//! End-to-end check that a server can't open channels back to Tern that it
//! was never offered: X11, sessions, and connections or sockets out of
//! this computer are refused, as an unasked-for agent channel is.

mod common;

use common::*;
use russh::keys::{Algorithm, PrivateKey};
use tern_core::ssh::{Credential, Event, Session, check_host_key};

#[tokio::test(flavor = "multi_thread")]
async fn refuses_channels_it_never_asked_for() {
    let scratch = scratch("channels");
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let port = start_server_with(
        host_key.clone(),
        client_key.public_key().clone(),
        Setup {
            probe_channels: true,
            ..Setup::default()
        },
    )
    .await;
    check_host_key(
        "127.0.0.1",
        port,
        host_key.public_key(),
        Some(host_key.public_key()),
    )
    .unwrap();
    let mut session = Session::open(request(
        port,
        vec![Credential::Password(PASSWORD.into())],
        None,
    ));
    let mut seen = String::new();
    while !seen.contains("forwarded-socket:") {
        match next(&mut session).await {
            Event::Output(bytes) => seen.push_str(&String::from_utf8_lossy(&bytes)),
            Event::Connected => {}
            other => panic!("expected output, got {other:?}"),
        }
    }
    let said = seen
        .lines()
        .find(|line| line.starts_with("x11:"))
        .expect("the server's report");
    assert_eq!(
        said,
        "x11:refused session:refused tcpip:refused forwarded-tcpip:refused \
         socket:refused forwarded-socket:refused"
    );
    drop(session);
    let _ = std::fs::remove_dir_all(&scratch);
}
