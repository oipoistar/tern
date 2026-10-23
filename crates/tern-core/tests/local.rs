//! End-to-end check of a local terminal: this computer's shell runs on a
//! pseudo-terminal, echoes what's typed and reports its exit like an SSH
//! session does, even when a job it started holds the terminal open.

mod common;

use std::time::Duration;

use common::next_within;
use tern_core::ssh::{Event, Session, TermSize};

/// A shell can take a while to start on a cold machine, as CI runners are.
const START: Duration = Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread")]
async fn runs_this_computers_shell() {
    let mut session = Session::local(TermSize {
        cols: 100,
        rows: 30,
    });
    let event = next_within(&mut session, START).await;
    assert!(matches!(event, Event::Connected), "{event:?}");

    // Windows' ConPTY asks where the cursor is and waits for the answer, as
    // a real terminal gives it. A question can arrive split over two reads,
    // so they're counted in everything seen.
    let mut seen = String::new();
    let mut answered = 0;
    let mut typed = false;
    loop {
        match next_within(&mut session, START).await {
            Event::Output(bytes) => {
                seen.push_str(&String::from_utf8_lossy(&bytes));
                let asked = seen.matches("\x1b[6n").count();
                for _ in answered..asked {
                    session.write(b"\x1b[1;1R".to_vec());
                }
                answered = asked;
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
        match next_within(&mut session, START).await {
            Event::Output(_) => {}
            Event::Closed { exit_status } => {
                assert_eq!(exit_status, Some(0));
                break;
            }
            other => panic!("expected the shell to exit, got {other:?}"),
        }
    }
}

/// A job left running holds the terminal open after the shell exits, as a
/// server started with `&` does; the session ends with the shell anyway.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn ends_with_the_shell_while_a_job_runs_on() {
    let mut session = Session::local(TermSize {
        cols: 100,
        rows: 30,
    });
    let event = next_within(&mut session, START).await;
    assert!(matches!(event, Event::Connected), "{event:?}");

    // Started from a subshell, the job isn't the shell's own, so no shell
    // asks about it or hangs it up on the way out.
    let mut seen = String::new();
    let mut typed = false;
    while !seen.contains("tern-job-42") {
        match next_within(&mut session, START).await {
            Event::Output(bytes) => {
                seen.push_str(&String::from_utf8_lossy(&bytes));
                if !typed {
                    session.write(b"(sleep 60 &); echo tern-job-$((6*7))\r".to_vec());
                    typed = true;
                }
            }
            other => panic!("expected output, got {other:?} after {seen:?}"),
        }
    }

    // The session ends long before the job does.
    session.write(b"exit\r".to_vec());
    loop {
        match next_within(&mut session, Duration::from_secs(20)).await {
            Event::Output(_) => {}
            Event::Closed { exit_status } => {
                assert_eq!(exit_status, Some(0));
                break;
            }
            other => panic!("expected the shell to exit, got {other:?}"),
        }
    }
}
