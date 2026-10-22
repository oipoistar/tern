//! End-to-end checks of `tern_core::ssh` against a russh server running in
//! this process: host-key trust, password and key auth, data, resize, close,
//! and connecting through a jump host.

mod common;

use common::*;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey};
use tern_core::ssh::{ConnectError, Credential, Event, Session, TermSize, check_host_key};

// One test, so the process-wide environment it sets isn't shared with others.
#[tokio::test(flavor = "multi_thread")]
async fn session_lifecycle() {
    let scratch = scratch("ssh-test");

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
