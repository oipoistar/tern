//! End-to-end check that a host key the user accepts vouches only for the
//! host they accepted it for: of two servers sharing one key, as cloned
//! machines do, each is asked about in turn.

mod common;

use common::*;
use russh::keys::{Algorithm, PrivateKey};
use tern_core::ssh::{ConnectError, Credential, Event, Session, Target, Trusted};

#[tokio::test(flavor = "multi_thread")]
async fn trusts_a_key_for_its_host_only() {
    let _scratch = scratch("trust");
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let first = start_server(host_key.clone(), client_key.public_key().clone()).await;
    let second = start_server(host_key.clone(), client_key.public_key().clone()).await;
    let password = || vec![Credential::Password(PASSWORD.into())];
    let trusted = |port| {
        Some(Trusted {
            host: "127.0.0.1".into(),
            port,
            key: host_key.public_key().clone(),
        })
    };

    // Through the first to the second, with the key accepted for the first:
    // the second still has to be asked about.
    let mut through = request(second, password(), None);
    through.jumps = vec![(
        Target {
            host: "127.0.0.1".into(),
            port: first,
            user: USER.into(),
        },
        password(),
    )];
    through.trust = trusted(first);
    let mut session = Session::open(through.clone());
    match next(&mut session).await {
        Event::Failed(ConnectError::UnknownHostKey { port, .. }) => assert_eq!(port, second),
        other => panic!("the second wasn't asked about: {other:?}"),
    }

    // Accepted for the second too, both are known.
    through.trust = trusted(second);
    let mut session = Session::open(through);
    let event = next(&mut session).await;
    assert!(matches!(event, Event::Connected), "{event:?}");
}
