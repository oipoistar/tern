//! Tern's MCP server end to end: an agent's messages through `tern --mcp`'s
//! relay, over the loopback port, to a stand-in for the running Tern, and
//! its answers back.

use std::path::PathBuf;

use serde_json::{Value, json};
use tern_core::mcp::{self, Answer, Relay, Server};
use tokio::io::{
    AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};

/// A data folder of its own, with the MCP server switched on or off.
fn data_dir(name: &str, on: bool) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tern-mcp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    switch(&dir, on);
    dir
}

fn switch(dir: &std::path::Path, on: bool) {
    std::fs::write(dir.join("settings.json"), json!({ "mcp": on }).to_string()).unwrap();
}

/// The agent's end of `tern --mcp`'s standard input and output.
struct Agent {
    to: WriteHalf<DuplexStream>,
    from: Lines<BufReader<ReadHalf<DuplexStream>>>,
    next_id: u64,
}

impl Agent {
    /// Starts the relay for the data folder `dir`.
    fn start(dir: &std::path::Path) -> (Self, tokio::task::JoinHandle<std::io::Result<()>>) {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let (input, output) = tokio::io::split(theirs);
        let serving = tokio::spawn(mcp::serve(input, output, Relay::new(dir.into(), None)));
        let (from, to) = tokio::io::split(ours);
        let agent = Self {
            to,
            from: BufReader::new(from).lines(),
            next_id: 1,
        };
        (agent, serving)
    }

    async fn send(&mut self, message: Value) {
        let line = format!("{message}\n");
        self.to.write_all(line.as_bytes()).await.unwrap();
    }

    async fn reply(&mut self) -> Value {
        let line = self.from.next_line().await.unwrap().expect("a reply");
        serde_json::from_str(&line).unwrap()
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        let reply = self.reply().await;
        assert_eq!(reply["id"], id);
        reply
    }

    /// The text a tool answered, and whether it's an error.
    async fn call(&mut self, name: &str, arguments: Value) -> (String, bool) {
        let reply = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await;
        let result = &reply["result"];
        (
            result["content"][0]["text"].as_str().unwrap().to_string(),
            result["isError"].as_bool().unwrap(),
        )
    }
}

#[tokio::test]
async fn an_agent_reaches_the_running_tern() {
    let dir = data_dir("reach", true);
    let (server, mut incomings) = Server::start(&dir).unwrap();
    assert!(dir.join("mcp.json").exists());
    // Stands in for the app: says what it was asked, and by whom.
    tokio::spawn(async move {
        while let Some(incoming) = incomings.next().await {
            let text = format!("{:?} for {} chars", incoming.call, incoming.client.len());
            incoming.answer(Answer::ok(text));
        }
    });
    let (mut agent, serving) = Agent::start(&dir);

    let init = agent
        .request(
            "initialize",
            json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } }),
        )
        .await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    agent
        .send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
        .await;
    let tools = agent.request("tools/list", json!({})).await;
    assert!(
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "send_input")
    );

    assert_eq!(
        agent.call("list_hosts", json!({ "query": "web" })).await,
        (r#"ListHosts { query: "web" } for 16 chars"#.into(), false)
    );
    // Tern checks the arguments; what's wrong comes back as the tool's error.
    let (text, error) = agent.call("read_session", json!({})).await;
    assert!(error && text.contains("missing field `session`"), "{text}");

    // Switched off, calls say how to switch it on.
    switch(&dir, false);
    let (text, error) = agent.call("list_sessions", json!({})).await;
    assert!(error && text.contains("switch it on"), "{text}");
    switch(&dir, true);

    // Tern quit: no one takes calls, and the relay doesn't start one here.
    drop(server);
    assert!(!dir.join("mcp.json").exists());
    assert_eq!(
        agent.call("list_sessions", json!({})).await,
        ("Tern isn't running".into(), true)
    );

    // The agent closing its end ends the relay.
    agent.to.shutdown().await.unwrap();
    drop(agent);
    serving.await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn tern_takes_calls_only_with_its_token() {
    use tokio::net::TcpStream;

    let dir = data_dir("token", true);
    let (_server, mut incomings) = Server::start(&dir).unwrap();
    let endpoint: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("mcp.json")).unwrap()).unwrap();
    let port = endpoint["port"].as_u64().unwrap() as u16;
    let request = json!({ "token": "guess", "client": "x", "name": "list_sessions" });
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).await.unwrap();
    assert_eq!(reply, "", "a wrong token gets no answer");
    // And reaches nothing.
    let nothing = tokio::time::timeout(std::time::Duration::from_millis(100), incomings.next());
    assert!(nothing.await.is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(start_paused = true)]
async fn the_relay_starts_tern_when_no_one_takes_calls() {
    let dir = data_dir("start", true);
    // A program that isn't there can't start.
    let missing = Relay::new(dir.clone(), Some(dir.join("no-such-tern")));
    let answer = missing.call("list_sessions", json!({})).await;
    assert!(
        answer.error && answer.text.starts_with("Tern couldn't start: "),
        "{}",
        answer.text
    );
    // One that starts and never takes calls is given up on.
    let shell = if cfg!(windows) { "cmd.exe" } else { "/bin/sh" };
    let quiet = Relay::new(dir.clone(), Some(shell.into()));
    assert_eq!(
        quiet.call("list_sessions", json!({})).await,
        Answer::error("Tern started, but isn't taking calls")
    );
    let _ = std::fs::remove_dir_all(&dir);
}
