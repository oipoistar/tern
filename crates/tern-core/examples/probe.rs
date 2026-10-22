//! Connects with Tern's SSH stack, runs one line in the remote shell and
//! prints what comes back. Handy for debugging without the app:
//!
//! ```text
//! cargo run -p tern-core --example probe -- "ssh -p 2222 -i dev/test-ssh/id_ed25519 tern@127.0.0.1" "echo hi" [--trust]
//! ```
//!
//! `--trust` accepts an unknown host key (and saves it to Tern's known hosts,
//! so point `TERN_DATA_DIR` somewhere disposable when experimenting).

use std::time::Duration;

use tern_core::ssh::{
    ConnectError, ConnectRequest, Credential, Event, Session, TermSize, default_credentials,
    parse_command,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args
        .first()
        .map(String::as_str)
        .unwrap_or("ssh -p 2222 -i dev/test-ssh/id_ed25519 tern@127.0.0.1");
    let line = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "echo hello from tern".into());
    let trust_unknown = args.iter().any(|arg| arg == "--trust");

    let (target, key) = parse_command(command)?;
    let credentials = match key {
        Some(path) => vec![Credential::KeyFile {
            path,
            passphrase: None,
        }],
        None => default_credentials(),
    };
    let mut trust = None;
    loop {
        let request = ConnectRequest {
            target: target.clone(),
            credentials: credentials.clone(),
            size: TermSize {
                cols: 100,
                rows: 30,
            },
            trust: trust.clone(),
        };
        let mut session = Session::open(request);
        match session.next_event().await {
            Some(Event::Connected) => {
                eprintln!(
                    "connected to {}:{} as {}",
                    target.host, target.port, target.user
                );
                // Give the shell a moment to print its prompt, then type.
                let typed = tokio::time::sleep(Duration::from_millis(2500));
                let deadline = tokio::time::sleep(Duration::from_secs(6));
                tokio::pin!(typed, deadline);
                let mut sent = false;
                loop {
                    tokio::select! {
                        _ = &mut typed, if !sent => {
                            sent = true;
                            session.write(format!("{line}\r").into_bytes());
                        }
                        _ = &mut deadline => break,
                        event = session.next_event() => match event {
                            Some(Event::Output(bytes)) => {
                                // Answer cursor-position queries like a terminal would;
                                // Windows' ConPTY waits for one before showing anything.
                                if bytes.windows(4).any(|w| w == b"\x1b[6n") {
                                    session.write(b"\x1b[1;1R".to_vec());
                                }
                                print!("{}", String::from_utf8_lossy(&bytes));
                            }
                            Some(Event::Closed { exit_status }) => {
                                eprintln!("\nclosed, exit status {exit_status:?}");
                                return Ok(());
                            }
                            other => eprintln!("\n{other:?}"),
                        }
                    }
                }
                eprintln!("\n(done)");
                return Ok(());
            }
            Some(Event::Failed(ConnectError::UnknownHostKey { key, fingerprint }))
                if trust_unknown && trust.is_none() =>
            {
                eprintln!("trusting host key {fingerprint}");
                trust = Some(*key);
            }
            Some(Event::Failed(error)) => anyhow::bail!("{error}"),
            other => anyhow::bail!("unexpected {other:?}"),
        }
    }
}
