//! Tern's MCP server, for agents (Claude Code, Claude Desktop, any MCP
//! client): they list, add, change and remove saved hosts, and open, read,
//! type into and close sessions, in the same tabs the user sees.
//!
//! The agent starts `tern --mcp` ([`serve_stdio`]), which speaks MCP over
//! its standard input and output and hands each tool call to the Tern
//! running here, starting it when it isn't. Tern takes calls ([`Server`])
//! only while its settings say so, on a loopback port, and only with the
//! token it wrote beside the port in `mcp.json` in its data folder, which
//! is the user's alone. The app acts on a call as the user would (tern-app's
//! `mcp.rs`); what a call may say, and how it's checked, is here.

use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::{
    AsyncBufReadExt as _, AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, BufReader,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::draft::HostDraft;
use crate::host::Auth;
use crate::settings::Settings;
use crate::ssh::{Target, parse_command};
use crate::vault::Vault;

/// The MCP versions spoken, the one answered to a client asking for
/// another first.
const VERSIONS: &[&str] = &["2025-06-18", "2025-11-25", "2025-03-26", "2024-11-05"];

/// The longest a call may wait for a session, in seconds.
pub const MAX_WAIT: f64 = 120.;
/// Lines a read gives unless asked for another number, and at most.
const LINES: usize = 200;
const MOST_LINES: usize = 10_000;
/// How long `open_session` and `send_input` wait unless told otherwise.
const OPEN_WAIT: f64 = 20.;
const SEND_WAIT: f64 = 10.;
/// The longest a tool call's request to the running Tern may be.
const MAX_REQUEST: u64 = 4 << 20;
/// How long a starting Tern has to start taking calls.
const START_WAIT: Duration = Duration::from_secs(20);
/// How long a call waits for a Tern that may be starting by itself before
/// starting one.
const GRACE: Duration = Duration::from_secs(2);

/// Why a call couldn't reach Tern, as the agent reads it.
const OFF: &str = "Tern's MCP server is off: switch it on in Tern's Settings, under General";
const NOT_RUNNING: &str = "Tern isn't running";

const INSTRUCTIONS: &str = "Tern is the SSH client on this computer, and these tools act in the \
Tern its user has open, in the tabs they see. list_hosts shows the saved hosts; open_session \
connects to one (or to a typed destination, or opens a shell on this computer) in a new tab; \
send_input types into a session and returns what it printed; read_session reads more of it, or \
its whole screen for full-screen programs; close_session closes it. add_host, edit_host and \
remove_host change the saved hosts. A guarded host's sessions hold typed input until the user \
allows it in Tern, and only the user can change or remove a guarded host.";

/// A tool call, checked: what the running Tern is asked to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    /// The saved hosts matching `query` (all for an empty one).
    ListHosts {
        query: String,
    },
    AddHost(HostFields),
    /// Changes the saved host called `host`.
    EditHost {
        host: String,
        fields: HostFields,
    },
    RemoveHost {
        host: String,
    },
    ListSessions,
    OpenSession {
        to: To,
        wait: Duration,
    },
    ReadSession {
        session: u32,
        wait: Duration,
        /// The whole screen instead of what's new.
        screen: bool,
        lines: usize,
    },
    SendInput {
        session: u32,
        text: String,
        keys: Vec<Key>,
        wait: Duration,
    },
    CloseSession {
        session: u32,
    },
}

/// Where `open_session` connects.
#[derive(Clone, Debug, PartialEq)]
pub enum To {
    /// A saved host, by name.
    Host(String),
    /// A typed destination, with the key file `-i` gave.
    Typed(Target, Option<PathBuf>),
    /// A shell on this computer.
    Local,
}

/// A key an agent presses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// A named key (`enter`, `up`, `f5`, `space`…) or one character.
    pub name: String,
}

/// Keys by name, as a terminal's keyboard has them.
const KEYS: &[&str] = &[
    "enter",
    "tab",
    "escape",
    "backspace",
    "delete",
    "insert",
    "up",
    "down",
    "left",
    "right",
    "home",
    "end",
    "pageup",
    "pagedown",
    "space",
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f7",
    "f8",
    "f9",
    "f10",
    "f11",
    "f12",
];

/// Other names keys go by.
const ALSO: &[(&str, &str)] = &[
    ("return", "enter"),
    ("esc", "escape"),
    ("del", "delete"),
    ("ins", "insert"),
    ("pgup", "pageup"),
    ("pgdn", "pagedown"),
    ("page_up", "pageup"),
    ("page_down", "pagedown"),
    ("arrowup", "up"),
    ("arrowdown", "down"),
    ("arrowleft", "left"),
    ("arrowright", "right"),
];

impl Key {
    /// The key `name` names: `enter`, `ctrl+c`, `Ctrl-C`, `alt+x`,
    /// `shift+tab`, `ctrl+shift+up`…
    pub fn parse(name: &str) -> Result<Self, String> {
        let wrong =
            || format!("`{name}` isn't a key: name one like enter, tab, escape, up, f5 or ctrl+c");
        let name = name.trim();
        // The last character is the key itself when it's `+` or `-`.
        let (modifiers, pressed) = match name.char_indices().last() {
            None => return Err(wrong()),
            Some((at, '+' | '-')) => (&name[..at], &name[at..]),
            Some(_) => match name.rfind(['+', '-']) {
                Some(at) => (&name[..at], &name[at + 1..]),
                None => ("", name),
            },
        };
        let mut key = Self {
            ctrl: false,
            alt: false,
            shift: false,
            name: String::new(),
        };
        for modifier in modifiers.split(['+', '-']).filter(|part| !part.is_empty()) {
            match modifier.to_lowercase().as_str() {
                "ctrl" | "control" => key.ctrl = true,
                "alt" | "option" | "meta" => key.alt = true,
                "shift" => key.shift = true,
                _ => return Err(wrong()),
            }
        }
        // One character keeps its case; names don't care.
        let named = pressed.to_lowercase();
        key.name = if pressed.chars().count() == 1 {
            pressed.to_string()
        } else if KEYS.contains(&named.as_str()) {
            named
        } else if let Some((_, also)) = ALSO.iter().find(|(also, _)| *also == named) {
            also.to_string()
        } else {
            return Err(wrong());
        };
        Ok(key)
    }
}

/// A host's fields as a call gives them; those left out stay as they are,
/// or take Tern's defaults on a new host.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostFields {
    pub name: Option<String>,
    pub address: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub group: Option<String>,
    pub tags: Option<Vec<String>>,
    pub sign_in: Option<SignIn>,
    /// The key file for [`SignIn::Key`]; `~` is the home folder.
    pub key: Option<String>,
    /// A password to save in the system's credential store.
    pub password: Option<String>,
    /// An identity's name; empty for none.
    pub identity: Option<String>,
    /// A jump host's name; empty for none.
    pub jump: Option<String>,
    /// A proxy's URL; empty for none.
    pub proxy: Option<String>,
    pub notes: Option<String>,
    pub on_login: Option<String>,
    pub env: Option<std::collections::BTreeMap<String, String>>,
    pub mac: Option<String>,
    pub keep_alive: Option<bool>,
    pub agent_forwarding: Option<bool>,
    pub guarded: Option<bool>,
    pub keep_shell: Option<bool>,
}

/// How a host signs in, as a call names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignIn {
    Agent,
    Key,
    Password,
}

impl HostFields {
    /// Writes these fields over `draft`, a new host's or the one being
    /// changed; the draft's own check follows. Fails with a one-line reason
    /// on what no check could make sense of.
    pub fn apply(&self, draft: &mut HostDraft, vault: &Vault) -> Result<(), String> {
        let set = |field: &mut String, value: &Option<String>| {
            if let Some(value) = value {
                *field = value.clone();
            }
        };
        set(&mut draft.name, &self.name);
        set(&mut draft.address, &self.address);
        set(&mut draft.user, &self.user);
        set(&mut draft.group, &self.group);
        set(&mut draft.notes, &self.notes);
        set(&mut draft.on_login, &self.on_login);
        set(&mut draft.mac, &self.mac);
        if let Some(port) = self.port {
            draft.port = port.to_string();
        }
        if let Some(tags) = &self.tags {
            draft.tags = tags.clone();
        }
        if let Some(env) = &self.env {
            draft.env = crate::env::format(env);
        }
        let flags = [
            (&mut draft.keep_alive, self.keep_alive),
            (&mut draft.agent_forwarding, self.agent_forwarding),
            (&mut draft.guarded, self.guarded),
            (&mut draft.keep_shell, self.keep_shell),
        ];
        for (flag, value) in flags {
            if let Some(value) = value {
                *flag = value;
            }
        }
        let none_if_empty = |value: &str| Some(value.trim().to_string()).filter(|v| !v.is_empty());
        if let Some(jump) = &self.jump {
            draft.jump = none_if_empty(jump);
        }
        if let Some(proxy) = &self.proxy {
            draft.proxy = none_if_empty(proxy);
        }
        if let Some(identity) = &self.identity {
            draft.identity = match none_if_empty(identity) {
                None => None,
                Some(name) => Some(
                    vault
                        .identities
                        .iter()
                        .find(|identity| identity.name.eq_ignore_ascii_case(&name))
                        .map(|identity| identity.id.clone())
                        .ok_or_else(|| format!("No identity is called {name}"))?,
                ),
            };
        }
        let sign_in = match (self.sign_in, &self.key, &self.password) {
            (Some(SignIn::Agent | SignIn::Password), Some(_), _) => {
                return Err("A key goes with sign_in key".into());
            }
            (Some(SignIn::Agent | SignIn::Key), _, Some(_)) => {
                return Err("A password goes with sign_in password".into());
            }
            (_, Some(_), Some(_)) => return Err("Give a key or a password, not both".into()),
            (_, Some(key), _) => Some(Auth::Key(home_path(key))),
            (Some(SignIn::Key), None, _) => match &draft.auth {
                Auth::Key(path) => Some(Auth::Key(path.clone())),
                _ => return Err("Give the key file's path in key".into()),
            },
            (Some(SignIn::Password), _, _) | (None, None, Some(_)) => Some(Auth::Password),
            (Some(SignIn::Agent), None, None) => Some(Auth::Agent),
            (None, None, None) => None,
        };
        if let Some(auth) = sign_in {
            draft.auth = auth;
        }
        Ok(())
    }
}

/// `path` with a leading `~` as the home folder.
fn home_path(path: &str) -> PathBuf {
    match (path.strip_prefix("~"), std::env::home_dir()) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            home.join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(path),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListHostsArguments {
    #[serde(default)]
    query: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HostArguments {
    host: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenArguments {
    host: Option<String>,
    destination: Option<String>,
    #[serde(default)]
    local: bool,
    wait: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArguments {
    session: u32,
    wait: Option<f64>,
    #[serde(default)]
    screen: bool,
    lines: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArguments {
    session: u32,
    #[serde(default)]
    text: String,
    #[serde(default)]
    keys: Vec<String>,
    wait: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionArguments {
    session: u32,
}

/// `arguments` as `T`, or why they aren't one, in a line.
fn arguments<T: serde::de::DeserializeOwned>(arguments: Value) -> Result<T, String> {
    // A call with nothing to say may leave its arguments out.
    let arguments = match arguments {
        Value::Null => Value::Object(Map::new()),
        given => given,
    };
    serde_json::from_value(arguments).map_err(|error| error.to_string())
}

/// A wait in seconds, `default` when not given, at most [`MAX_WAIT`].
fn seconds(given: Option<f64>, default: f64) -> Result<Duration, String> {
    match given.unwrap_or(default) {
        wait if wait.is_finite() && wait >= 0. => Ok(Duration::from_secs_f64(wait.min(MAX_WAIT))),
        _ => Err("wait is a number of seconds, from 0".into()),
    }
}

impl Call {
    /// The call of the tool `name` with `arguments`, checked; a one-line
    /// reason when they don't do.
    pub fn parse(name: &str, given: Value) -> Result<Self, String> {
        Ok(match name {
            "list_hosts" => Call::ListHosts {
                query: arguments::<ListHostsArguments>(given)?.query,
            },
            "add_host" => Call::AddHost(arguments(given)?),
            "edit_host" => {
                let Value::Object(mut fields) = given else {
                    return Err("edit_host takes the host's name and the fields to change".into());
                };
                let host = match fields.remove("host") {
                    Some(Value::String(host)) => host,
                    _ => return Err("host, the name of the host to change, is required".into()),
                };
                Call::EditHost {
                    host,
                    fields: arguments(Value::Object(fields))?,
                }
            }
            "remove_host" => Call::RemoveHost {
                host: arguments::<HostArguments>(given)?.host,
            },
            "list_sessions" => {
                arguments::<NoArguments>(given)?;
                Call::ListSessions
            }
            "open_session" => {
                let open: OpenArguments = arguments(given)?;
                let to = match (open.host, open.destination, open.local) {
                    (Some(host), None, false) => To::Host(host),
                    (None, Some(destination), false) => {
                        let (target, key) = parse_command(&destination)
                            .map_err(|error| format!("destination: {error}"))?;
                        To::Typed(target, key)
                    }
                    (None, None, true) => To::Local,
                    _ => return Err("Give one of host, destination or local".into()),
                };
                Call::OpenSession {
                    to,
                    wait: seconds(open.wait, OPEN_WAIT)?,
                }
            }
            "read_session" => {
                let read: ReadArguments = arguments(given)?;
                Call::ReadSession {
                    session: read.session,
                    wait: seconds(read.wait, 0.)?,
                    screen: read.screen,
                    lines: read.lines.unwrap_or(LINES).clamp(1, MOST_LINES),
                }
            }
            "send_input" => {
                let send: SendArguments = arguments(given)?;
                if send.text.is_empty() && send.keys.is_empty() {
                    return Err("Give text or keys to send".into());
                }
                Call::SendInput {
                    session: send.session,
                    text: send.text,
                    keys: send
                        .keys
                        .iter()
                        .map(|key| Key::parse(key))
                        .collect::<Result<_, _>>()?,
                    wait: seconds(send.wait, SEND_WAIT)?,
                }
            }
            "close_session" => Call::CloseSession {
                session: arguments::<SessionArguments>(given)?.session,
            },
            _ => return Err(format!("There's no tool called {name}")),
        })
    }
}

/// What a tool call answers: text for the agent, which says what went
/// wrong when `error` is set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub error: bool,
}

impl Answer {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            error: true,
        }
    }
}

/// The tools, as `tools/list` gives them.
pub fn tools() -> Value {
    let session = json!({
        "type": "integer",
        "minimum": 0,
        "description": "The session's id, from open_session or list_sessions",
    });
    let wait = |default: f64, what: &str| {
        json!({
            "type": "number",
            "minimum": 0,
            "maximum": MAX_WAIT,
            "description": format!("Seconds to wait {what}; {default} unless given"),
        })
    };
    let text = |description: &str| json!({ "type": "string", "description": description });
    let flag = |description: &str| json!({ "type": "boolean", "description": description });
    let fields = json!({
        "name": text("What Tern calls the host, which the other tools take; unique"),
        "address": text("Its hostname or IP address"),
        "port": { "type": "integer", "minimum": 1, "maximum": 65535, "description": "Its SSH port; 22 for a new host unless given" },
        "user": text("The user to sign in as"),
        "group": text("The group it's listed in; Hosts for a new host unless given"),
        "tags": { "type": "array", "items": { "type": "string" } },
        "sign_in": {
            "type": "string",
            "enum": ["agent", "key", "password"],
            "description": "agent: the SSH agent, then the usual keys in ~/.ssh. key: the key file in key first. password: a password, asked for when connecting unless password saves one",
        },
        "key": text("A private key file's path, for sign_in key"),
        "password": text("A password, saved in the system's credential store (never in Tern's files); means sign_in password"),
        "identity": text("A saved identity's name, whose user and sign-in the host takes instead of its own; empty for none"),
        "jump": text("Another saved host's name, to connect through first; empty for none"),
        "proxy": text("A proxy to connect through, like socks5://host:1080 or http://host:3128; empty for none"),
        "notes": text("Notes about the host"),
        "on_login": text("A command typed into the shell when it starts"),
        "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Environment variables the shell starts with, which the server must accept" },
        "mac": text("Its MAC address, for Wake-on-LAN"),
        "keep_alive": flag("Keep idle connections up; on for a new host unless given"),
        "agent_forwarding": flag("Let the host use this computer's SSH agent"),
        "guarded": flag("Guard against stray input: pastes, snippets and agents' typing wait for the user's yes"),
        "keep_shell": flag("Keep the shell running in tmux or screen on the host, so a dropped session reattaches"),
    });
    let mut edit_fields = fields.clone();
    edit_fields["host"] = text("The name of the host to change");
    edit_fields["name"] = text("A new name for it");
    let object = |properties: Value, required: &[&str]| {
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        })
    };
    let hints = |read_only: bool, destructive: bool| json!({ "readOnlyHint": read_only, "destructiveHint": destructive, "openWorldHint": true });
    json!([
        {
            "name": "list_hosts",
            "title": "List hosts",
            "description": "Lists the hosts saved in Tern: each one's name (which the other tools take), user, address, port, group, tags, how it signs in, jump host, notes, whether it answered Tern's last check, and the ids of the sessions open to it.",
            "inputSchema": object(json!({ "query": text("Only hosts whose name, user, address, tags or notes have every word of this") }), &[]),
            "annotations": hints(true, false),
        },
        {
            "name": "add_host",
            "title": "Add a host",
            "description": "Saves a new host in Tern, checked as Tern's host editor checks one; the answer says what to fix.",
            "inputSchema": object(fields, &["name", "address"]),
            "annotations": hints(false, false),
        },
        {
            "name": "edit_host",
            "title": "Change a host",
            "description": "Changes a saved host: only the fields given change, and name renames it. A guarded host only the user can change.",
            "inputSchema": object(edit_fields, &["host"]),
            "annotations": hints(false, false),
        },
        {
            "name": "remove_host",
            "title": "Remove a host",
            "description": "Deletes a saved host, with its saved password. Refused while other hosts connect through it, and for a guarded host. Its open sessions stay open.",
            "inputSchema": object(json!({ "host": text("The name of the host to delete") }), &["host"]),
            "annotations": hints(false, true),
        },
        {
            "name": "list_sessions",
            "title": "List sessions",
            "description": "Lists the sessions open in Tern's tabs, in all its windows, the user's own included: each one's id (which the session tools take), what it's connected to, its title, its state and whether its tab is showing.",
            "inputSchema": object(json!({}), &[]),
            "annotations": hints(true, false),
        },
        {
            "name": "open_session",
            "title": "Open a session",
            "description": "Opens a session in a new Tern tab, to a saved host, to a typed destination or as a shell on this computer, and waits for it to come up or to ask for something. Answers with its id, its state and the lines up to its cursor.",
            "inputSchema": object(json!({
                "host": text("A saved host's name"),
                "destination": text("Where to connect when it isn't saved: user@host, user@host:port, or an ssh command like ssh -p 2222 -i ~/.ssh/key user@host"),
                "local": flag("A shell on this computer instead"),
                "wait": wait(OPEN_WAIT, "for the session to come up"),
            }), &[]),
            "annotations": hints(false, false),
        },
        {
            "name": "read_session",
            "title": "Read a session",
            "description": "Reads a session: the lines it printed since this agent last read it or typed into it (a first read gives the lines up to the cursor), or with screen the whole screen as a full-screen program draws it. Starts with the session's state.",
            "inputSchema": object(json!({
                "session": session,
                "wait": wait(0., "for new output to arrive and settle"),
                "screen": flag("The whole screen, rows top to bottom, instead of the new lines"),
                "lines": { "type": "integer", "minimum": 1, "maximum": MOST_LINES, "description": format!("At most this many lines, the last ones; {LINES} unless given") },
            }), &["session"]),
            "annotations": hints(true, false),
        },
        {
            "name": "send_input",
            "title": "Type into a session",
            "description": "Types into a session as at its keyboard: text as it is (a newline is Enter), then each of keys pressed in turn (enter, tab, escape, backspace, delete, up, down, left, right, home, end, pageup, pagedown, space, f1 to f12, with ctrl+, alt+ or shift+ as in ctrl+c). While the session asks something before its shell starts, this answers it as the keyboard would: type a password or answer and end it with a newline; enter trusts an unknown host key and escape refuses it; enter reconnects an ended session. Then waits for the command to finish or the output to settle, and answers with what was printed since this agent last read the session. A guarded host's session holds the input until the user allows it in Tern.",
            "inputSchema": object(json!({
                "session": session,
                "text": text("Text to type; a newline is Enter"),
                "keys": { "type": "array", "items": { "type": "string" }, "description": "Keys to press after the text, like enter, ctrl+c or up" },
                "wait": wait(SEND_WAIT, "for the command to finish or the output to settle"),
            }), &["session"]),
            "annotations": hints(false, true),
        },
        {
            "name": "close_session",
            "title": "Close a session",
            "description": "Closes a session's pane (its tab, when it's the tab's last), ending the connection.",
            "inputSchema": object(json!({ "session": session }), &["session"]),
            "annotations": hints(false, true),
        },
    ])
}

/// The tools' names.
fn tool_names() -> Vec<String> {
    tools()
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["name"].as_str().map(String::from))
        .collect()
}

/// What a message from the client needs.
#[derive(Debug, PartialEq)]
enum Handling {
    /// Nothing: a notification, or a reply.
    Nothing,
    /// This reply, at once.
    Reply(Value),
    /// The running Tern's answer to the tool `name` called with
    /// `arguments`.
    Call {
        id: Value,
        name: String,
        arguments: Value,
    },
}

fn result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn failure(id: &Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// A tool call's reply carrying `answer`.
fn answered(id: &Value, answer: Answer) -> Value {
    result(
        id,
        json!({
            "content": [{ "type": "text", "text": answer.text }],
            "isError": answer.error,
        }),
    )
}

/// What the JSON-RPC message `line` needs.
fn handle(line: &str) -> Handling {
    let Ok(message) = serde_json::from_str::<Value>(line) else {
        return Handling::Reply(failure(&Value::Null, -32700, "Parse error"));
    };
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        // A reply to a request of ours: there are none.
        return match message.is_object() {
            true => Handling::Nothing,
            false => Handling::Reply(failure(&Value::Null, -32600, "Invalid Request")),
        };
    };
    let Some(id) = message.get("id") else {
        return Handling::Nothing;
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or_default();
            let version = VERSIONS
                .iter()
                .find(|version| **version == asked)
                .unwrap_or(&VERSIONS[0]);
            Handling::Reply(result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "tern", "title": "Tern", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                }),
            ))
        }
        "ping" => Handling::Reply(result(id, json!({}))),
        "tools/list" => Handling::Reply(result(id, json!({ "tools": tools() }))),
        "tools/call" => match params["name"].as_str() {
            Some(name) if tool_names().iter().any(|tool| tool == name) => Handling::Call {
                id: id.clone(),
                name: name.to_string(),
                arguments: params.get("arguments").cloned().unwrap_or(Value::Null),
            },
            Some(name) => Handling::Reply(failure(id, -32602, &format!("Unknown tool: {name}"))),
            None => Handling::Reply(failure(id, -32602, "Invalid params")),
        },
        _ => Handling::Reply(failure(id, -32601, "Method not found")),
    }
}

/// Serves MCP on standard input and output until input ends: what
/// `tern --mcp` runs.
pub fn serve_stdio() -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let relay = Relay::new(crate::paths::data_dir(), std::env::current_exe().ok());
    runtime.block_on(serve(tokio::io::stdin(), tokio::io::stdout(), relay))
}

/// Serves MCP over `input` and `output`, handing tool calls to `relay`,
/// until input ends.
pub async fn serve(
    input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin + Send + 'static,
    relay: Relay,
) -> io::Result<()> {
    // One writer, so replies to calls answered at once don't interleave.
    let (replies, mut to_write) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(reply) = to_write.recv().await {
            output.write_all(format!("{reply}\n").as_bytes()).await?;
            output.flush().await?;
        }
        io::Result::Ok(())
    });
    let relay = Arc::new(relay);
    let mut calls = tokio::task::JoinSet::new();
    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match handle(line) {
            Handling::Nothing => {}
            Handling::Reply(reply) => {
                let _ = replies.send(reply);
            }
            // Calls run side by side: one waiting for a session doesn't
            // hold up the next.
            Handling::Call {
                id,
                name,
                arguments,
            } => {
                let (relay, replies) = (relay.clone(), replies.clone());
                calls.spawn(async move {
                    let answer = relay.call(&name, arguments).await;
                    let _ = replies.send(answered(&id, answer));
                });
            }
        }
    }
    // The client is done: calls still waiting go unanswered.
    calls.abort_all();
    drop(replies);
    writer.await.map_err(io::Error::other)?
}

/// Where the running Tern takes tool calls, as `mcp.json` says.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Endpoint {
    port: u16,
    token: String,
}

/// A tool call as the running Tern receives it.
#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    client: String,
    name: String,
    #[serde(default)]
    arguments: Value,
}

fn endpoint_file(dir: &Path) -> PathBuf {
    dir.join("mcp.json")
}

fn read_endpoint(dir: &Path) -> Option<Endpoint> {
    let text = std::fs::read_to_string(endpoint_file(dir)).ok()?;
    serde_json::from_str(&text).ok()
}

/// A fresh random token, in hex.
fn token() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Whether two tokens are the same, taking as long whatever they hold.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |diff, (x, y)| diff | (x ^ y))
            == 0
}

/// Why a call didn't get an answer from the running Tern.
enum Unanswered {
    /// Nothing listens: it isn't running, or isn't taking calls yet.
    Unreachable,
    /// It took the call and stopped answering.
    Broken(String),
}

/// Hands an agent's tool calls to the Tern running here, starting it if
/// it isn't.
pub struct Relay {
    /// Tern's data folder, whose `mcp.json` says where it listens.
    dir: PathBuf,
    /// Tern's program, to start when it isn't running; `None` doesn't.
    program: Option<PathBuf>,
    /// Who this agent is to Tern, which keeps where each agent read up to.
    client: String,
    /// Held while Tern starts, so that calls side by side start one.
    starting: Mutex<()>,
}

impl Relay {
    pub fn new(dir: PathBuf, program: Option<PathBuf>) -> Self {
        Self {
            dir,
            program,
            client: token()[..16].to_string(),
            starting: Mutex::new(()),
        }
    }

    /// The running Tern's answer to the tool `name` called with
    /// `arguments`.
    pub async fn call(&self, name: &str, arguments: Value) -> Answer {
        if !Settings::load(&self.dir.join("settings.json")).mcp {
            return Answer::error(OFF);
        }
        let unanswered = match self.ask(name, &arguments).await {
            Ok(answer) => return answer,
            Err(unanswered) => unanswered,
        };
        if let Unanswered::Broken(reason) = unanswered {
            return Answer::error(reason);
        }
        let Some(program) = &self.program else {
            return Answer::error(NOT_RUNNING);
        };
        let _starting = self.starting.lock().await;
        // It may be starting by itself, or another call started it.
        let mut waited = Duration::ZERO;
        let mut started = false;
        let step = Duration::from_millis(250);
        loop {
            match self.ask(name, &arguments).await {
                Ok(answer) => return answer,
                Err(Unanswered::Broken(reason)) => return Answer::error(reason),
                Err(Unanswered::Unreachable) => {}
            }
            if !started && waited >= GRACE {
                if let Err(error) = start(program) {
                    return Answer::error(format!("Tern couldn't start: {error}"));
                }
                started = true;
            }
            if waited >= GRACE + START_WAIT {
                return Answer::error("Tern started, but isn't taking calls");
            }
            tokio::time::sleep(step).await;
            waited += step;
        }
    }

    /// Hands the call to the running Tern and waits for its answer.
    async fn ask(&self, name: &str, arguments: &Value) -> Result<Answer, Unanswered> {
        let endpoint = read_endpoint(&self.dir).ok_or(Unanswered::Unreachable)?;
        let connecting = TcpStream::connect((Ipv4Addr::LOCALHOST, endpoint.port));
        let stream = match tokio::time::timeout(Duration::from_secs(2), connecting).await {
            Ok(Ok(stream)) => stream,
            _ => return Err(Unanswered::Unreachable),
        };
        let request = Request {
            token: endpoint.token,
            client: self.client.clone(),
            name: name.to_string(),
            arguments: arguments.clone(),
        };
        let broken =
            |error: io::Error| Unanswered::Broken(format!("Tern stopped answering: {error}"));
        let (read, mut write) = stream.into_split();
        let line = serde_json::to_string(&request).map_err(|error| broken(error.into()))?;
        write
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(broken)?;
        let mut reply = String::new();
        BufReader::new(read)
            .read_line(&mut reply)
            .await
            .map_err(broken)?;
        if reply.is_empty() {
            // Tern refused the token, or closed while working on the call.
            return Err(Unanswered::Broken("Tern closed without answering".into()));
        }
        serde_json::from_str(&reply).map_err(|error| broken(error.into()))
    }
}

/// Starts Tern, on its own: closing the agent leaves it running.
fn start(program: &Path) -> io::Result<()> {
    use std::process::{Command, Stdio};
    let mut command = Command::new(program);
    // Never the agent's pipes: Tern writing there would garble MCP.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x8;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        // Out of the agent's job, when it may leave it, so that ending the
        // agent's processes doesn't end Tern.
        command.creation_flags(
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB,
        );
        if command.spawn().is_ok() {
            return Ok(());
        }
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    command.spawn().map(drop)
}

/// Tern taking agents' tool calls, for as long as it's kept: on a loopback
/// port, written with its token to `mcp.json`.
pub struct Server {
    dir: PathBuf,
    token: String,
    accepting: tokio::task::JoinHandle<()>,
}

/// A tool call for the app to act on and answer.
pub struct Incoming {
    /// The agent asking (each `tern --mcp` is one).
    pub client: String,
    pub call: Call,
    reply: oneshot::Sender<Answer>,
}

impl Incoming {
    pub fn answer(self, answer: Answer) {
        let _ = self.reply.send(answer);
    }
}

/// The tool calls arriving, readable from any async executor.
pub struct Incomings(mpsc::UnboundedReceiver<Incoming>);

impl Incomings {
    pub async fn next(&mut self) -> Option<Incoming> {
        self.0.recv().await
    }
}

impl Server {
    /// Starts taking calls, saying where in `dir`'s `mcp.json`.
    pub fn start(dir: &Path) -> Result<(Self, Incomings), String> {
        // A Tern already taking calls keeps them: another one started, or
        // a check's, mustn't take its place.
        if let Some(endpoint) = read_endpoint(dir) {
            let address = (Ipv4Addr::LOCALHOST, endpoint.port).into();
            if std::net::TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok() {
                return Err("Another Tern takes agents' calls".into());
            }
        }
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .and_then(|listener| listener.set_nonblocking(true).map(|()| listener))
            .map_err(|error| format!("Can't listen for agents: {error}"))?;
        let port = listener
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        let token = token();
        let endpoint = Endpoint {
            port,
            token: token.clone(),
        };
        let json = serde_json::to_string_pretty(&endpoint).map_err(|error| error.to_string())?;
        crate::paths::write_atomically(&endpoint_file(dir), format!("{json}\n").as_bytes())
            .map_err(|error| format!("Can't save {}: {error}", endpoint_file(dir).display()))?;
        let runtime = crate::ssh::runtime();
        let listener = {
            let _entered = runtime.enter();
            TcpListener::from_std(listener).map_err(|error| error.to_string())?
        };
        let (calls, incoming) = mpsc::unbounded_channel();
        let accepting = runtime.spawn(accept(listener, token.clone(), calls));
        let server = Self {
            dir: dir.to_path_buf(),
            token,
            accepting,
        };
        Ok((server, Incomings(incoming)))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.accepting.abort();
        // Another Tern may have taken calls over since.
        if read_endpoint(&self.dir).is_some_and(|endpoint| same(&endpoint.token, &self.token)) {
            let _ = std::fs::remove_file(endpoint_file(&self.dir));
        }
    }
}

async fn accept(listener: TcpListener, token: String, calls: mpsc::UnboundedSender<Incoming>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let (token, calls) = (token.clone(), calls.clone());
                tokio::spawn(async move {
                    let _ = take_call(stream, &token, &calls).await;
                });
            }
            // Out of sockets, say: try again in a moment.
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Reads one call from `stream`, hands it to the app, and writes back the
/// answer. A request without the token gets none.
async fn take_call(
    stream: TcpStream,
    token: &str,
    calls: &mpsc::UnboundedSender<Incoming>,
) -> io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(read.take(MAX_REQUEST));
    let reading = reader.read_line(&mut line);
    tokio::time::timeout(Duration::from_secs(10), reading)
        .await
        .map_err(io::Error::other)??;
    let request: Request = serde_json::from_str(&line)?;
    if !same(&request.token, token) {
        return Ok(());
    }
    let answer = match Call::parse(&request.name, request.arguments) {
        Err(reason) => Answer::error(reason),
        Ok(call) => {
            let (reply, answered) = oneshot::channel();
            let incoming = Incoming {
                client: request.client,
                call,
                reply,
            };
            match calls.send(incoming) {
                Ok(()) => answered
                    .await
                    .unwrap_or_else(|_| Answer::error("Tern closed before answering")),
                Err(_) => Answer::error("Tern is closing"),
            }
        }
    };
    let line = serde_json::to_string(&answer)?;
    write.write_all(format!("{line}\n").as_bytes()).await?;
    write.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Host;

    fn parse(name: &str, arguments: Value) -> Result<Call, String> {
        Call::parse(name, arguments)
    }

    #[test]
    fn reads_keys_as_agents_name_them() {
        let key = |name: &str| Key::parse(name).unwrap();
        assert_eq!(
            key("Ctrl+c"),
            Key {
                ctrl: true,
                alt: false,
                shift: false,
                name: "c".into()
            }
        );
        // A character keeps its case.
        assert_eq!(key("Y").name, "Y");
        assert_eq!(
            key("ctrl-shift-up"),
            Key {
                shift: true,
                ..key("ctrl+up")
            }
        );
        assert_eq!(key("ESC").name, "escape");
        assert_eq!(key("Return").name, "enter");
        assert_eq!(key("alt+.").name, ".");
        // Minus and plus are keys too.
        assert_eq!(
            key("ctrl+-"),
            Key {
                name: "-".into(),
                ..key("ctrl+c")
            }
        );
        assert_eq!(key("+").name, "+");
        assert!(key("shift+tab").shift);
        for wrong in ["", "hyper+x", "ctrl+wat", "enterr"] {
            assert!(Key::parse(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn checks_what_each_tool_is_given() {
        assert_eq!(
            parse("list_hosts", Value::Null),
            Ok(Call::ListHosts {
                query: String::new()
            })
        );
        assert_eq!(parse("list_sessions", json!({})), Ok(Call::ListSessions));
        assert_eq!(
            parse("read_session", json!({ "session": 3 })),
            Ok(Call::ReadSession {
                session: 3,
                wait: Duration::ZERO,
                screen: false,
                lines: LINES
            })
        );
        // Waits are capped; lines are kept in range.
        assert_eq!(
            parse(
                "read_session",
                json!({ "session": 3, "wait": 1e6, "lines": 0 })
            ),
            Ok(Call::ReadSession {
                session: 3,
                wait: Duration::from_secs_f64(MAX_WAIT),
                screen: false,
                lines: 1
            })
        );
        assert_eq!(
            parse(
                "send_input",
                json!({ "session": 1, "text": "ls\n", "keys": ["ctrl+c"] })
            ),
            Ok(Call::SendInput {
                session: 1,
                text: "ls\n".into(),
                keys: vec![Key::parse("ctrl+c").unwrap()],
                wait: Duration::from_secs_f64(SEND_WAIT),
            })
        );
        let wrong = |name: &str, arguments: Value| parse(name, arguments).unwrap_err();
        assert_eq!(
            wrong("send_input", json!({ "session": 1 })),
            "Give text or keys to send"
        );
        assert!(
            wrong(
                "send_input",
                json!({ "session": 1, "keys": ["hyperspace"] })
            )
            .contains("isn't a key")
        );
        assert!(wrong("read_session", json!({ "session": "one" })).contains("invalid type"));
        assert!(wrong("read_session", json!({ "session": 1, "wait": -1 })).contains("wait"));
        // A misspelt field says which fields there are.
        assert!(wrong("remove_host", json!({ "hostname": "web" })).contains("expected `host`"));
        assert!(wrong("list_sessions", json!({ "all": true })).contains("unknown field"));
        assert_eq!(wrong("fly", json!({})), "There's no tool called fly");
    }

    #[test]
    fn opens_sessions_to_one_place() {
        assert_eq!(
            parse("open_session", json!({ "host": "web", "wait": 5 })),
            Ok(Call::OpenSession {
                to: To::Host("web".into()),
                wait: Duration::from_secs(5)
            })
        );
        let Ok(Call::OpenSession { to, .. }) = parse(
            "open_session",
            json!({ "destination": "ssh -p 2222 -i key.pem me@example.com" }),
        ) else {
            panic!("a typed destination opens");
        };
        assert_eq!(
            to,
            To::Typed(
                Target {
                    host: "example.com".into(),
                    port: 2222,
                    user: "me".into()
                },
                Some(PathBuf::from("key.pem"))
            )
        );
        assert!(matches!(
            parse("open_session", json!({ "local": true })),
            Ok(Call::OpenSession { to: To::Local, .. })
        ));
        for arguments in [json!({}), json!({ "host": "web", "local": true })] {
            assert_eq!(
                parse("open_session", arguments),
                Err("Give one of host, destination or local".into())
            );
        }
        assert!(
            parse("open_session", json!({ "destination": "ssh" }))
                .unwrap_err()
                .starts_with("destination: ")
        );
    }

    fn vault() -> Vault {
        let mut vault = Vault::default();
        let mut db = Host::new("db", "admin", "10.0.0.5", 22);
        db.auth = Auth::Key(PathBuf::from("/keys/db"));
        vault.save(None, db, "Hosts");
        vault.save(
            None,
            Host::new("bastion", "ops", "bastion.example.com", 22),
            "Hosts",
        );
        vault
            .identities
            .push(crate::identity::Identity::new("Ops", "ops", Auth::Agent));
        vault
    }

    #[test]
    fn adds_hosts_through_the_editors_checks() {
        let vault = vault();
        let Ok(Call::AddHost(fields)) = parse(
            "add_host",
            json!({
                "name": "web",
                "address": "web.example.com",
                "user": "deploy",
                "port": 2200,
                "tags": ["prod"],
                "jump": "bastion",
                "password": "secret",
                "env": { "LANG": "C.UTF-8" },
                "guarded": true,
            }),
        ) else {
            panic!("the fields are add_host's");
        };
        let mut draft = HostDraft::new(fields.group.as_deref().unwrap_or("Hosts"));
        fields.apply(&mut draft, &vault).unwrap();
        let host = draft.check(&vault, None).unwrap();
        assert_eq!(
            (
                host.name.as_str(),
                host.address.as_str(),
                host.port,
                host.user.as_str()
            ),
            ("web", "web.example.com", 2200, "deploy")
        );
        assert_eq!(host.auth, Auth::Password);
        assert_eq!(host.jump.as_deref(), Some("bastion"));
        assert_eq!(host.env.get("LANG").map(String::as_str), Some("C.UTF-8"));
        assert!(host.guarded && host.keep_alive);

        // What the editor would refuse, the check refuses.
        let mut draft = HostDraft::new("Hosts");
        HostFields {
            name: Some("DB".into()),
            address: Some("x".into()),
            ..Default::default()
        }
        .apply(&mut draft, &vault)
        .unwrap();
        let problems = draft.check(&vault, None).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|problem| problem.message == "Another host is already called that")
        );
        assert!(
            problems
                .iter()
                .any(|problem| problem.message == "Username is required")
        );
    }

    #[test]
    fn changes_only_the_fields_given() {
        let vault = vault();
        let db = vault.find("db").unwrap();
        let mut draft = HostDraft::edit(db, "Hosts");
        HostFields {
            name: Some("database".into()),
            identity: Some("ops".into()),
            sign_in: Some(SignIn::Key),
            ..Default::default()
        }
        .apply(&mut draft, &vault)
        .unwrap();
        // Sign in with a key keeps the key the host has.
        assert_eq!(draft.auth, Auth::Key(PathBuf::from("/keys/db")));
        assert_eq!(draft.name, "database");
        assert_eq!(draft.address, "10.0.0.5");
        assert_eq!(draft.identity, Some(vault.identities[0].id.clone()));

        let wrong = |fields: HostFields| {
            fields
                .apply(&mut HostDraft::new("Hosts"), &vault)
                .unwrap_err()
        };
        assert_eq!(
            wrong(HostFields {
                identity: Some("nobody".into()),
                ..Default::default()
            }),
            "No identity is called nobody"
        );
        assert_eq!(
            wrong(HostFields {
                sign_in: Some(SignIn::Key),
                ..Default::default()
            }),
            "Give the key file's path in key"
        );
        assert_eq!(
            wrong(HostFields {
                sign_in: Some(SignIn::Agent),
                password: Some("x".into()),
                ..Default::default()
            }),
            "A password goes with sign_in password"
        );
        // Empty clears.
        let mut draft = HostDraft::new("Hosts");
        draft.jump = Some("bastion".into());
        HostFields {
            jump: Some(String::new()),
            ..Default::default()
        }
        .apply(&mut draft, &vault)
        .unwrap();
        assert_eq!(draft.jump, None);
    }

    #[test]
    fn a_key_path_may_start_at_home() {
        let home = std::env::home_dir().unwrap();
        assert_eq!(
            home_path("~/.ssh/id_ed25519"),
            home.join(".ssh").join("id_ed25519")
        );
        assert_eq!(home_path("/etc/key"), PathBuf::from("/etc/key"));
        assert_eq!(home_path("~other/key"), PathBuf::from("~other/key"));
    }

    #[test]
    fn answers_the_protocol_and_passes_tool_calls_on() {
        let reply = |line: &str| match handle(line) {
            Handling::Reply(reply) => reply,
            other => panic!("{line} got {other:?}"),
        };
        let init = reply(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#,
        );
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "tern");
        assert!(init["result"]["capabilities"]["tools"].is_object());
        // A version this server doesn't know gets the one it speaks.
        let init = reply(
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
        );
        assert_eq!(init["result"]["protocolVersion"], VERSIONS[0]);

        let listed = reply(r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#);
        assert_eq!(listed["id"], "a");
        let tools = listed["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 9);
        for tool in tools {
            assert!(
                tool["description"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
            );
            assert_eq!(tool["inputSchema"]["type"], "object", "{}", tool["name"]);
        }

        assert_eq!(
            handle(
                r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"list_hosts"}}"#
            ),
            Handling::Call {
                id: json!(7),
                name: "list_hosts".into(),
                arguments: Value::Null
            }
        );
        let unknown =
            reply(r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"fly"}}"#);
        assert_eq!(unknown["error"]["code"], -32602);
        assert_eq!(
            reply(r#"{"jsonrpc":"2.0","id":9,"method":"resources/list"}"#)["error"]["code"],
            -32601
        );
        assert_eq!(reply("{not json")["error"]["code"], -32700);
        assert_eq!(
            reply(r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#)["result"],
            json!({})
        );
        // Notifications and replies need nothing.
        assert_eq!(
            handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Handling::Nothing
        );
        assert_eq!(
            handle(r#"{"jsonrpc":"2.0","id":4,"result":{}}"#),
            Handling::Nothing
        );
    }

    #[test]
    fn tokens_are_compared_whole() {
        let token = token();
        assert_eq!(token.len(), 64);
        assert!(same(&token, &token.clone()));
        assert!(!same(&token, &token[..63]));
        let mut other = token.clone().into_bytes();
        other[0] = if other[0] == b'0' { b'1' } else { b'0' };
        assert!(!same(&token, std::str::from_utf8(&other).unwrap()));
    }
}
