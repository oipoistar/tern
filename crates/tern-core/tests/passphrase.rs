//! End-to-end check of an encrypted key file: it's offered to the server
//! first, and its passphrase is asked only once the server says it would
//! take the key, as OpenSSH does; a wrong one is asked again.

mod common;

use common::*;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey};
use tern_core::ssh::{ConnectError, Credential, Event, Session, check_host_key};

#[tokio::test(flavor = "multi_thread")]
async fn asks_for_a_passphrase_only_when_the_key_would_do() {
    let scratch = scratch("passphrase");
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let locked = scratch.join("id_locked");
    client_key
        .encrypt(&mut rand::rng(), "open sesame")
        .unwrap()
        .write_openssh_file(&locked, LineEnding::LF)
        .unwrap();
    let credentials = vec![Credential::KeyFile {
        path: locked,
        passphrase: None,
    }];
    let start = |client: PrivateKey| {
        let host_key = host_key.clone();
        async move {
            let port = start_server(host_key.clone(), client.public_key().clone()).await;
            let key = host_key.public_key();
            check_host_key("127.0.0.1", port, key, Some(key)).unwrap();
            port
        }
    };

    // The server would take the key: its passphrase is asked, again after
    // a wrong one.
    let port = start(client_key).await;
    let mut session = Session::open(request(port, credentials.clone(), None));
    let mut asked = Vec::new();
    loop {
        match next(&mut session).await {
            Event::Prompts(prompts) => {
                asked.extend(prompts.questions);
                let answer = if asked.len() == 1 {
                    "wrong"
                } else {
                    "open sesame"
                };
                session.answer(vec![answer.into()]);
            }
            Event::Connected => break,
            other => panic!("expected to sign in, got {other:?} after {asked:?}"),
        }
    }
    let question = ("Passphrase for id_locked:".to_string(), false);
    assert_eq!(asked, [question.clone(), question]);

    // One that wouldn't: nothing is asked.
    let stranger = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let port = start(stranger).await;
    let mut session = Session::open(request(port, credentials, None));
    match next(&mut session).await {
        Event::Failed(ConnectError::AuthFailed { .. }) => {}
        other => panic!("expected to be refused, unasked: {other:?}"),
    }
}
