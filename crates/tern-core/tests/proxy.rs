//! End-to-end check of proxies: sessions and reachability checks reach the
//! in-process server through a SOCKS5 and an HTTP `CONNECT` proxy the test
//! runs, and a proxy's refusal is reported in its words.

mod common;

use common::*;
use russh::keys::{Algorithm, PrivateKey};
use tern_core::proxy::{Proxy, ProxyKind};
use tern_core::ssh::{ConnectError, Credential, Event, Session, check_host_key, probe};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// A SOCKS5 proxy that asks for no password and connects by name.
async fn socks5_proxy() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut greeting = [0u8; 3];
                client.read_exact(&mut greeting).await.unwrap();
                client.write_all(&[5, 0]).await.unwrap();
                // Version, command, reserved, address type 3, name length.
                let mut head = [0u8; 5];
                client.read_exact(&mut head).await.unwrap();
                let mut name = vec![0u8; head[4] as usize];
                client.read_exact(&mut name).await.unwrap();
                let mut port = [0u8; 2];
                client.read_exact(&mut port).await.unwrap();
                let host = String::from_utf8(name).unwrap();
                match TcpStream::connect((host.as_str(), u16::from_be_bytes(port))).await {
                    Ok(mut upstream) => {
                        client
                            .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                            .await
                            .unwrap();
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    }
                    // 5: connection refused.
                    Err(_) => {
                        let _ = client.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await;
                    }
                }
            });
        }
    });
    port
}

/// An HTTP proxy that only answers `CONNECT`.
async fn http_proxy() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut byte = [0u8; 1];
                    client.read_exact(&mut byte).await.unwrap();
                    head.push(byte[0]);
                }
                let head = String::from_utf8(head).unwrap();
                let authority = head
                    .strip_prefix("CONNECT ")
                    .and_then(|rest| rest.split(' ').next())
                    .unwrap()
                    .to_string();
                match TcpStream::connect(authority.as_str()).await {
                    Ok(mut upstream) => {
                        client
                            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                            .await
                            .unwrap();
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    }
                    Err(_) => {
                        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    }
                }
            });
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread")]
async fn connects_through_proxies() {
    let scratch = scratch("proxy-test");
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
    let password = || vec![Credential::Password(PASSWORD.into())];
    let socks = Proxy {
        kind: ProxyKind::Socks5,
        host: "127.0.0.1".into(),
        port: socks5_proxy().await,
    };
    let http = Proxy {
        kind: ProxyKind::Http,
        host: "127.0.0.1".into(),
        port: http_proxy().await,
    };

    for proxy in [&socks, &http] {
        let mut through = request(port, password(), None);
        through.proxy = Some(proxy.clone());
        let mut session = Session::open(through);
        let event = next(&mut session).await;
        assert!(matches!(event, Event::Connected), "{proxy}: {event:?}");
        expect_output(&mut session, "ready").await;
        drop(session);

        // Reachability checks go through it too.
        let reached = probe("127.0.0.1", port, Some(proxy.clone())).await.unwrap();
        assert!(reached.banner.starts_with("SSH-"), "{proxy}: {reached:?}");
    }

    // When the proxy can't get through, its answer is the reason.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed_port = closed.local_addr().unwrap().port();
    drop(closed);
    for (proxy, said) in [
        (&socks, "the proxy was refused by the host"),
        (&http, "the proxy answered 502 Bad Gateway"),
    ] {
        let mut through = request(closed_port, password(), None);
        through.proxy = Some(proxy.clone());
        let mut session = Session::open(through);
        match next(&mut session).await {
            Event::Failed(ConnectError::Other(message)) => assert_eq!(message, said),
            other => panic!("{proxy}: expected the proxy's refusal, got {other:?}"),
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
}
