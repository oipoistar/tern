//! End-to-end checks of SFTP over a session, against the in-process server
//! serving a scratch folder: listing with item counts, downloading and
//! uploading with progress, never replacing a file nor leaving the chosen
//! folder, and renaming, making folders, changing permissions and
//! deleting.

mod common;

use common::*;
use russh::keys::{Algorithm, PrivateKey};
use tern_core::sftp::{Entry, Progress, list_local, mkdir_local, remove_local, rename_local};
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
                items: Some(2),
                mode: listed[0].mode,
            },
            Entry {
                name: "hello.txt".into(),
                dir: false,
                size: 15,
                items: None,
                mode: listed[1].mode,
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

    // The host names what it sends, and a name that would be a path here
    // is refused before anything is written.
    let refused = sftp
        .download("/docs/..", here.clone(), Progress::default())
        .await
        .unwrap_err();
    assert!(refused.contains(".."), "{refused}");
    if cfg!(windows) {
        for (remote, problem) in [(r"/docs/..\escaped.txt", "\\"), ("/c:escaped.txt", ":")] {
            let refused = sftp
                .download(remote, here.clone(), Progress::default())
                .await
                .unwrap_err();
            assert!(refused.contains(problem), "{refused}");
        }
    }
    assert!(!scratch.join("escaped.txt").exists());

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
    if cfg!(unix) {
        // A `\` belongs to a name on Unix, but would part folders on a
        // host running Windows.
        let odd = here.join(r"odd\name.txt");
        std::fs::write(&odd, "odd").unwrap();
        let refused = sftp
            .upload(odd.clone(), "/docs", Progress::default())
            .await
            .unwrap_err();
        assert!(refused.contains('\\'), "{refused}");
        std::fs::remove_file(odd).unwrap();
    }

    // This computer lists the same way.
    let names: Vec<String> = list_local(&here)
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(names, ["big.bin", "hello (2).txt", "hello.txt"]);

    // Managing files on the host: a folder, a rename into it, permissions,
    // and deleting the folder with what it holds.
    sftp.mkdir("/new").await.unwrap();
    assert!(
        sftp.mkdir("/new")
            .await
            .unwrap_err()
            .contains("already exists")
    );
    sftp.rename("/hello.txt", "/new/greeting.txt")
        .await
        .unwrap();
    assert!(served.join("new").join("greeting.txt").exists());
    sftp.upload(here.join("hello.txt"), "/", Progress::default())
        .await
        .unwrap();
    assert!(
        sftp.rename("/hello.txt", "/new/greeting.txt")
            .await
            .unwrap_err()
            .contains("already exists"),
        "nothing is replaced"
    );
    let mode_of = |entries: Vec<Entry>, name: &str| {
        entries
            .into_iter()
            .find(|entry| entry.name == name)
            .and_then(|entry| entry.mode)
            .unwrap()
    };
    sftp.set_mode("/new/greeting.txt", 0o444).await.unwrap();
    assert_eq!(
        mode_of(sftp.list("/new").await.unwrap(), "greeting.txt") & 0o222,
        0
    );
    sftp.set_mode("/new/greeting.txt", 0o644).await.unwrap();
    assert_ne!(
        mode_of(sftp.list("/new").await.unwrap(), "greeting.txt") & 0o200,
        0
    );
    sftp.remove("/new", true).await.unwrap();
    assert!(!served.join("new").exists());
    sftp.remove("/hello.txt", false).await.unwrap();
    assert!(!served.join("hello.txt").exists());
    #[cfg(unix)]
    {
        // A socket's type includes a folder's bit, but it's no folder: a
        // folder holding one deletes whole.
        std::fs::create_dir(served.join("cm")).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(served.join("cm").join("s")).unwrap();
        let listed = sftp.list("/cm").await.unwrap();
        assert!(!listed[0].dir, "{listed:?}");
        sftp.remove("/cm", true).await.unwrap();
        assert!(!served.join("cm").exists());
    }

    // And on this computer.
    mkdir_local(&here.join("box")).unwrap();
    rename_local(&here.join("big.bin"), &here.join("box").join("big.bin")).unwrap();
    assert!(rename_local(&here.join("hello.txt"), &here.join("hello (2).txt")).is_err());
    remove_local(&here.join("box"), true).unwrap();
    assert!(!here.join("box").exists());
    if cfg!(windows) {
        // Joined to a folder, a name with a drive would replace it.
        let refused = mkdir_local(&here.join("q:notes")).unwrap_err();
        assert!(refused.contains(':'), "{refused}");
        let refused = rename_local(&here.join("hello.txt"), &here.join("hello.txt:old"));
        assert!(refused.unwrap_err().contains(':'));
    }

    drop((input, events));
    let _ = std::fs::remove_dir_all(&scratch);
}
