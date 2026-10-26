//! End-to-end check of OpenSSH certificates against the in-process server:
//! a host certificate is trusted through an `@cert-authority` line instead
//! of the host's own key, and a user certificate beside a key file signs in
//! where the bare key wouldn't.

mod common;

use common::*;
use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::certificate::{Builder, CertType};
use russh::keys::{Algorithm, Certificate, PrivateKey};
use tern_core::ssh::{ConnectError, Credential, Event, Session};

fn key() -> PrivateKey {
    PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap()
}

/// A certificate for `key` of `kind`, for `principal`, valid for a day
/// either side of now, signed by `ca`.
fn certificate(key: &PrivateKey, ca: &PrivateKey, kind: CertType, principal: &str) -> Certificate {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut builder = Builder::new(
        [9u8; 16],
        key.public_key().key_data().clone(),
        now - 86_400,
        now + 86_400,
    )
    .unwrap();
    builder.cert_type(kind).unwrap();
    builder.valid_principal(principal).unwrap();
    builder.sign(ca).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn signs_in_and_trusts_hosts_with_certificates() {
    let scratch = scratch("cert-test");
    let ca = key();
    let host_key = key();
    let client_key = key();
    // The server knows no bare key of ours: only the certificate can work.
    let port = start_server_with(
        host_key.clone(),
        key().public_key().clone(),
        Setup {
            certificate: Some(certificate(&host_key, &ca, CertType::Host, "127.0.0.1")),
            user_ca: Some(ca.public_key().clone()),
            ..Setup::default()
        },
    )
    .await;
    let password = || vec![Credential::Password(PASSWORD.into())];

    // No authority named yet: the host's own key is unknown.
    let mut session = Session::open(request(port, password(), None));
    let event = next(&mut session).await;
    assert!(
        matches!(event, Event::Failed(ConnectError::UnknownHostKey { .. })),
        "{event:?}"
    );

    // Once known_hosts names the authority for the host, its certificate
    // is enough.
    let known_hosts = tern_core::paths::known_hosts();
    std::fs::create_dir_all(known_hosts.parent().unwrap()).unwrap();
    let line = format!(
        "@cert-authority [127.0.0.1]:{port} {}\n",
        ca.public_key().to_openssh().unwrap()
    );
    std::fs::write(&known_hosts, line).unwrap();
    let mut session = Session::open(request(port, password(), None));
    let event = next(&mut session).await;
    assert!(matches!(event, Event::Connected), "{event:?}");
    drop(session);

    // A certificate for another host isn't, and isn't offered for trust.
    let other_key = key();
    let elsewhere = start_server_with(
        other_key.clone(),
        client_key.public_key().clone(),
        Setup {
            certificate: Some(certificate(&other_key, &ca, CertType::Host, "elsewhere")),
            ..Setup::default()
        },
    )
    .await;
    let line = format!(
        "@cert-authority [127.0.0.1]:{elsewhere} {}\n",
        ca.public_key().to_openssh().unwrap()
    );
    let mut known = std::fs::read_to_string(&known_hosts).unwrap();
    known.push_str(&line);
    std::fs::write(&known_hosts, known).unwrap();
    let mut session = Session::open(request(elsewhere, password(), None));
    match next(&mut session).await {
        Event::Failed(ConnectError::Other(message)) => {
            assert_eq!(message, "the host certificate isn't for 127.0.0.1")
        }
        other => panic!("expected the certificate to be refused, got {other:?}"),
    }

    // A user certificate beside the key file signs in; without it, the
    // bare key is refused.
    let key_file = scratch.join("id_ed25519");
    std::fs::write(&key_file, client_key.to_openssh(LineEnding::LF).unwrap()).unwrap();
    let cert_file = scratch.join("id_ed25519-cert.pub");
    let user_cert = certificate(&client_key, &ca, CertType::User, USER);
    std::fs::write(&cert_file, user_cert.to_openssh().unwrap()).unwrap();
    let with_key = || {
        vec![Credential::KeyFile {
            path: key_file.clone(),
            passphrase: None,
        }]
    };
    let mut session = Session::open(request(port, with_key(), None));
    let event = next(&mut session).await;
    assert!(matches!(event, Event::Connected), "{event:?}");
    drop(session);
    std::fs::remove_file(&cert_file).unwrap();
    let mut session = Session::open(request(port, with_key(), None));
    let event = next(&mut session).await;
    assert!(
        matches!(event, Event::Failed(ConnectError::AuthFailed { .. })),
        "{event:?}"
    );

    // A revoked authority vouches for nothing.
    let trusting = std::fs::read_to_string(&known_hosts).unwrap();
    let revoking = format!(
        "{trusting}@revoked [127.0.0.1]:{port} {}\n",
        ca.public_key().to_openssh().unwrap()
    );
    std::fs::write(&known_hosts, revoking).unwrap();
    let mut session = Session::open(request(port, password(), None));
    match next(&mut session).await {
        Event::Failed(ConnectError::Other(message)) => {
            assert!(
                message.starts_with("the host certificate is revoked by"),
                "{message}"
            )
        }
        other => panic!("expected the authority to be revoked, got {other:?}"),
    }

    // Nor does one while a known_hosts file can't be read: it might revoke
    // the authority the other trusts.
    std::fs::write(&known_hosts, trusting).unwrap();
    let system = tern_core::paths::system_known_hosts().unwrap();
    std::fs::create_dir_all(&system).unwrap();
    let mut session = Session::open(request(port, password(), None));
    match next(&mut session).await {
        Event::Failed(ConnectError::Other(message)) => {
            assert!(message.starts_with("can't read"), "{message}")
        }
        other => panic!("expected an unreadable known_hosts to stop it, got {other:?}"),
    }
    std::fs::remove_dir(&system).unwrap();
    let _ = std::fs::remove_dir_all(&scratch);
}
