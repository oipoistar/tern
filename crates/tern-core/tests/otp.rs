//! End-to-end check of keyboard-interactive sign-in against a server that
//! takes no plain passwords and wants a one-time code after the password:
//! the password question is answered from the password the session has,
//! the code is asked for.

mod common;

use common::*;
use russh::keys::{Algorithm, PrivateKey};
use tern_core::ssh::{ConnectError, Credential, Event, Prompts, Session, check_host_key};

/// The questions the session asks next.
async fn questions(session: &mut Session) -> Prompts {
    match next(session).await {
        Event::Prompts(prompts) => prompts,
        other => panic!("expected questions, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn answers_the_servers_questions() {
    let scratch = scratch("otp-test");
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let port = start_server_with(
        host_key.clone(),
        client_key.public_key().clone(),
        Setup {
            otp: Some("123456".into()),
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
    let password = || vec![Credential::Password(PASSWORD.into())];

    // Only the code is asked; the password question was answered.
    let mut session = Session::open(request(port, password(), None));
    let prompts = questions(&mut session).await;
    assert_eq!(prompts.from, "tern@127.0.0.1");
    assert_eq!(prompts.instructions, "Use the code from your phone.");
    assert_eq!(
        prompts.questions,
        [("Verification code:".to_string(), true)]
    );
    session.answer(vec!["123456".into()]);
    let event = next(&mut session).await;
    assert!(matches!(event, Event::Connected), "{event:?}");
    expect_output(&mut session, "ready").await;
    drop(session);

    // A wrong code is refused.
    let mut session = Session::open(request(port, password(), None));
    questions(&mut session).await;
    session.answer(vec!["000000".into()]);
    let event = next(&mut session).await;
    assert!(
        matches!(event, Event::Failed(ConnectError::AuthFailed { .. })),
        "{event:?}"
    );

    // Without a password the attempt stops at the password question, so
    // the user is asked for one the usual way.
    let mut session = Session::open(request(port, Vec::new(), None));
    match next(&mut session).await {
        Event::Failed(ConnectError::AuthFailed { tried }) => {
            assert_eq!(tried, ["keyboard-interactive"])
        }
        other => panic!("expected the sign-in to fail, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&scratch);
}
