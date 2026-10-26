//! The Tern that `tern --mcp` starts doesn't hold the agent's pipes: an
//! agent reading the relay's output to its end would otherwise wait for as
//! long as that Tern runs.
//!
//! No test harness, since each part is a process of its own: this program
//! is the agent when cargo runs it, the relay when the agent starts it with
//! `--mcp`, and a stand-in for Tern when the relay starts it.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{Ipv4Addr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tern_core::mcp::{self, Answer, Server};

/// Set for the processes the check starts: the relay, and the Tern it
/// starts in turn.
const STARTED: &str = "TERN_RELAY_PIPES";
/// What the stand-in answers every call with.
const ANSWER: &str = "the stand-in's answer";

fn main() {
    if std::env::var_os(STARTED).is_none() {
        agent();
    } else if std::env::args().nth(1).as_deref() == Some("--mcp") {
        mcp::serve_stdio().unwrap();
    } else {
        stand_in();
    }
}

/// Calls a tool through the relay, which starts the stand-in to answer it,
/// then closes the relay and reads the rest of its output.
fn agent() {
    let dir = Scratch::new();
    let mut relay = Command::new(std::env::current_exe().unwrap())
        .arg("--mcp")
        .env(STARTED, "1")
        .env("TERN_DATA_DIR", &dir.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = relay.stdin.take().unwrap();
    let mut output = BufReader::new(relay.stdout.take().unwrap());
    let mut errors = relay.stderr.take().unwrap();
    let call = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "list_sessions", "arguments": {} },
    });
    writeln!(input, "{call}").unwrap();
    let mut reply = String::new();
    output.read_line(&mut reply).unwrap();
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["result"]["content"][0]["text"], ANSWER, "{reply}");

    // The agent is done: the relay ends, and its output with it.
    drop(input);
    let status = relay.wait().unwrap();
    let (ended, end) = mpsc::channel();
    std::thread::spawn(move || {
        let mut said = String::new();
        let _ = std::io::copy(&mut output, &mut std::io::sink());
        let _ = errors.read_to_string(&mut said);
        let _ = ended.send(said);
    });
    let said = end
        .recv_timeout(Duration::from_secs(10))
        .expect("the relay's output stays open while the Tern it started runs");
    assert!(status.success(), "{said}");
    assert!(dir.takes_calls(), "the stand-in quit too soon to tell");
}

/// Tern, as the relay starts it: answers calls until the agent removes the
/// data folder, or for a minute at most.
fn stand_in() {
    let dir = tern_core::paths::data_dir();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (_server, mut incomings) = Server::start(&dir).unwrap();
        tokio::spawn(async move {
            while let Some(incoming) = incomings.next().await {
                incoming.answer(Answer::ok(ANSWER));
            }
        });
        let started = Instant::now();
        while dir.exists() && started.elapsed() < Duration::from_secs(60) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
}

/// The agent's data folder, with the MCP server switched on. Removing it,
/// at the end or when a check fails, tells the stand-in to quit.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("tern-relay-pipes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("settings.json"),
            json!({ "mcp": true }).to_string(),
        )
        .unwrap();
        Self(dir)
    }

    /// Whether a Tern takes calls on the port its `mcp.json` names.
    fn takes_calls(&self) -> bool {
        let Ok(text) = std::fs::read_to_string(self.0.join("mcp.json")) else {
            return false;
        };
        let endpoint: Value = serde_json::from_str(&text).unwrap();
        let port = endpoint["port"].as_u64().unwrap() as u16;
        TcpStream::connect_timeout(&(Ipv4Addr::LOCALHOST, port).into(), Duration::from_secs(1))
            .is_ok()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
