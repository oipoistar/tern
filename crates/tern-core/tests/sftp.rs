//! End-to-end checks of SFTP over a session, against the in-process server
//! serving a scratch folder: listing with item counts, downloading and
//! uploading with progress, and never replacing a file.

mod common;

use common::*;
use russh::keys::{Algorithm, PrivateKey};
use tern_core::sftp::{Entry, Progress, list_local};
use tern_core::ssh::{Credential, Event, Session, check_host_key};

#[tokio::test(flavor = "multi_thread")]
async fn copies_files_both_ways() {
    let scratch = scratch("sftp-test");
    let (served, here) = (scratch.join("served"), scratch.join("here"));
    std::fs::create_dir_all(served.join("docs")).unwrap();
    std::fs::create_dir_all(&here).unwrap();
    std::fs::write(served.join("hello.txt"), "hello over sftp").unwrap();
    std::fs::write(served.join("docs").join("a.md"), "a").unwrap();
    std::fs::write(served.join("docs").join("b.md"), "b").unwrap();

    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let port = start_server_with_sftp(
        host_key.clone(),
        client_key.public_key().clone(),
        Some(served.clone()),
    )
    .await;
    check_host_key(
        "127.0.0.1",
        port,
        host_key.public_key(),
        Some(host_key.public_key()),
    )
    .unwrap();
    let session = Session::open(request(
        port,
        vec![Credential::Password(PASSWORD.into())],
        None,
    ));
    let (input, mut events) = session.into_parts();
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), events.next())
        .await
        .unwrap();
    assert!(matches!(first, Some(Event::Connected)), "{first:?}");

    let sftp = input.open_sftp().await.expect("the server offers SFTP");
    assert_eq!(sftp.home().await.unwrap(), "/");
    let listed = sftp.list("/").await.unwrap();
    assert_eq!(
        listed,
        [
            Entry {
                name: "docs".into(),
                dir: true,
                size: listed[0].size,
                items: Some(2)
            },
            Entry {
                name: "hello.txt".into(),
                dir: false,
                size: 15,
                items: None
            },
        ]
    );

    // Down, twice: the second copy takes the next free name.
    let progress = Progress::default();
    let got = sftp
        .download("/hello.txt", here.clone(), progress.clone())
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&got).unwrap(), "hello over sftp");
    assert_eq!(
        (progress.done(), progress.total(), progress.fraction()),
        (15, 15, 1.)
    );
    let again = sftp
        .download("/hello.txt", here.clone(), Progress::default())
        .await
        .unwrap();
    assert_eq!(again, here.join("hello (2).txt"));

    // Up: a file bigger than one chunk lands whole in the chosen folder.
    let big: Vec<u8> = (0..300_000u32).map(|n| (n % 251) as u8).collect();
    let local = here.join("big.bin");
    std::fs::write(&local, &big).unwrap();
    let progress = Progress::default();
    let put = sftp
        .upload(local.clone(), "/docs", progress.clone())
        .await
        .unwrap();
    assert_eq!(put, "/docs/big.bin");
    assert_eq!(
        std::fs::read(served.join("docs").join("big.bin")).unwrap(),
        big
    );
    assert_eq!(progress.done(), big.len() as u64);
    let put = sftp
        .upload(local, "/docs", Progress::default())
        .await
        .unwrap();
    assert_eq!(put, "/docs/big (2).bin");
    assert_eq!(sftp.list("/").await.unwrap()[0].items, Some(4));

    // This computer lists the same way.
    let names: Vec<String> = list_local(&here)
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names, ["big.bin", "hello (2).txt", "hello.txt"]);

    drop((input, events));
    let _ = std::fs::remove_dir_all(&scratch);
}
