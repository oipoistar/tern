//! Adds the SSH lab's servers (`dev/lab`) to Tern's vault, in a group of
//! their own: each way of signing in, the jump chain into its inner
//! networks, its two proxies, and port forwarding through the bastion. Quit
//! Tern first: it keeps the vault in memory and would save over the change.
//!
//! ```text
//! cargo run -p tern-core --example lab_hosts
//! cargo run -p tern-core --example lab_hosts -- --check
//! ```
//!
//! Running it again updates the lab's hosts in place. `--check` changes
//! nothing; it signs in to each lab host as the vault has it, answering what
//! the servers ask with the lab's password and codes. The host keys it
//! trusts go to a scratch folder, so Tern still asks about each one.

use std::path::Path;
use std::time::Duration;

use tern_core::env::Env;
use tern_core::forward::{Forward, ForwardKind};
use tern_core::identity::Identity;
use tern_core::proxy::{self, Proxy};
use tern_core::ssh::{ConnectError, Event, Session, TermSize, Trusted};
use tern_core::{Auth, Host, Vault, paths, vault};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

/// The group the lab's hosts go in.
const GROUP: &str = "Docker lab";
/// The password of `lab` on every server, and the encrypted key's
/// passphrase (see `entrypoint.sh` and `setup.sh`).
const PASSWORD: &str = "lab-pass";
/// The secret the one-time codes come from.
const CODES: &str = "JBSWY3DPEHPK3PXP";
/// What the web server inside the lab answers.
const HELLO: &str = "hello from the lab's web server";
const SIZE: TermSize = TermSize {
    cols: 100,
    rows: 30,
};

fn main() -> Result<(), String> {
    // The vault open in Tern.
    let open = tern_core::vaults::Vaults::load(&paths::vaults()).current;
    let path = paths::vault(&open);
    let opened = vault::open(&path);
    if let Some(notice) = opened.notice {
        return Err(notice);
    }
    let mut vault = opened.vault;
    if std::env::args().any(|arg| arg == "--check") {
        return check(&vault);
    }
    let keys = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or("no repository around this crate")?
        .join("dev")
        .join("lab")
        .join("keys");
    if !keys.join("id_ed25519").is_file() {
        return Err(format!(
            "no lab keys in {}: run dev/lab/setup.sh first",
            keys.display()
        ));
    }
    let count = fill(&mut vault, &keys)?;
    vault.save_to(&path).map_err(|error| error.to_string())?;
    println!("{count} lab hosts in {GROUP}, in {}", path.display());
    Ok(())
}

/// Puts the lab's hosts in the vault, updating ones of the same name, with
/// the identity most of them sign in with; says how many there are.
fn fill(vault: &mut Vault, keys: &Path) -> Result<usize, String> {
    let key = |name: &str| Auth::Key(keys.join(name));
    let mut identity = Identity::new("lab", "lab", key("id_ed25519"));
    if let Some(old) = vault.identities.iter().find(|old| old.name == "lab") {
        identity.id = old.id.clone();
        identity.unknown = old.unknown.clone();
    }
    let lab = identity.id.clone();
    vault.save_identity(identity);

    let host = |name: &str, port: u16, tags: &[&str], notes: &str| Host {
        os: "Debian 12".into(),
        tags: ["lab"]
            .iter()
            .chain(tags)
            .map(|tag| tag.to_string())
            .collect(),
        notes: notes.into(),
        ..Host::new(name, "lab", "127.0.0.1", port)
    };
    let as_lab = |host: Host| Host {
        identity: Some(lab.clone()),
        ..host
    };
    let inside = |name: &str, address: &str, tag: &str, notes: &str| Host {
        address: address.into(),
        ..as_lab(host(name, 22, &[tag], notes))
    };
    let hosts = [
        Host {
            auth: Auth::Password,
            ..host(
                "lab-password",
                2201,
                &["password"],
                "Signs in with a password only.",
            )
        },
        Host {
            env: Env::from([("TERN_LAB".to_string(), "from Tern".to_string())]),
            ..as_lab(host(
                "lab-key",
                2202,
                &["key"],
                "Signs in with keys only. The shell starts with TERN_LAB set.",
            ))
        },
        Host {
            auth: key("id_locked"),
            ..host(
                "lab-passphrase",
                2202,
                &["key"],
                "Signs in with an encrypted key; its passphrase is the lab's password.",
            )
        },
        Host {
            auth: Auth::Password,
            ..host(
                "lab-otp",
                2203,
                &["otp"],
                "Asks for a password, then a one-time code.",
            )
        },
        as_lab(host(
            "lab-mfa",
            2204,
            &["otp"],
            "Takes a key, then asks for a one-time code.",
        )),
        Host {
            user: "carol".into(),
            auth: key("id_carol"),
            ..host(
                "lab-cert",
                2205,
                &["cert"],
                "carol has no authorized keys: the certificate beside her key signs her in. \
                 The server shows a host certificate.",
            )
        },
        as_lab(host(
            "lab-strict",
            2206,
            &["algorithms"],
            "Offers only today's algorithms.",
        )),
        Host {
            auth: key("id_rsa"),
            ..host(
                "lab-legacy",
                2207,
                &["algorithms"],
                "Offers older algorithms: an RSA host key, finite-field key exchange, \
                 CTR ciphers and SHA-1 MACs.",
            )
        },
        Host {
            forwards: vec![
                Forward::typed(ForwardKind::Local, "18000", "web:8000")?,
                Forward::typed(ForwardKind::Dynamic, "18090", "")?,
            ],
            ..as_lab(host(
                "lab-bastion",
                2210,
                &["jump", "forwarding"],
                "The way into the lab's inner network. Forwards 127.0.0.1:18000 to the web \
                 server inside, and runs a SOCKS proxy on 127.0.0.1:18090.",
            ))
        },
        Host {
            jump: Some("lab-bastion".into()),
            ..inside(
                "lab-hop2",
                "hop2",
                "jump",
                "Inside the lab, through lab-bastion.",
            )
        },
        Host {
            jump: Some("lab-hop2".into()),
            ..inside(
                "lab-deep",
                "deep",
                "jump",
                "Two networks in, through lab-bastion and lab-hop2.",
            )
        },
        Host {
            proxy: Some(Proxy::parse("socks5://127.0.0.1:1080")?),
            ..inside(
                "lab-hop2-socks",
                "hop2",
                "proxy",
                "lab-hop2 again, through the lab's SOCKS5 proxy.",
            )
        },
        Host {
            proxy: Some(Proxy::parse("http://127.0.0.1:8888")?),
            ..inside(
                "lab-hop2-http",
                "hop2",
                "proxy",
                "lab-hop2 again, through the lab's HTTP proxy.",
            )
        },
    ];
    let count = hosts.len();
    for mut host in hosts {
        let old = vault
            .find(&host.name)
            .map(|old| (old.id.clone(), old.unknown.clone()));
        let replacing = old.is_some().then(|| host.name.clone());
        if let Some((id, unknown)) = old {
            host.id = id;
            host.unknown = unknown;
        }
        vault.save(replacing.as_deref(), host, GROUP);
    }
    for group in vault.groups.iter().filter(|group| group.name == GROUP) {
        for host in &group.hosts {
            vault.connection(host)?;
        }
    }
    Ok(count)
}

/// Signs in to each of the lab's hosts; fails if any didn't.
fn check(vault: &Vault) -> Result<(), String> {
    let scratch = std::env::temp_dir().join(format!("tern-lab-hosts-{}", std::process::id()));
    // SAFETY: set before any session, and so any other thread, starts.
    unsafe { std::env::set_var("TERN_DATA_DIR", &scratch) };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let hosts: Vec<&Host> = vault
        .groups
        .iter()
        .filter(|group| group.name == GROUP)
        .flat_map(|group| &group.hosts)
        .collect();
    if hosts.is_empty() {
        return Err(format!("no {GROUP} group in the vault"));
    }
    let mut failed = 0;
    runtime.block_on(async {
        for host in hosts {
            match sign_in(vault, host).await {
                Ok(what) => println!("ok    {}: {what}", host.name),
                Err(why) => {
                    println!("FAIL  {}: {why}", host.name);
                    failed += 1;
                }
            }
        }
    });
    let _ = std::fs::remove_dir_all(&scratch);
    match failed {
        0 => Ok(()),
        failed => Err(format!("{failed} didn't sign in")),
    }
}

/// Signs in to `host` as a pane would: trusting each new host key, then
/// answering what the server asks. Once in, the shell says who and where
/// it is, and a host's forwarding rules fetch the web server inside.
async fn sign_in(vault: &Vault, host: &Host) -> Result<String, String> {
    let connection = vault.connection(host)?;
    let password = (connection.auth == Auth::Password).then_some(PASSWORD);
    let mut trust = None;
    // Each hop may show a new key.
    for _ in 0..connection.jumps.len() + 2 {
        let request = connection.request(SIZE, password, trust.take(), |_| None);
        let mut session = Session::open(request);
        loop {
            match next(&mut session).await? {
                Event::Connected => return shell(&mut session, host).await,
                Event::Prompts(prompts) => {
                    let answers = prompts
                        .questions
                        .iter()
                        .map(|(question, _)| answer(host, question))
                        .collect::<Result<_, _>>()?;
                    session.answer(answers);
                }
                Event::Failed(ConnectError::UnknownHostKey {
                    host, port, key, ..
                }) => {
                    trust = Some(Trusted {
                        host,
                        port,
                        key: *key,
                    });
                    break;
                }
                Event::Failed(error) => return Err(error.to_string()),
                Event::Closed { .. } | Event::Lost => return Err("the connection closed".into()),
                _ => {}
            }
        }
    }
    Err("it kept showing new host keys".into())
}

async fn next(session: &mut Session) -> Result<Event, String> {
    tokio::time::timeout(Duration::from_secs(30), session.next_event())
        .await
        .map_err(|_| "nothing for 30 seconds".to_string())?
        .ok_or_else(|| "the session ended".to_string())
}

/// The lab's password for a password or the encrypted key; a one-time code
/// worked out in the server's container.
fn answer(host: &Host, question: &str) -> Result<String, String> {
    let question = question.to_lowercase();
    if question.contains("code") {
        // lab-otp is the container tern-lab-otp-1, and so on.
        let role = host.name.strip_prefix("lab-").unwrap_or(&host.name);
        let out = std::process::Command::new("docker")
            .args(["exec", &format!("tern-lab-{role}-1"), "oathtool"])
            .args(["--totp", "-b", CODES])
            .output()
            .map_err(|error| format!("docker: {error}"))?;
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr);
            return Err(format!("docker: {}", why.trim()));
        }
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    if question.contains("password") || question.contains("passphrase") {
        return Ok(PASSWORD.into());
    }
    Err(format!("asked {question:?}"))
}

/// Asks the shell for its user, machine and `TERN_LAB`, then tries the
/// host's forwarding rules.
async fn shell(session: &mut Session, host: &Host) -> Result<String, String> {
    session.write("echo \"@$(whoami)@$(hostname)@${TERN_LAB-}@\"\r");
    let mut seen = String::new();
    let said = loop {
        if let Event::Output(bytes) = next(session).await? {
            seen.push_str(&String::from_utf8_lossy(&bytes));
            // The typed line shows too, but with its `$(...)`.
            let said = seen.split(['\r', '\n']).find(|line| {
                line.starts_with('@')
                    && line.ends_with('@')
                    && line.matches('@').count() == 4
                    && !line.contains('$')
            });
            if let Some(said) = said {
                break said.to_string();
            }
        }
    };
    let said: Vec<&str> = said.split('@').collect();
    let mut what = format!("{}@{}", said[1], said[2]);
    if !said[3].is_empty() {
        what += &format!(", TERN_LAB={}", said[3]);
    }
    for forward in host.forwards.iter().filter(|forward| forward.on) {
        let to = format!("{}:{}", forward.listen_host, forward.listen_port);
        through(forward)
            .await
            .map_err(|why| format!("{what}; {to}: {why}"))?;
        what += &format!(", web inside through {to}");
    }
    session.close();
    Ok(what)
}

/// Fetches the web server's page through a forwarding rule, once it
/// listens.
async fn through(forward: &Forward) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..50 {
        let stream = match forward.kind {
            ForwardKind::Dynamic => {
                let socks = Proxy::parse(&format!("socks5://127.0.0.1:{}", forward.listen_port))?;
                proxy::connect(&socks, "web", 8000).await
            }
            _ => TcpStream::connect(("127.0.0.1", forward.listen_port))
                .await
                .map_err(|error| error.to_string()),
        };
        match stream {
            Ok(stream) => return hello(stream).await,
            Err(error) => last = error,
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(last)
}

async fn hello(mut stream: TcpStream) -> Result<(), String> {
    stream
        .write_all(b"GET /hello.txt HTTP/1.0\r\n\r\n")
        .await
        .map_err(|error| error.to_string())?;
    let mut reply = String::new();
    stream
        .read_to_string(&mut reply)
        .await
        .map_err(|error| error.to_string())?;
    if reply.contains(HELLO) {
        Ok(())
    } else {
        Err(format!("got {:?}", reply.lines().next().unwrap_or("")))
    }
}
