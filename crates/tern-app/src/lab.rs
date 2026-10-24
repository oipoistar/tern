//! The app against real OpenSSH servers: the lab in `dev/lab`, run in
//! Docker. Panes sign in through their own cards, keyboard only, as a user
//! would. It needs the lab up, so it only runs when asked for:
//!
//! ```text
//! dev/lab/setup.sh
//! cargo test -p tern-app lab -- --ignored --nocapture
//! ```

// Explicit imports: `gpui_kit::*` would shadow Rust's `#[test]`.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{TestAppContext, VisualTestContext};
use gpui_kit::{AppContext as _, Entity};
use tern_core::containers::Container;
use tern_core::forward::ForwardState;
use tern_core::ssh::{Connection, Target};
use tern_core::{Auth, Host, Vault};

use crate::files::FilesPanel;
use crate::terminal::{Marked, Phase, TerminalView};

fn keys() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dev/lab/keys")
}

fn key() -> Auth {
    Auth::Key(keys().join("id_ed25519"))
}

fn docker(args: &[&str]) -> String {
    let out = std::process::Command::new("docker")
        .args(args)
        .output()
        .expect("docker runs");
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A lab server published on this computer, signed in to as `lab`.
fn here(port: u16, auth: Option<Auth>) -> Connection {
    let target = Target {
        host: "127.0.0.1".into(),
        port,
        user: "lab".into(),
    };
    match auth {
        Some(auth) => Connection {
            auth,
            ..Connection::typed(target, None)
        },
        None => Connection::typed(target, None),
    }
}

/// A pane signing in to `connection` in a window of its own, with the
/// keyboard.
fn pane(
    cx: &mut TestAppContext,
    connection: Connection,
) -> (Entity<TerminalView>, &mut VisualTestContext) {
    let (view, cx) = cx.add_window_view(|_, cx| TerminalView::new(connection, cx));
    cx.update(|window, cx| view.update(cx, |view, cx| view.focus(window, cx)));
    (view, cx)
}

fn phase(view: &TerminalView) -> String {
    match &view.phase {
        Phase::Connecting => "connecting".into(),
        Phase::Trust { shown, .. } => format!("asked to trust {shown}"),
        Phase::Password { rejected, .. } => format!("asked for a password (rejected: {rejected})"),
        Phase::Questions { prompts, .. } => format!("asked {:?}", prompts.questions),
        Phase::Live => "live".into(),
        Phase::Ended(why) => format!("ended: {why}"),
    }
}

/// Waits, up to half a minute, until `done` holds; panics with the pane's
/// phase and screen if it never does.
fn wait(
    view: &Entity<TerminalView>,
    cx: &mut VisualTestContext,
    what: &str,
    done: impl Fn(&TerminalView) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        cx.run_until_parked();
        if view.read_with(cx, |view, _| done(view)) {
            return;
        }
        if Instant::now() > deadline {
            let (now, screen) = view.read_with(cx, |view, _| (phase(view), view.output_text()));
            panic!("never {what}: {now}\n{screen}");
        }
        std::thread::sleep(Duration::from_millis(50));
        // GPUI's test clock only moves when told: in step with the real one,
        // so the pane's own timers (reconnecting) run.
        cx.executor().advance_clock(Duration::from_millis(50));
    }
}

/// Waits, up to half a minute, until `done` holds, the test clock keeping
/// step with the real one.
fn until(cx: &mut VisualTestContext, what: &str, done: impl Fn(&mut VisualTestContext) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(Instant::now() < deadline, "never {what}");
        std::thread::sleep(Duration::from_millis(50));
        cx.executor().advance_clock(Duration::from_millis(50));
    }
}

/// Waits for the pane to stop connecting; where it got to.
fn settled(view: &Entity<TerminalView>, cx: &mut VisualTestContext) -> String {
    wait(view, cx, "settled", |view| {
        !matches!(view.phase, Phase::Connecting)
    });
    view.read_with(cx, |view, _| phase(view))
}

/// Trusts each new host key the pane asks about, until it's signed in.
fn trust_until_live(view: &Entity<TerminalView>, cx: &mut VisualTestContext) -> usize {
    let mut trusted = 0;
    loop {
        let now = settled(view, cx);
        println!("      pane: {now}");
        if now == "live" {
            return trusted;
        }
        assert!(now.starts_with("asked to trust"), "{now}");
        cx.simulate_keystrokes("enter");
        trusted += 1;
    }
}

/// Types `line` into the live shell and waits for `wanted` on the screen.
fn shell(view: &Entity<TerminalView>, cx: &mut VisualTestContext, line: &str, wanted: &str) {
    cx.simulate_input(line);
    cx.simulate_keystrokes("enter");
    wait(view, cx, &format!("{wanted} after `{line}`"), |view| {
        view.output_text().contains(wanted)
    });
}

#[gpui::test]
#[ignore = "needs the SSH lab: dev/lab/setup.sh"]
fn the_app_against_the_lab(cx: &mut TestAppContext) {
    let scratch = std::env::temp_dir().join(format!("tern-app-lab-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    // A copy of the lab's: Linux and macOS write keys they trust there.
    let known_hosts = scratch.join("known_hosts");
    std::fs::copy(keys().join("known_hosts"), &known_hosts).unwrap();
    // SAFETY: set before any session, and so any other thread, starts.
    unsafe {
        std::env::set_var("TERN_DATA_DIR", scratch.join("data"));
        std::env::set_var("TERN_SYSTEM_KNOWN_HOSTS", &known_hosts);
        std::env::set_var("TERN_SSH_DIR", scratch.join("ssh"));
    }
    // Sessions answer on their own threads.
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);

    // A new host: the trust card, Enter, a shell; then Files, then a
    // restart of the server, which the pane rides out by itself.
    {
        let (view, cx) = pane(cx, here(2202, Some(key())));
        assert_eq!(trust_until_live(&view, cx), 1, "one new key");
        shell(&view, cx, "echo tern-$((40+2))", "tern-42");
        println!("ok    the trust card, then a shell");

        let panel = cx.new(|cx| FilesPanel::new(&view, scratch.clone(), cx));
        let lists = |name: &str, cx: &mut VisualTestContext| {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !panel.read_with(cx, |panel, _| {
                panel.remote_names().iter().any(|n| n == name)
            }) {
                assert!(Instant::now() < deadline, "Files never listed {name}");
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        lists(".bashrc", cx);
        println!("ok    Files lists the host's home folder over SFTP");

        docker(&["restart", "-t", "1", "tern-lab-key-1"]);
        wait(&view, cx, "lost", |view| !matches!(view.phase, Phase::Live));
        wait(&view, cx, "back", |view| matches!(view.phase, Phase::Live));
        shell(&view, cx, "echo back-$((1+1))", "back-2");
        println!("ok    reconnects by itself after the server restarts");

        // Files' SFTP went with the old connection; Refresh opens it on
        // the new one, then lists what's there now.
        let fresh = format!("made-after-restart-{}", std::process::id());
        shell(
            &view,
            cx,
            &format!("touch {fresh} && echo made-$((2+3))"),
            "made-5",
        );
        panel.update(cx, |panel, cx| panel.refresh(cx));
        lists(&fresh, cx);
        println!("ok    Refresh opens Files again on the new connection");

        // A host's file edited here: each save goes back, and after a
        // change on the host it asks before writing over it.
        let edited = format!("edited-{}.txt", std::process::id());
        let on_host = || {
            docker(&[
                "exec",
                "tern-lab-key-1",
                "cat",
                &format!("/home/lab/{edited}"),
            ])
        };
        shell(
            &view,
            cx,
            &format!("echo one > {edited} && echo made-$((3+4))"),
            "made-7",
        );
        panel.update(cx, |panel, cx| panel.refresh(cx));
        lists(&edited, cx);
        panel.update(cx, |panel, cx| panel.edit_named(&edited, cx));
        until(cx, "the copy opened", |cx| {
            panel.read_with(cx, |p, _| !p.opened().is_empty())
        });
        let copy = panel.read_with(cx, |panel, _| panel.opened()[0].clone());
        assert_eq!(std::fs::read_to_string(&copy).unwrap(), "one\n");
        std::fs::write(&copy, "two\n").unwrap();
        until(cx, "the save went back", |_| on_host() == "two");
        docker(&[
            "exec",
            "tern-lab-key-1",
            "sh",
            "-c",
            &format!("echo three > /home/lab/{edited}"),
        ]);
        std::fs::write(&copy, "four\n").unwrap();
        until(cx, "asked to replace", |cx| {
            panel.read_with(cx, |p, _| p.asks_to_replace())
        });
        assert_eq!(on_host(), "three");
        panel.update(cx, |panel, cx| panel.confirm(cx));
        until(cx, "the copy replaced it", |_| on_host() == "four");
        println!("ok    a host's file edited here goes back, asking after a change there");

        // A port the session opens is offered, forwarded here in a click,
        // and goes when it closes.
        shell(
            &view,
            cx,
            "python3 -m http.server 18765 --bind 127.0.0.1 >/dev/null 2>&1 & echo served-$((5+6))",
            "served-11",
        );
        let offered = |view: &TerminalView| {
            view.opened_ports()
                .into_iter()
                .find(|(port, _)| *port == 18765)
        };
        wait(&view, cx, "the port offered", |view| {
            offered(view).is_some()
        });
        view.update(cx, |view, cx| view.toggle_port(18765, cx));
        wait(&view, cx, "the port forwarded", |view| {
            offered(view) == Some((18765, Some(Some(ForwardState::Running))))
        });
        let page = {
            use std::io::{Read as _, Write as _};
            let mut stream = std::net::TcpStream::connect("127.0.0.1:18765").expect("forwarded");
            stream.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
            let mut page = String::new();
            stream.read_to_string(&mut page).unwrap();
            page
        };
        assert!(page.contains("Directory listing"), "{page}");
        shell(&view, cx, "kill %1; echo stopped-$((6+6))", "stopped-12");
        wait(&view, cx, "the port gone", |view| offered(view).is_none());
        println!("ok    a port the session opens is forwarded here in a click");

        // The host's containers, listed beside the terminal (the lab's
        // stand-in docker has one).
        wait(&view, cx, "the containers listed", |view| {
            view.containers()
                .is_some_and(|listed| listed.iter().any(|c| c.name == "lab-app"))
        });
        println!("ok    the host's containers are listed");

        // Command marks, from bash told to report them as shell
        // integration does.
        shell(
            &view,
            cx,
            "PS0='\\e]133;C\\a'; PS1='\\[\\e]133;D;$?\\a\\e]133;A\\a\\]\\$ \\[\\e]133;B\\a\\]'; echo marked-$((7+7))",
            "marked-14",
        );
        shell(
            &view,
            cx,
            "ls / | head -3; echo listed-$((8+8))",
            "listed-16",
        );
        shell(&view, cx, "ls /nonexistent-$((9+9))", "nonexistent-18");
        wait(&view, cx, "the commands marked", |view| {
            let marks: Vec<Marked> = view
                .command_marks()
                .into_iter()
                .map(|(_, mark)| mark)
                .collect();
            marks.ends_with(&[Marked::Done, Marked::Failed])
        });
        println!("ok    commands are marked, done or failed, as bash reports them");

        // The shell's folder, as bash reports it: Files follows a cd, and
        // Files' button moves the shell.
        shell(
            &view,
            cx,
            "PS1='\\[\\e]7;file://$HOSTNAME$PWD\\a\\]'\"$PS1\"; cd /etc && echo moved-$((10+10))",
            "moved-20",
        );
        wait(&view, cx, "the folder told", |view| {
            view.folder() == Some("/etc")
        });
        let folder = view.read_with(cx, |view, _| view.folder().unwrap().to_string());
        panel.update(cx, |panel, cx| panel.follow(folder, cx));
        lists("passwd", cx);
        panel.update(cx, |panel, cx| panel.follow("/usr".into(), cx));
        lists("bin", cx);
        panel.update(cx, |panel, cx| panel.cd_shell(cx));
        wait(&view, cx, "the shell moved", |view| {
            view.folder() == Some("/usr")
        });
        println!("ok    Files follows the shell's folder, and moves the shell");
    }

    // A shell inside a container on the host: the stand-in runs it on the
    // server itself, marked.
    {
        let lab_app = Container {
            tool: "docker".into(),
            id: "c0ffee00".into(),
            name: "lab-app".into(),
            image: "debian:bookworm-slim".into(),
        };
        let connection = Connection {
            container: Some(lab_app),
            ..here(2202, Some(key()))
        };
        let (view, cx) = pane(cx, connection);
        trust_until_live(&view, cx);
        shell(&view, cx, "echo in-$TERN_CONTAINER", "in-lab-app");
        println!("ok    a shell inside one of the host's containers");
    }

    // A kept shell: the server ends the connection, the pane reconnects by
    // itself and finds the same shell, with what was set in it; ending it
    // by hand ends it on the server too.
    {
        let connection = Connection {
            keep_shell: true,
            ..here(2202, Some(key()))
        };
        let (view, cx) = pane(cx, connection);
        trust_until_live(&view, cx);
        shell(&view, cx, "KEPT=$((40+2)); echo in-$TMUX_PANE", "in-%");
        docker(&["exec", "tern-lab-key-1", "pkill", "-f", "sshd: lab"]);
        wait(&view, cx, "lost", |view| !matches!(view.phase, Phase::Live));
        wait(&view, cx, "back", |view| matches!(view.phase, Phase::Live));
        shell(&view, cx, "echo kept-$KEPT", "kept-42");
        println!("ok    a kept shell comes back with the connection");

        view.update(cx, |view, _| view.end_kept_shell());
        wait(&view, cx, "ended", |view| {
            matches!(view.phase, Phase::Ended(_))
        });
        let left = docker(&[
            "exec",
            "-u",
            "lab",
            "tern-lab-key-1",
            "sh",
            "-c",
            "tmux ls 2>&1 || true",
        ]);
        assert!(!left.contains("tern-"), "still kept: {left}");
        println!("ok    closing it by hand ends the kept shell");
    }

    // A password-only server: the password card.
    {
        let (view, cx) = pane(cx, here(2201, None));
        assert_eq!(
            trust_until_live_or_password(&view, cx),
            "asked for a password (rejected: false)"
        );
        cx.simulate_input("lab-pass");
        cx.simulate_keystrokes("enter");
        wait(&view, cx, "live", |view| matches!(view.phase, Phase::Live));
        shell(&view, cx, "echo user-$(whoami)", "user-lab");
        println!("ok    the password card");
    }

    // A password, then a one-time code: the password card, then the
    // question.
    {
        let (view, cx) = pane(cx, here(2203, None));
        assert_eq!(
            trust_until_live_or_password(&view, cx),
            "asked for a password (rejected: false)"
        );
        cx.simulate_input("lab-pass");
        cx.simulate_keystrokes("enter");
        let asked = settled(&view, cx);
        assert!(asked.contains("Verification code"), "{asked}");
        let code = docker(&[
            "exec",
            "tern-lab-otp-1",
            "oathtool",
            "--totp",
            "-b",
            "JBSWY3DPEHPK3PXP",
        ]);
        cx.simulate_input(&code);
        cx.simulate_keystrokes("enter");
        wait(&view, cx, "live", |view| matches!(view.phase, Phase::Live));
        shell(&view, cx, "echo user-$(whoami)", "user-lab");
        println!("ok    the password card, then the one-time code's question");
    }

    // A saved host two jump hosts away: a trust card for each new key.
    {
        let mut vault = Vault::default();
        for host in [
            Host {
                auth: key(),
                ..Host::new("bastion", "lab", "127.0.0.1", 2210)
            },
            Host {
                auth: key(),
                jump: Some("bastion".into()),
                ..Host::new("hop2", "lab", "hop2", 22)
            },
            Host {
                auth: key(),
                jump: Some("hop2".into()),
                ..Host::new("deep", "lab", "deep", 22)
            },
        ] {
            vault.save(None, host, "Lab");
        }
        let connection = vault.connection(vault.find("deep").unwrap()).unwrap();
        let (view, cx) = pane(cx, connection);
        assert_eq!(trust_until_live(&view, cx), 3, "three new keys");
        shell(&view, cx, "echo on-$(hostname)", "on-deep");
        println!("ok    a saved host through two jump hosts");
    }

    // An encrypted key: its passphrase asked as a question, once the server
    // says it would take the key.
    {
        let locked = Auth::Key(keys().join("id_locked"));
        let (view, cx) = pane(cx, here(2202, Some(locked)));
        let asked = settled(&view, cx);
        assert!(asked.contains("Passphrase for id_locked"), "{asked}");
        cx.simulate_input("lab-pass");
        cx.simulate_keystrokes("enter");
        wait(&view, cx, "live", |view| matches!(view.phase, Phase::Live));
        shell(&view, cx, "echo user-$(whoami)", "user-lab");
        println!("ok    an encrypted key's passphrase, asked in the pane");
    }

    // A command waiting for a host that's down runs, in a session of its
    // own, once a check finds the host back; the vault needn't be showing.
    {
        use std::cell::RefCell;
        use std::rc::Rc;

        use crate::live::LiveSlots;
        use crate::vault::{VaultContent, VaultEvent};

        let mut hosts = Vault::default();
        let host = Host {
            auth: key(),
            ..Host::new("lab-key", "lab", "127.0.0.1", 2202)
        };
        hosts.save(None, host, "Lab");
        docker(&["stop", "-t", "1", "tern-lab-key-1"]);
        let (vault, cx) = cx.add_window_view(|window, cx| {
            VaultContent::new(hosts, None, LiveSlots::default(), window, cx)
        });
        // Checks run while the vault shows in the active window.
        cx.update(|window, cx| {
            window.activate_window();
            vault.update(cx, |vault, cx| vault.start_checks(window, cx));
        });
        let ran: Rc<RefCell<Vec<Connection>>> = Rc::default();
        let seen = ran.clone();
        cx.update(|_, cx| {
            cx.subscribe(&vault, move |_, event: &VaultEvent, _| {
                if let VaultEvent::Run(connection) = event {
                    seen.borrow_mut().push(connection.clone());
                }
            })
            .detach()
        });
        let click = |cx: &mut VisualTestContext, selector: &'static str| {
            cx.run_until_parked();
            let bounds = cx.debug_bounds(selector).expect(selector);
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        };
        click(cx, "host-lab-key");
        until(cx, "a check finds lab-key down", |cx| {
            cx.debug_bounds("wait").is_some()
        });
        click(cx, "wait");
        cx.simulate_input("echo waited-$((6*7))");
        click(cx, "wait-queue");
        vault.update(cx, |vault, _| vault.set_showing(false));
        docker(&["start", "tern-lab-key-1"]);

        // Checks come every half minute.
        let deadline = Instant::now() + Duration::from_secs(90);
        while ran.borrow().is_empty() {
            assert!(Instant::now() < deadline, "the command never ran");
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(100));
            cx.executor().advance_clock(Duration::from_millis(100));
        }
        let connection = ran.borrow()[0].clone();
        let (view, cx) = pane(cx, connection);
        trust_until_live(&view, cx);
        wait(&view, cx, "the waiting command's output", |view| {
            view.output_text().contains("waited-42")
        });
        println!("ok    a command waiting for a host runs once it's back");
    }

    // A slow connection: what's typed shows before its echo, and what a
    // program reads without echo never shows.
    {
        let relay = slow_relay(2202, Duration::from_millis(150));
        let (view, cx) = pane(cx, here(relay, Some(key())));
        trust_until_live(&view, cx);
        shell(&view, cx, "echo ready-$((1+1))", "ready-2");
        let last_line = |view: &TerminalView| {
            let text = view.output_text();
            text.trim_end()
                .lines()
                .last()
                .unwrap_or_default()
                .to_string()
        };
        cx.simulate_input("e");
        wait(&view, cx, "the first key's echo", |view| {
            last_line(view).ends_with("$ e")
        });
        cx.simulate_input("cho fast");
        assert_eq!(view.read_with(cx, |view, _| view.guessed()), "cho fast");
        wait(&view, cx, "the echoes", |view| {
            last_line(view).ends_with("$ echo fast") && view.guessed().is_empty()
        });
        cx.simulate_keystrokes("enter");

        cx.simulate_input("read -s -p \"$(printf 'Co%s' 'de: ')\" code; echo got-${#code}");
        cx.simulate_keystrokes("enter");
        wait(&view, cx, "the question", |view| {
            last_line(view).ends_with("Code:")
        });
        for c in ["h", "u", "n", "t", "e", "r", "2"] {
            cx.simulate_input(c);
            std::thread::sleep(Duration::from_millis(60));
            cx.run_until_parked();
            view.read_with(cx, |view, _| assert!(view.guessed().is_empty()));
        }
        cx.simulate_keystrokes("enter");
        wait(&view, cx, "the answer", |view| {
            view.output_text().contains("got-7")
        });
        println!("ok    on a slow connection typing shows at once, a hidden answer never");
    }

    // The server's key changed: refused, with the line to look at. Its own
    // keys go back after, pass or fail, so a key trusted elsewhere (in your
    // own Tern) still holds.
    {
        docker(&[
            "exec",
            "tern-lab-key-1",
            "sh",
            "-c",
            "mkdir -p /root/tern-host-keys && cp -p /etc/ssh/ssh_host_* /root/tern-host-keys/ \
             && rm -f /etc/ssh/ssh_host_* && ssh-keygen -A >/dev/null && kill -HUP 1",
        ]);
        std::thread::sleep(Duration::from_secs(1));
        let (view, cx) = pane(cx, here(2202, Some(key())));
        let ended = settled(&view, cx);
        docker(&[
            "exec",
            "tern-lab-key-1",
            "sh",
            "-c",
            "cp -p /root/tern-host-keys/ssh_host_* /etc/ssh/ && kill -HUP 1",
        ]);
        std::thread::sleep(Duration::from_secs(1));
        assert!(
            ended.starts_with("ended") && ended.contains("known_hosts"),
            "{ended}"
        );
        println!("ok    a changed host key: {ended}");
    }
}

/// A relay on this computer to `port` that holds what it passes for
/// `delay` each way, like a slow connection; its port.
fn slow_relay(port: u16, delay: Duration) -> u16 {
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener, TcpStream};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let Ok(server) = TcpStream::connect(("127.0.0.1", port)) else {
                continue;
            };
            let ways = [
                (client.try_clone().unwrap(), server.try_clone().unwrap()),
                (server, client),
            ];
            for (mut from, mut to) in ways {
                let (sent, held) = std::sync::mpsc::channel::<(Instant, Vec<u8>)>();
                std::thread::spawn(move || {
                    let mut buf = [0; 16384];
                    while let Ok(n @ 1..) = from.read(&mut buf) {
                        if sent
                            .send((Instant::now() + delay, buf[..n].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                std::thread::spawn(move || {
                    for (due, bytes) in held {
                        std::thread::sleep(due.saturating_duration_since(Instant::now()));
                        if to.write_all(&bytes).is_err() {
                            break;
                        }
                    }
                    let _ = to.shutdown(Shutdown::Write);
                });
            }
        }
    });
    relay
}

/// Trusts new keys until the pane signs in or asks for a password; which.
fn trust_until_live_or_password(view: &Entity<TerminalView>, cx: &mut VisualTestContext) -> String {
    loop {
        let now = settled(view, cx);
        if !now.starts_with("asked to trust") {
            return now;
        }
        cx.simulate_keystrokes("enter");
    }
}
