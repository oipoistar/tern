//! Tern against real OpenSSH servers: the lab in `dev/lab`, run in Docker.
//! It needs the lab up, so it only runs when asked for:
//!
//! ```text
//! dev/lab/setup.sh
//! cargo test -p tern-core --test lab -- --ignored --nocapture
//! ```
//!
//! Every check runs and prints how it went; the test fails at the end if
//! any didn't pass. What each server offers is in `dev/lab/entrypoint.sh`.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use russh::keys::PrivateKey;
use russh::keys::signature::Signer as _;
use russh::keys::ssh_key::Signature;
use russh::keys::ssh_key::encoding::Encode as _;
use tern_core::Vault;
use tern_core::forward::{Forward, ForwardKind};
use tern_core::proxy::Proxy;
use tern_core::sftp::Progress;
use tern_core::ssh::{
    ConnectError, ConnectRequest, Credential, Event, Prompts, Session, Target, TermSize, Trusted,
};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// The lab's servers, at their ports on this computer.
const PASSWORD: u16 = 2201;
const KEY: u16 = 2202;
const OTP: u16 = 2203;
const MFA: u16 = 2204;
const CERT: u16 = 2205;
const STRICT: u16 = 2206;
const LEGACY: u16 = 2207;
const BASTION: u16 = 2210;
const SOCKS: &str = "socks5://127.0.0.1:1080";
const HTTP: &str = "http://127.0.0.1:8888";

fn keys() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dev/lab/keys")
}

fn key(name: &str) -> Credential {
    Credential::KeyFile {
        path: keys().join(name),
        passphrase: None,
    }
}

fn ed25519() -> Vec<Credential> {
    vec![key("id_ed25519")]
}

fn password() -> Credential {
    Credential::Password("lab-pass".into())
}

/// A lab server published on this computer.
fn here(port: u16, user: &str) -> Target {
    Target {
        host: "127.0.0.1".into(),
        port,
        user: user.into(),
    }
}

/// A lab server only reachable inside the lab's networks, by its name.
fn inside(name: &str) -> Target {
    Target {
        host: name.into(),
        port: 22,
        user: "lab".into(),
    }
}

fn request(target: Target, credentials: Vec<Credential>) -> ConnectRequest {
    ConnectRequest {
        target,
        credentials,
        jumps: Vec::new(),
        proxy: None,
        size: TermSize {
            cols: 100,
            rows: 30,
        },
        trust: None,
        keep_alive: true,
        forwards: Vec::new(),
        agent_forwarding: false,
        env: Vec::new(),
        shell: true,
    }
}

fn through(mut request: ConnectRequest, jumps: Vec<(Target, Vec<Credential>)>) -> ConnectRequest {
    request.jumps = jumps;
    request
}

fn proxied(mut request: ConnectRequest, proxy: &str) -> ConnectRequest {
    request.proxy = Some(Proxy::parse(proxy).expect("the lab's proxy"));
    request
}

/// Runs `docker` with `args`; its output, or why it failed.
fn docker(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("docker")
        .args(args)
        .output()
        .map_err(|error| format!("docker: {error}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The lab's one-time code right now, as its servers work it out.
fn one_time_code() -> String {
    docker(&[
        "exec",
        "tern-lab-otp-1",
        "oathtool",
        "--totp",
        "-b",
        "JBSWY3DPEHPK3PXP",
    ])
    .expect("the otp server works out codes")
}

async fn next(session: &mut Session) -> Event {
    match tokio::time::timeout(Duration::from_secs(30), session.next_event()).await {
        Ok(Some(event)) => event,
        Ok(None) => Event::Failed(ConnectError::Other("the session ended".into())),
        Err(_) => Event::Failed(ConnectError::Other("nothing for 30 s".into())),
    }
}

/// Signs in as the app does. A host key not seen before is trusted when
/// `trust_new` (the user's yes); the server's questions go to `answer`,
/// which gives the replies, or `None` to give up.
async fn open_with(
    mut request: ConnectRequest,
    trust_new: bool,
    mut answer: impl FnMut(&Prompts) -> Option<Vec<String>>,
) -> Result<Session, ConnectError> {
    // The target and two jump hosts may each be new.
    for _ in 0..6 {
        let mut session = Session::open(request.clone());
        loop {
            match next(&mut session).await {
                Event::Connected => return Ok(session),
                Event::Forwarding { .. } => {}
                Event::Prompts(prompts) => match answer(&prompts) {
                    Some(replies) => session.answer(replies),
                    None => {
                        return Err(ConnectError::Other(format!(
                            "{} asked {:?}",
                            prompts.from, prompts.questions
                        )));
                    }
                },
                Event::Failed(ConnectError::UnknownHostKey {
                    host, port, key, ..
                }) if trust_new => {
                    request.trust = Some(Trusted {
                        host,
                        port,
                        key: *key,
                    });
                    break;
                }
                Event::Failed(error) => return Err(error),
                other => return Err(ConnectError::Other(format!("{other:?} first"))),
            }
        }
    }
    Err(ConnectError::Other("asked to trust key after key".into()))
}

async fn open(request: ConnectRequest) -> Result<Session, String> {
    open_with(request, true, |_| None)
        .await
        .map_err(|error| error.to_string())
}

/// Types `line` and returns what came back, up to a marker only its
/// output holds (the echoed line has `$((40+2))`, not 42).
async fn run(session: &mut Session, line: &str) -> Result<String, String> {
    session.write(format!("{line}; echo tern-$((40+2))-done\r").into_bytes());
    let mut seen = String::new();
    loop {
        match next(session).await {
            Event::Output(bytes) => {
                seen.push_str(&String::from_utf8_lossy(&bytes));
                if seen.contains("tern-42-done") {
                    return Ok(seen);
                }
            }
            Event::Forwarding { .. } => {}
            other => return Err(format!("{other:?} while running `{line}`: {seen:?}")),
        }
    }
}

/// What `expression` prints in the shell, echoed as `name=…`, which the
/// echoed line itself never shows (it has `name=$(`).
async fn value(session: &mut Session, name: &str, expression: &str) -> Result<String, String> {
    let out = run(session, &format!("echo \"{name}=$({expression})\"")).await?;
    let marker = format!("{name}=");
    out.split(['\r', '\n'])
        .filter_map(|line| line.split(&marker).nth(1))
        .find(|value| !value.starts_with("$("))
        .map(|value| value.trim().to_string())
        .ok_or_else(|| format!("no {name} in {out:?}"))
}

fn expect(what: &str, got: String, wanted: &str) -> Result<String, String> {
    if got == wanted {
        Ok(format!("{what} {got}"))
    } else {
        Err(format!("{what} {got:?}, not {wanted:?}"))
    }
}

/// Signs in and says who the shell runs as.
async fn signs_in_as(request: ConnectRequest, user: &str) -> Result<String, String> {
    let mut session = open(request).await?;
    expect(
        "signed in as",
        value(&mut session, "user", "whoami").await?,
        user,
    )
}

/// Signs in and says which lab machine the shell is on.
async fn reaches(request: ConnectRequest, host: &str) -> Result<String, String> {
    let mut session = open(request).await?;
    expect(
        "reached",
        value(&mut session, "host", "hostname").await?,
        host,
    )
}

async fn refused(
    request: ConnectRequest,
    wanted: fn(&ConnectError) -> bool,
) -> Result<String, String> {
    match open_with(request, true, |_| None).await {
        Ok(_) => Err("signed in".into()),
        Err(error) if wanted(&error) => Ok(format!("refused: {error}")),
        Err(error) => Err(format!("refused the wrong way: {error:?}")),
    }
}

/// `GET /hello.txt` from the lab's web server over `stream`.
async fn hello(mut stream: TcpStream) -> Result<String, String> {
    stream
        .write_all(b"GET /hello.txt HTTP/1.0\r\nHost: web\r\n\r\n")
        .await
        .map_err(|error| error.to_string())?;
    let mut reply = String::new();
    stream
        .read_to_string(&mut reply)
        .await
        .map_err(|error| error.to_string())?;
    reply
        .split("\r\n\r\n")
        .nth(1)
        .map(|body| body.trim().to_string())
        .ok_or_else(|| format!("no body in {reply:?}"))
}

/// Connects to a port a rule listens on, once it does.
async fn listening(port: u16) -> Result<TcpStream, String> {
    for _ in 0..50 {
        if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)).await {
            return Ok(stream);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!("nothing listens on {port}"))
}

const HELLO: &str = "hello from the lab's web server";

struct Lab {
    failed: Vec<String>,
    passed: usize,
}

impl Lab {
    async fn check(&mut self, name: &str, check: impl Future<Output = Result<String, String>>) {
        let result = tokio::time::timeout(Duration::from_secs(120), check)
            .await
            .unwrap_or_else(|_| Err("timed out".into()));
        match result {
            Ok(detail) => {
                println!("ok    {name}: {detail}");
                self.passed += 1;
            }
            Err(problem) => {
                println!("FAIL  {name}: {problem}");
                self.failed.push(format!("{name}: {problem}"));
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the SSH lab: dev/lab/setup.sh"]
async fn lab() {
    let scratch = std::env::temp_dir().join(format!("tern-lab-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    // SAFETY: set before any session, and so any other thread, starts.
    unsafe {
        std::env::set_var("TERN_DATA_DIR", scratch.join("data"));
        // Trusts the certificate server's host CA.
        std::env::set_var("TERN_SYSTEM_KNOWN_HOSTS", keys().join("known_hosts"));
        // No one's own keys are tried.
        std::env::set_var("TERN_SSH_DIR", scratch.join("ssh"));
    }
    serve_agent(&scratch);
    let mut lab = Lab {
        failed: Vec::new(),
        passed: 0,
    };

    // Host keys first, while Tern knows none of the lab's.
    lab.check("a new host's key is asked about, then remembered", async {
        let ask = || request(here(PASSWORD, "lab"), vec![password()]);
        match open_with(ask(), false, |_| None).await {
            Err(ConnectError::UnknownHostKey { fingerprint, .. }) => {
                open(ask()).await?;
                open_with(ask(), false, |_| None)
                    .await
                    .map_err(|error| format!("asked again: {error}"))?;
                Ok(format!("asked once about {fingerprint}"))
            }
            other => Err(format!("not asked: {:?}", other.err())),
        }
    })
    .await;

    // Signing in.
    lab.check(
        "password",
        signs_in_as(request(here(PASSWORD, "lab"), vec![password()]), "lab"),
    )
    .await;
    lab.check(
        "a wrong password is refused",
        refused(
            request(
                here(PASSWORD, "lab"),
                vec![Credential::Password("nope".into())],
            ),
            |error| matches!(error, ConnectError::AuthFailed { .. }),
        ),
    )
    .await;
    for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
        lab.check(
            name,
            signs_in_as(request(here(KEY, "lab"), vec![key(name)]), "lab"),
        )
        .await;
    }
    lab.check(
        "an encrypted key with its passphrase",
        signs_in_as(
            request(
                here(KEY, "lab"),
                vec![Credential::KeyFile {
                    path: keys().join("id_locked"),
                    passphrase: Some("lab-pass".into()),
                }],
            ),
            "lab",
        ),
    )
    .await;
    lab.check("an encrypted key asks for its passphrase", async {
        let mut asked = Vec::new();
        let mut session = open_with(
            request(here(KEY, "lab"), vec![key("id_locked")]),
            true,
            |prompts| {
                asked.extend(
                    prompts
                        .questions
                        .iter()
                        .map(|(question, _)| question.clone()),
                );
                Some(vec!["lab-pass".into()])
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        let user = value(&mut session, "user", "whoami").await?;
        expect(&format!("asked {asked:?}, signed in as"), user, "lab")
    })
    .await;
    lab.check("three wrong passphrases end the sign-in", async {
        let mut asked = 0;
        match open_with(
            request(here(KEY, "lab"), vec![key("id_locked")]),
            true,
            |_| {
                asked += 1;
                Some(vec!["nope".into()])
            },
        )
        .await
        {
            Err(error) if asked == 3 => Ok(format!("asked 3 times: {error}")),
            Err(error) => Err(format!("asked {asked} times: {error}")),
            Ok(_) => Err("signed in".into()),
        }
    })
    .await;
    lab.check("a key the server won't take isn't unlocked", async {
        // The password server takes no keys: no passphrase asked, and the
        // password after it still signs in.
        let mut asked = Vec::new();
        let mut session = open_with(
            request(here(PASSWORD, "lab"), vec![key("id_locked"), password()]),
            true,
            |prompts| {
                asked.push(format!("{:?}", prompts.questions));
                None
            },
        )
        .await
        .map_err(|error| format!("{error}, asked {asked:?}"))?;
        expect(
            "not asked, signed in as",
            value(&mut session, "user", "whoami").await?,
            "lab",
        )
    })
    .await;
    lab.check("password, then a one-time code", async {
        let mut asked = Vec::new();
        let mut session = open_with(
            request(here(OTP, "lab"), vec![password()]),
            true,
            |prompts| {
                asked.extend(
                    prompts
                        .questions
                        .iter()
                        .map(|(question, _)| question.clone()),
                );
                Some(vec![one_time_code()])
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        let user = value(&mut session, "user", "whoami").await?;
        // The password answers its own question; only the code is asked.
        match asked.as_slice() {
            [code] if code.contains("code") => {
                expect(&format!("asked {asked:?}, signed in as"), user, "lab")
            }
            _ => Err(format!("asked {asked:?}")),
        }
    })
    .await;
    lab.check("a wrong one-time code is refused", async {
        match open_with(request(here(OTP, "lab"), vec![password()]), true, |_| {
            Some(vec!["000000".into()])
        })
        .await
        {
            Err(ConnectError::AuthFailed { tried }) => Ok(format!("refused, tried {tried:?}")),
            Err(error) => Err(format!("refused the wrong way: {error}")),
            Ok(_) => Err("signed in".into()),
        }
    })
    .await;
    lab.check("a key, then a one-time code", async {
        let mut session = open_with(request(here(MFA, "lab"), ed25519()), true, |_| {
            Some(vec![one_time_code()])
        })
        .await
        .map_err(|error| error.to_string())?;
        expect(
            "signed in as",
            value(&mut session, "user", "whoami").await?,
            "lab",
        )
    })
    .await;
    lab.check("user and host certificates", async {
        // The host's certificate is trusted through its CA: no question.
        let mut session = open_with(
            request(here(CERT, "carol"), vec![key("id_carol")]),
            false,
            |_| None,
        )
        .await
        .map_err(|error| error.to_string())?;
        expect(
            "signed in as",
            value(&mut session, "user", "whoami").await?,
            "carol",
        )
    })
    .await;
    lab.check(
        "only today's algorithms",
        signs_in_as(request(here(STRICT, "lab"), ed25519()), "lab"),
    )
    .await;
    lab.check(
        "older algorithms",
        signs_in_as(request(here(LEGACY, "lab"), vec![password()]), "lab"),
    )
    .await;

    // The agent (the stand-in this test serves).
    lab.check(
        "the SSH agent",
        signs_in_as(request(here(KEY, "lab"), vec![Credential::Agent]), "lab"),
    )
    .await;
    lab.check("the SSH agent, forwarded", async {
        let mut ask = request(here(BASTION, "lab"), vec![Credential::Agent]);
        ask.agent_forwarding = true;
        let mut session = open(ask).await?;
        // The bastion's own ssh signs in to hop2 through it.
        let ssh = "ssh -o StrictHostKeyChecking=no -o BatchMode=yes lab@hop2 hostname";
        expect(
            "the bastion's ssh reached",
            value(&mut session, "host", ssh).await?,
            "hop2",
        )
    })
    .await;
    lab.check("no agent unless it's forwarded", async {
        let mut session = open(request(here(BASTION, "lab"), ed25519())).await?;
        // ssh-add exits with 2 when it can't reach an agent.
        let code = value(&mut session, "code", "ssh-add -l >/dev/null 2>&1; echo $?").await?;
        expect("ssh-add exited with", code, "2")
    })
    .await;

    // Jump hosts and proxies.
    let bastion = || (here(BASTION, "lab"), ed25519());
    lab.check(
        "a jump host",
        reaches(
            through(request(inside("hop2"), ed25519()), vec![bastion()]),
            "hop2",
        ),
    )
    .await;
    lab.check(
        "two jump hosts",
        reaches(
            through(
                request(inside("deep"), ed25519()),
                vec![bastion(), (inside("hop2"), ed25519())],
            ),
            "deep",
        ),
    )
    .await;
    lab.check(
        "a jump host's saved password",
        reaches(
            through(
                request(inside("hop2"), ed25519()),
                vec![(here(BASTION, "lab"), vec![password()])],
            ),
            "hop2",
        ),
    )
    .await;
    lab.check(
        "a jump host refusing its password isn't retried",
        refused(
            through(
                request(inside("hop2"), ed25519()),
                vec![(
                    here(BASTION, "lab"),
                    vec![Credential::Password("nope".into())],
                )],
            ),
            |error| matches!(error, ConnectError::JumpRefused(_)),
        ),
    )
    .await;
    lab.check("a jump host asking its password as a question", async {
        let mut asked = Vec::new();
        let mut session = open_with(
            through(
                request(inside("deep"), ed25519()),
                vec![bastion(), (inside("hop2"), Vec::new())],
            ),
            true,
            |prompts| {
                asked.push(format!("{} {:?}", prompts.from, prompts.questions));
                Some(vec!["lab-pass".into()])
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        expect(
            &format!("asked {asked:?}, reached"),
            value(&mut session, "host", "hostname").await?,
            "deep",
        )
    })
    .await;
    lab.check(
        "a SOCKS5 proxy (the proxy finds the name)",
        reaches(proxied(request(inside("hop2"), ed25519()), SOCKS), "hop2"),
    )
    .await;
    lab.check(
        "an HTTP proxy",
        reaches(proxied(request(inside("hop2"), ed25519()), HTTP), "hop2"),
    )
    .await;
    lab.check(
        "a proxy, then a jump host",
        reaches(
            proxied(
                through(
                    request(inside("deep"), ed25519()),
                    vec![(inside("hop2"), ed25519())],
                ),
                SOCKS,
            ),
            "deep",
        ),
    )
    .await;

    // Port forwarding, through the bastion to the web server inside.
    lab.check("local forwarding", async {
        let mut ask = request(here(BASTION, "lab"), ed25519());
        ask.forwards = vec![Forward::typed(ForwardKind::Local, "18080", "web:8000")?];
        let _session = open(ask).await?;
        expect("got", hello(listening(18080).await?).await?, HELLO)
    })
    .await;
    lab.check("dynamic forwarding", async {
        let mut ask = request(here(BASTION, "lab"), ed25519());
        ask.forwards = vec![Forward::typed(ForwardKind::Dynamic, "18081", "")?];
        let _session = open(ask).await?;
        drop(listening(18081).await?);
        let socks = Proxy::parse("socks5://127.0.0.1:18081")?;
        let stream = tern_core::proxy::connect(&socks, "web", 8000).await?;
        expect("got", hello(stream).await?, HELLO)
    })
    .await;
    lab.check("remote forwarding", async {
        // A server here, answering what the bastion forwards back.
        let server = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| error.to_string())?;
        let port = server
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = server.accept().await {
                let mut request = [0u8; 1024];
                let _ = stream.read(&mut request).await;
                let _ = stream
                    .write_all(b"HTTP/1.0 200 OK\r\n\r\nhello from this computer")
                    .await;
            }
        });
        let mut ask = request(here(BASTION, "lab"), ed25519());
        ask.forwards = vec![Forward::typed(
            ForwardKind::Remote,
            "19000",
            &format!("127.0.0.1:{port}"),
        )?];
        let mut session = open(ask).await?;
        // The rule may take a moment after the shell starts.
        let got = value(&mut session, "remote", "sleep 1; curl -s localhost:19000/").await?;
        expect("the bastion got", got, "hello from this computer")
    })
    .await;
    lab.check("forwarding without a terminal", async {
        let mut ask = request(here(BASTION, "lab"), ed25519());
        ask.shell = false;
        ask.forwards = vec![Forward::typed(ForwardKind::Local, "18082", "web:8000")?];
        let _session = open(ask).await?;
        expect("got", hello(listening(18082).await?).await?, HELLO)
    })
    .await;

    // SFTP against OpenSSH's own server.
    lab.check("files over SFTP", sftp(&scratch)).await;

    // The session itself.
    lab.check("environment variables", async {
        let mut ask = request(here(KEY, "lab"), ed25519());
        ask.env = vec![("TERN_LAB".into(), "from tern".into())];
        let mut session = open(ask).await?;
        expect(
            "the shell has",
            value(&mut session, "env", "printenv TERN_LAB").await?,
            "from tern",
        )
    })
    .await;
    lab.check("the terminal's size follows the window", async {
        let mut session = open(request(here(KEY, "lab"), ed25519())).await?;
        session.resize(TermSize {
            cols: 120,
            rows: 40,
        });
        expect(
            "stty says",
            value(&mut session, "size", "stty size").await?,
            "40 120",
        )
    })
    .await;
    lab.check("the shell's exit status", async {
        let mut session = open(request(here(KEY, "lab"), ed25519())).await?;
        session.write(b"exit 3\r".to_vec());
        loop {
            match next(&mut session).await {
                Event::Output(_) => {}
                Event::Closed { exit_status } => {
                    break expect("closed with", format!("{exit_status:?}"), "Some(3)");
                }
                other => break Err(format!("{other:?}")),
            }
        }
    })
    .await;
    lab.check(
        "a restarted server is a lost connection, then back",
        async {
            let mut session = open(request(here(KEY, "lab"), ed25519())).await?;
            docker(&["restart", "-t", "1", "tern-lab-key-1"])?;
            let ended = loop {
                match next(&mut session).await {
                    Event::Output(_) | Event::Forwarding { .. } => {}
                    event => break event,
                }
            };
            if !matches!(ended, Event::Lost) {
                return Err(format!(
                    "ended with {ended:?}, so the app wouldn't reconnect"
                ));
            }
            // Back once the server is: what the app's reconnect does.
            for _ in 0..20 {
                if let Ok(mut session) = open(request(here(KEY, "lab"), ed25519())).await {
                    return expect(
                        "lost, then back on",
                        value(&mut session, "host", "hostname").await?,
                        "key",
                    );
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err("never back".into())
        },
    )
    .await;

    // ~/.ssh/config, imported and signed in through as written.
    lab.check("ssh_config hosts connect as configured", import(&scratch))
        .await;
    lab.check("copied for OpenSSH, run by OpenSSH", export())
        .await;

    // Last, as it changes the server's key for good.
    lab.check("a changed host key is refused, naming its line", async {
        docker(&[
            "exec",
            "tern-lab-key-1",
            "sh",
            "-c",
            "rm -f /etc/ssh/ssh_host_* && ssh-keygen -A >/dev/null && kill -HUP 1",
        ])?;
        tokio::time::sleep(Duration::from_secs(1)).await;
        match open_with(request(here(KEY, "lab"), ed25519()), false, |_| None).await {
            Err(ConnectError::HostKeyChanged { file, line, .. }) => {
                Ok(format!("refused; {}:{line}", file.display()))
            }
            other => Err(format!("{:?}", other.err())),
        }
    })
    .await;

    println!("\n{} passed, {} failed", lab.passed, lab.failed.len());
    assert!(lab.failed.is_empty(), "failed:\n{}", lab.failed.join("\n"));
}

/// Copies hosts as an ssh command and as config blocks, and runs both with
/// OpenSSH in a container on the lab's network (it has `nc` for proxies):
/// two jump hosts, and a jump host behind the SOCKS proxy.
async fn export() -> Result<String, String> {
    let host =
        |name: &str, address: &str, jump: Option<&str>, proxy: Option<&str>| tern_core::Host {
            auth: tern_core::Auth::Key("/root/.ssh/id_ed25519".into()),
            jump: jump.map(str::to_string),
            proxy: proxy.map(|proxy| Proxy::parse(proxy).expect("a proxy")),
            ..tern_core::Host::new(name, "lab", address, 22)
        };
    let mut vault = Vault::default();
    for saved in [
        host("bastion", "bastion", None, None),
        host("hop2", "hop2", Some("bastion"), None),
        host("deep", "deep", Some("hop2"), None),
        host("hop2-proxied", "hop2", None, Some("socks5://socks:1080")),
        host("deep-proxied", "deep", Some("hop2-proxied"), None),
    ] {
        vault.save(None, saved, "Lab");
    }
    let find = |name: &str| vault.find(name).expect("saved");
    let command = |name: &str| tern_core::openssh::ssh_command(&vault, find(name));
    let config = |name: &str| tern_core::openssh::config_block(&vault, find(name));
    // OpenSSH passes no -o down to the ssh a jump runs, but every one reads
    // ~/.ssh/config: the lab's defaults go there, then the copied blocks.
    let script = format!(
        "set -e\n\
         install -d -m 700 /root/.ssh\n\
         install -m 600 /lab/keys/id_ed25519 /root/.ssh/id_ed25519\n\
         cat > /root/.ssh/config <<'EOF'\n\
         Host *\n  StrictHostKeyChecking no\n  UserKnownHostsFile /dev/null\n  BatchMode yes\n  \
         LogLevel ERROR\n  IdentityFile /root/.ssh/id_ed25519\n\
         {}\n{}\nEOF\n\
         echo \"command=$({} hostname)\"\n\
         echo \"proxied=$({} hostname)\"\n\
         echo \"config=$(ssh deep hostname)\"\n\
         echo \"config-proxied=$(ssh deep-proxied hostname)\"\n",
        config("deep"),
        config("deep-proxied"),
        command("deep"),
        command("deep-proxied"),
    );
    let keys = std::fs::canonicalize(keys()).map_err(|error| error.to_string())?;
    // Docker takes C:\…, not the \\?\C:\… a canonical path on Windows is.
    let keys = keys.display().to_string().replace(r"\\?\", "");
    let out = docker(&[
        "run",
        "--rm",
        "--network",
        "tern-lab_edge",
        "--entrypoint",
        "bash",
        "-v",
        &format!("{keys}:/lab/keys:ro"),
        "tern-lab",
        "-c",
        &script,
    ])?;
    let wanted = "command=deep\nproxied=deep\nconfig=deep\nconfig-proxied=deep";
    if out != wanted {
        return Err(format!("{out:?} from\n{script}"));
    }
    Ok(format!(
        "{} and {}, as commands and as config",
        command("deep"),
        command("deep-proxied")
    ))
}

/// Serves an SSH agent holding the lab's ed25519 key where `SSH_AUTH_SOCK`
/// points: what Tern signs in with as the agent, and forwards. (Windows' own
/// agent only runs as a service, which takes an administrator to enable.)
fn serve_agent(scratch: &Path) {
    let key = Arc::new(
        russh::keys::load_secret_key(keys().join("id_ed25519"), None).expect("the lab's key"),
    );
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ServerOptions;
        let pipe = format!(r"\\.\pipe\tern-lab-agent-{}", std::process::id());
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe)
            .expect("a pipe for the agent");
        // SAFETY: set before any session, and so any other thread, starts.
        unsafe { std::env::set_var("SSH_AUTH_SOCK", &pipe) };
        tokio::spawn(async move {
            while server.connect().await.is_ok() {
                let Ok(next) = ServerOptions::new().create(&pipe) else {
                    return;
                };
                let connected = std::mem::replace(&mut server, next);
                tokio::spawn(agent(connected, key.clone()));
            }
        });
    }
    #[cfg(unix)]
    {
        let path = scratch.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&path).expect("a socket for the agent");
        // SAFETY: set before any session, and so any other thread, starts.
        unsafe { std::env::set_var("SSH_AUTH_SOCK", &path) };
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(agent(stream, key.clone()));
            }
        });
    }
    let _ = scratch;
}

/// Answers one client as an agent does: lists the key, signs with it, and
/// fails anything else.
async fn agent<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, key: Arc<PrivateKey>) {
    loop {
        let mut length = [0u8; 4];
        if stream.read_exact(&mut length).await.is_err() {
            return;
        }
        let mut message = vec![0u8; u32::from_be_bytes(length) as usize];
        if stream.read_exact(&mut message).await.is_err() {
            return;
        }
        // SSH_AGENT_FAILURE for anything it can't do.
        let reply = agent_reply(&key, &message).unwrap_or_else(|| vec![5]);
        let mut framed = (reply.len() as u32).to_be_bytes().to_vec();
        framed.extend_from_slice(&reply);
        if stream.write_all(&framed).await.is_err() {
            return;
        }
    }
}

fn agent_reply(key: &PrivateKey, message: &[u8]) -> Option<Vec<u8>> {
    let blob = key.public_key().to_bytes().ok()?;
    let string = |bytes: &[u8]| [&(bytes.len() as u32).to_be_bytes()[..], bytes].concat();
    match message.split_first()? {
        // REQUEST_IDENTITIES: the one key.
        (11, _) => Some(
            [
                &[12][..],
                &1u32.to_be_bytes(),
                &string(&blob),
                &string(b"lab agent"),
            ]
            .concat(),
        ),
        // SIGN_REQUEST: key, data, flags.
        (13, rest) => {
            let mut rest: &[u8] = rest;
            if take(&mut rest)? != blob.as_slice() {
                return None;
            }
            let signature: Signature = key.try_sign(take(&mut rest)?).ok()?;
            Some([&[14][..], &string(&signature.encode_vec().ok()?)].concat())
        }
        _ => None,
    }
}

/// An SSH string off the front of `rest`.
fn take<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let (length, after) = rest.split_first_chunk::<4>()?;
    let (taken, after) = after.split_at_checked(u32::from_be_bytes(*length) as usize)?;
    *rest = after;
    Some(taken)
}

/// Lists, copies both ways, renames (case alone too), makes folders, sets
/// permissions and deletes on OpenSSH's SFTP server, and refuses a name
/// that would be a path here.
async fn sftp(scratch: &Path) -> Result<String, String> {
    let session = open(request(here(KEY, "lab"), ed25519())).await?;
    let (input, _events) = session.into_parts();
    let sftp = input.open_sftp().await?;
    let home = sftp.home().await?;
    let local = scratch.join("sftp-here");
    let back = scratch.join("sftp-back");
    std::fs::create_dir_all(&local).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&back).map_err(|error| error.to_string())?;
    let name = "héllo wörld.txt";
    let content: Vec<u8> = (0..200_000u32).map(|n| (n % 251) as u8).collect();
    std::fs::write(local.join(name), &content).map_err(|error| error.to_string())?;

    let up = sftp
        .upload(local.join(name), &home, Progress::default())
        .await?;
    let down = sftp
        .download(&up, back.clone(), Progress::default())
        .await?;
    if std::fs::read(&down).map_err(|error| error.to_string())? != content {
        return Err("the file came back different".into());
    }
    let folder = format!("{home}/box");
    sftp.mkdir(&folder).await?;
    let moved = format!("{folder}/notes.txt");
    sftp.rename(&up, &moved).await?;
    sftp.rename(&moved, &format!("{folder}/NOTES.txt")).await?;
    sftp.set_mode(&format!("{folder}/NOTES.txt"), 0o600).await?;
    let listed = sftp.list(&folder).await?;
    match listed.as_slice() {
        [entry] if entry.name == "NOTES.txt" && entry.mode == Some(0o600) => {}
        _ => return Err(format!("the folder holds {listed:?}")),
    }
    // A control socket inside, as ssh's ControlMaster leaves them.
    docker(&[
        "exec",
        "-u",
        "lab",
        "tern-lab-key-1",
        "python3",
        "-c",
        "import socket; socket.socket(socket.AF_UNIX).bind('/home/lab/box/cm.sock')",
    ])?;
    sftp.remove(&folder, true).await?;
    if sftp
        .list(&home)
        .await?
        .iter()
        .any(|entry| entry.name == "box")
    {
        return Err("the folder is still there".into());
    }
    // A name a Linux host allows that would be a path on Windows.
    docker(&[
        "exec",
        "-u",
        "lab",
        "tern-lab-key-1",
        "touch",
        r"/home/lab/..\odd.txt",
    ])?;
    let odd = sftp
        .download(
            &format!(r"{home}/..\odd.txt"),
            back.clone(),
            Progress::default(),
        )
        .await;
    let refused = match (cfg!(windows), odd) {
        (true, Err(problem)) => format!("refused ..\\odd.txt: {problem}"),
        (true, Ok(path)) => return Err(format!("..\\odd.txt landed at {}", path.display())),
        (false, _) => "..\\odd.txt is a plain name here".to_string(),
    };
    Ok(format!(
        "{} KB both ways, renamed, chmod 600, deleted with a socket inside; {refused}",
        content.len() / 1000
    ))
}

/// Imports an ssh_config describing the lab, then signs in to its hosts
/// as the vault resolves them: through jump hosts and a proxy, with
/// environment variables.
async fn import(scratch: &Path) -> Result<String, String> {
    let key = keys().join("id_ed25519");
    let key = key.display();
    let config = format!(
        "Host lab-bastion\n  HostName 127.0.0.1\n  Port {BASTION}\n  User lab\n  IdentityFile \"{key}\"\n\
         Host lab-hop2\n  HostName hop2\n  User lab\n  IdentityFile \"{key}\"\n  ProxyJump lab-bastion\n\
         Host lab-deep\n  HostName deep\n  User lab\n  IdentityFile \"{key}\"\n  ProxyJump lab-hop2\n\
         Host lab-proxied\n  HostName hop2\n  User lab\n  IdentityFile \"{key}\"\n  ProxyCommand nc -X 5 -x 127.0.0.1:1080 %h %p\n\
         Host lab-env\n  HostName 127.0.0.1\n  Port {KEY}\n  User lab\n  IdentityFile \"{key}\"\n  SetEnv TERN_LAB=imported\n"
    );
    let hosts = tern_core::openssh::importable(&Vault::default(), &config, scratch, "lab");
    let mut vault = Vault::default();
    vault.import(hosts, tern_core::openssh::GROUP);
    let size = TermSize {
        cols: 100,
        rows: 30,
    };
    let mut reached = Vec::new();
    for (name, check, wanted) in [
        ("lab-deep", "hostname", "deep"),
        ("lab-proxied", "hostname", "hop2"),
        ("lab-env", "printenv TERN_LAB", "imported"),
    ] {
        let host = vault.find(name).ok_or(format!("{name} wasn't imported"))?;
        let connection = vault.connection(host)?;
        let mut session = open(connection.request(size, None, None, |_| None)).await?;
        let got = value(&mut session, "got", check).await?;
        if got != wanted {
            return Err(format!("{name}: {got:?}, not {wanted:?}"));
        }
        reached.push(format!("{name} → {got}"));
    }
    Ok(reached.join(", "))
}
