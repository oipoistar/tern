//! End-to-end check of a local terminal: this computer's shell runs on a
//! pseudo-terminal, echoes what's typed and reports its exit like an SSH
//! session does.

mod common;

use common::next;
use tern_core::ssh::{Event, Session, TermSize};

#[tokio::test(flavor = "multi_thread")]
async fn runs_this_computers_shell() {
    let mut session = Session::local(TermSize {
        cols: 100,
        rows: 30,
    });
    let event = next(&mut session).await;
    assert!(matches!(event, Event::Connected), "{event:?}");

    // Windows' ConPTY asks where the cursor is and waits for the answer, as
    // a real terminal gives it.
    let mut seen = String::new();
    let mut typed = false;
    loop {
        match next(&mut session).await {
            Event::Output(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                if text.contains("\x1b[6n") {
                    session.write(b"\x1b[1;1R".to_vec());
                }
                seen.push_str(&text);
                // Both PowerShell and POSIX shells work this out to 42, so
                // only the shell's answer matches, not the typed line.
                if !typed {
                    session.write(b"echo tern-local-$((6*7))\r".to_vec());
                    typed = true;
                }
                if seen.contains("tern-local-42") {
                    break;
                }
            }
            other => panic!("expected output, got {other:?} after {seen:?}"),
        }
    }

    session.write(b"exit\r".to_vec());
    loop {
        match next(&mut session).await {
            Event::Output(_) => {}
            Event::Closed { exit_status } => {
                assert_eq!(exit_status, Some(0));
                break;
            }
            other => panic!("expected the shell to exit, got {other:?}"),
        }
    }
}
