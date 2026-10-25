//! End-to-end check that a host can't pass off a key of another type as a
//! new one: Tern asks for the type it already knows, and a server that
//! shows another type for a known host is refused as a changed key.

mod common;

use common::*;
use russh::keys::ssh_key::EcdsaCurve;
use russh::keys::{Algorithm, PrivateKey};
use tern_core::ssh::{ConnectError, Credential, Event, Session};

#[tokio::test(flavor = "multi_thread")]
async fn a_key_of_another_type_is_a_changed_key() {
    let scratch = scratch("key-types");
    let known_hosts = scratch.join("ssh").join("known_hosts");
    std::fs::create_dir_all(known_hosts.parent().unwrap()).unwrap();
    let ecdsa = Algorithm::Ecdsa {
        curve: EcdsaCurve::NistP256,
    };
    let random = |algorithm| PrivateKey::random(&mut rand::rng(), algorithm).unwrap();
    let client_key = random(Algorithm::Ed25519);
    let password = || vec![Credential::Password(PASSWORD.into())];
    let trust = |port: u16, key: &PrivateKey| {
        std::fs::write(
            &known_hosts,
            format!(
                "[127.0.0.1]:{port} {}\n",
                key.public_key().to_openssh().unwrap()
            ),
        )
        .unwrap()
    };

    // A server with an Ed25519 key and an ECDSA one, known by its ECDSA
    // key: asked for that type first, it shows it, and is let in. (Asked
    // in the usual order, it would show its Ed25519 key.)
    let (ed25519, known) = (random(Algorithm::Ed25519), random(ecdsa.clone()));
    let port = start_server_with(
        ed25519,
        client_key.public_key().clone(),
        Setup {
            more_keys: vec![known.clone()],
            ..Setup::default()
        },
    )
    .await;
    trust(port, &known);
    let mut session = Session::open(request(port, password(), None));
    let event = next(&mut session).await;
    assert!(matches!(event, Event::Connected), "{event:?}");
    drop(session);

    // A server known by an Ed25519 key that shows only an ECDSA one, as an
    // impostor in the middle would: refused, never offered for trust.
    let (expected, shown) = (random(Algorithm::Ed25519), random(ecdsa));
    let port = start_server(shown.clone(), client_key.public_key().clone()).await;
    trust(port, &expected);
    let mut session = Session::open(request(port, password(), Some(shown.public_key().clone())));
    match next(&mut session).await {
        Event::Failed(ConnectError::HostKeyChanged { file, line, .. }) => {
            assert_eq!((file, line), (known_hosts.clone(), 1))
        }
        other => panic!("expected a changed key, got {other:?}"),
    }
    // Nothing was learned for it.
    assert_eq!(
        std::fs::read_to_string(&known_hosts)
            .unwrap()
            .lines()
            .count(),
        1
    );
    let _ = std::fs::remove_dir_all(&scratch);
}
