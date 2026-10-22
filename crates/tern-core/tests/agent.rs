//! End-to-end check of agent forwarding against the in-process server: with
//! forwarding on, the server lists the keys of an agent on this computer (a
//! stand-in the test serves where `SSH_AUTH_SOCK` points); with it off, the
//! server's attempt to reach the agent is refused.

mod common;

use common::*;
use russh::keys::{Algorithm, PrivateKey};
use tern_core::ssh::{Credential, Event, Session, check_host_key};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// Answers one client the way an agent does, holding a single key.
async fn answer<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, blob: Vec<u8>) {
    loop {
        let mut length = [0u8; 4];
        if stream.read_exact(&mut length).await.is_err() {
            return;
        }
        let mut message = vec![0u8; u32::from_be_bytes(length) as usize];
        if stream.read_exact(&mut message).await.is_err() {
            return;
        }
        // 11 asks for identities; 12 lists them; anything else fails (5).
        let reply = if message.first() == Some(&11) {
            let comment = b"forwarded-key";
            let mut reply = vec![12];
            reply.extend_from_slice(&1u32.to_be_bytes());
            reply.extend_from_slice(&(blob.len() as u32).to_be_bytes());
            reply.extend_from_slice(&blob);
            reply.extend_from_slice(&(comment.len() as u32).to_be_bytes());
            reply.extend_from_slice(comment);
            reply
        } else {
            vec![5]
        };
        let mut framed = (reply.len() as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(&reply);
        if stream.write_all(&framed).await.is_err() {
            return;
        }
    }
}

/// Serves the stand-in agent and points `SSH_AUTH_SOCK` at it.
fn serve_agent(scratch: &std::path::Path, blob: Vec<u8>) {
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;
        let pipe = format!(r"\\.\pipe\tern-test-agent-{}", std::process::id());
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe)
            .unwrap();
        // SAFETY: the binary's only test sets it before any session starts.
        unsafe { std::env::set_var("SSH_AUTH_SOCK", &pipe) };
        tokio::spawn(async move {
            loop {
                if server.connect().await.is_err() {
                    return;
                }
                let next = ServerOptions::new().create(&pipe).unwrap();
                let connected = std::mem::replace(&mut server, next);
                tokio::spawn(answer(connected, blob.clone()));
            }
        });
    }
    #[cfg(unix)]
    {
        let path = scratch.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        // SAFETY: the binary's only test sets it before any session starts.
        unsafe { std::env::set_var("SSH_AUTH_SOCK", &path) };
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(answer(stream, blob.clone()));
            }
        });
    }
    let _ = scratch;
}

/// What the server said about the agent in the shell's output.
async fn said(session: &mut Session) -> &'static str {
    const ANSWERS: [&str; 5] = [
        "agent:forwarded-key",
        "agent:refused",
        "agent:opened",
        "agent:empty",
        "agent:unreadable",
    ];
    let mut seen = String::new();
    loop {
        if let Some(answer) = ANSWERS.iter().find(|answer| seen.contains(**answer)) {
            return answer;
        }
        match next(session).await {
            Event::Output(bytes) => seen.push_str(&String::from_utf8_lossy(&bytes)),
            Event::Connected => {}
            other => panic!("expected output, got {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn forwards_the_agent_only_when_asked() {
    let scratch = scratch("agent-test");
    let held = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    serve_agent(&scratch, held.public_key().to_bytes().unwrap());

    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let port = start_server_probing_agent(host_key.clone(), client_key.public_key().clone()).await;
    check_host_key(
        "127.0.0.1",
        port,
        host_key.public_key(),
        Some(host_key.public_key()),
    )
    .unwrap();
    let password = || vec![Credential::Password(PASSWORD.into())];

    let mut forwarding = request(port, password(), None);
    forwarding.agent_forwarding = true;
    let mut session = Session::open(forwarding);
    assert_eq!(said(&mut session).await, "agent:forwarded-key");
    drop(session);

    let mut session = Session::open(request(port, password(), None));
    assert_eq!(said(&mut session).await, "agent:refused");
    drop(session);
    let _ = std::fs::remove_dir_all(&scratch);
}
