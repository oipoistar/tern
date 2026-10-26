//! Groups that fill themselves: a command run on this computer prints
//! hosts in OpenSSH's config form (from Tailscale, a cloud's CLI, an
//! Ansible inventory, a script), and its group is kept to what it prints.
//! The commands stay on this computer, in `sources.json`, and never sync:
//! a synced vault mustn't be able to run anything here.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::host::Host;
use crate::vault::Vault;

/// How long a source's command may take.
pub const WAIT: Duration = Duration::from_secs(30);
/// What a source's command may print, at most.
const PRINTED_AT_MOST: u64 = 16 << 20;

/// A group and the command that fills it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub group: String,
    pub command: String,
}

/// This computer's sources, as kept in `sources.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sources {
    pub sources: Vec<Source>,
}

impl Sources {
    /// The sources in `path`; none if it's missing or unreadable.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        crate::paths::write_atomically(path, format!("{json}\n").as_bytes())
    }

    /// The command filling `group`, if one does.
    pub fn command(&self, group: &str) -> Option<&str> {
        self.sources
            .iter()
            .find(|source| source.group == group)
            .map(|source| source.command.as_str())
    }

    /// Fills `group` with `command` from now on; an empty command stops.
    pub fn set(&mut self, group: &str, command: &str) {
        self.sources.retain(|source| source.group != group);
        let command = command.trim();
        if !command.is_empty() {
            self.sources.push(Source {
                group: group.into(),
                command: command.into(),
            });
        }
    }
}

/// Runs `command` in this computer's shell (cmd on Windows, sh elsewhere)
/// for at most `wait`: what it printed, or why it failed in one line.
pub fn run(command: &str, wait: Duration) -> Result<String, String> {
    run_printing(command, wait, PRINTED_AT_MOST)
}

/// [`run`], keeping at most `at_most` bytes of each of what the command
/// prints and complains. Its deadline covers what it starts too: a program
/// it leaves running that still holds its output doesn't hold this up.
fn run_printing(command: &str, wait: Duration, at_most: u64) -> Result<String, String> {
    let mut child = shell(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let printed = read_all(child.stdout.take().expect("piped"), at_most);
    let complained = read_all(child.stderr.take().expect("piped"), at_most);
    let deadline = Instant::now() + wait;
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break status;
        }
        if Instant::now() > deadline {
            stop(&mut child);
            return Err("It took too long".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let left = || deadline.saturating_duration_since(Instant::now());
    let (Ok(printed), Ok(complained)) = (
        printed.recv_timeout(left()),
        complained.recv_timeout(left()),
    ) else {
        stop(&mut child);
        return Err("It took too long".into());
    };
    let (Some(printed), Some(complained)) = (printed, complained) else {
        return Err("It printed too much".into());
    };
    let (printed, complained) = (
        String::from_utf8_lossy(&printed).into_owned(),
        String::from_utf8_lossy(&complained).into_owned(),
    );
    if status.success() {
        return Ok(printed);
    }
    let why = complained
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty());
    Err(match (why, status.code()) {
        (Some(why), _) => why.to_string(),
        (None, Some(code)) => format!("It ended with status {code}"),
        (None, None) => "It was stopped".into(),
    })
}

/// Reads all of `pipe` as it comes, on a thread of its own, so a full pipe
/// never stops the command: up to `at_most` bytes of it, `None` if there's
/// more (that's read too, and let go).
fn read_all(mut pipe: impl Read + Send + 'static, at_most: u64) -> mpsc::Receiver<Option<Vec<u8>>> {
    let (done, read) = mpsc::channel();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let _ = (&mut pipe).take(at_most).read_to_end(&mut kept);
        let more = std::io::copy(&mut pipe, &mut std::io::sink()).unwrap_or(0) > 0;
        let _ = done.send((!more).then_some(kept));
    });
    read
}

/// Ends the command, and what it started that's still running: its
/// process group on Unix, its process tree on Windows.
fn stop(child: &mut Child) {
    #[cfg(unix)]
    {
        if let Ok(group) = i32::try_from(child.id()) {
            // SAFETY: signals the process group the shell leads (made at
            // its start, see `shell`), which nothing else is in.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let _ = Command::new(crate::paths::system_program("taskkill.exe"))
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(windows)]
fn shell(command: &str) -> Command {
    use std::os::windows::process::CommandExt as _;
    // No console window flashing up behind the app.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut shell = Command::new(crate::paths::system_program("cmd.exe"));
    shell
        .arg("/C")
        .raw_arg(command)
        .creation_flags(CREATE_NO_WINDOW);
    shell
}

#[cfg(not(windows))]
fn shell(command: &str) -> Command {
    use std::os::unix::process::CommandExt as _;
    let mut shell = Command::new("sh");
    // A process group of its own, so what it starts stops with it.
    shell.arg("-c").arg(command).process_group(0);
    shell
}

/// Makes `group` hold the hosts `printed` describes, matched by name. One
/// already there keeps its id and what was added to it here (notes, tags,
/// guard, kept shells, OS, MAC address, identity, login command); a new
/// one is added, named "name (group)" when another group has its name;
/// one no longer printed goes, unless another host goes through it.
/// Nothing printed changes nothing: a source that comes back empty is more
/// likely broken than emptied.
pub fn refill(vault: &mut Vault, group: &str, printed: Vec<Host>) -> Result<(), String> {
    if printed.is_empty() {
        return Err("It printed no hosts".into());
    }
    let elsewhere = |vault: &Vault, name: &str| {
        vault
            .groups
            .iter()
            .filter(|other| other.name != group)
            .flat_map(|other| &other.hosts)
            .any(|host| host.name.eq_ignore_ascii_case(name))
    };
    let names: HashMap<String, String> = printed
        .iter()
        .map(|host| {
            let name = if elsewhere(vault, &host.name) {
                format!("{} ({group})", host.name)
            } else {
                host.name.clone()
            };
            (host.name.clone(), name)
        })
        .collect();
    let mut kept = Vec::new();
    for mut host in printed {
        host.name = names[&host.name].clone();
        if let Some(jump) = host.jump.as_ref().and_then(|jump| names.get(jump)) {
            host.jump = Some(jump.clone());
        }
        let old = vault
            .groups
            .iter()
            .find(|have| have.name == group)
            .and_then(|have| have.hosts.iter().find(|old| old.name == host.name))
            .cloned();
        kept.push(host.name.clone());
        match old {
            Some(old) => {
                let host = Host {
                    id: old.id.clone(),
                    notes: old.notes.clone(),
                    tags: old.tags.clone(),
                    guarded: old.guarded,
                    keep_shell: old.keep_shell,
                    os: old.os.clone(),
                    mac: old.mac.clone(),
                    identity: old.identity.clone(),
                    on_login: old.on_login.clone(),
                    updated: old.updated,
                    unknown: old.unknown.clone(),
                    ..host
                };
                // Unchanged, it isn't saved, so sync has nothing to send.
                if host != old {
                    let name = host.name.clone();
                    vault.save(Some(&name), host, group);
                }
            }
            None => vault.save(None, host, group),
        }
    }
    let gone: Vec<String> = vault
        .groups
        .iter()
        .filter(|have| have.name == group)
        .flat_map(|have| &have.hosts)
        .filter(|host| !kept.contains(&host.name))
        .map(|host| host.name.clone())
        .collect();
    for name in gone {
        // Refused while another host goes through it: it stays.
        let _ = vault.delete(&name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(name: &str, address: &str) -> Host {
        Host::new(name, "deploy", address, 22)
    }

    #[test]
    fn keeps_a_group_to_what_its_command_prints() {
        let mut vault = Vault::default();
        vault.save(None, host("db", "10.0.0.9"), "Office");
        refill(
            &mut vault,
            "Fleet",
            vec![host("web", "10.0.1.1"), host("cache", "10.0.1.2")],
        )
        .unwrap();
        let web = vault.find("web").unwrap().clone();
        assert_eq!(vault.group_of("web"), Some("Fleet"));

        // What the user added stays; what the source says changes.
        let mut noted = web.clone();
        noted.notes = "the shop".into();
        noted.guarded = true;
        vault.save(Some("web"), noted, "Fleet");
        let mut moved = host("web", "10.0.1.9");
        moved.port = 2222;
        refill(&mut vault, "Fleet", vec![moved, host("db", "10.0.1.3")]).unwrap();
        let web_now = vault.find("web").unwrap();
        assert_eq!((web_now.id.as_str(), web_now.port), (web.id.as_str(), 2222));
        assert_eq!(web_now.address, "10.0.1.9");
        assert!(web_now.guarded && web_now.notes == "the shop");
        // Gone from the output, gone from the group.
        assert!(vault.find("cache").is_none());
        // Another group's name gets the group's name added.
        assert_eq!(vault.group_of("db (Fleet)"), Some("Fleet"));
        assert_eq!(vault.find("db").unwrap().address, "10.0.0.9");
    }

    #[test]
    fn jumps_follow_renames_and_nothing_printed_changes_nothing() {
        let mut vault = Vault::default();
        vault.save(None, host("bastion", "10.0.0.1"), "Office");
        let mut inside = host("inside", "10.9.0.2");
        inside.jump = Some("bastion".into());
        refill(
            &mut vault,
            "Fleet",
            vec![host("bastion", "10.9.0.1"), inside],
        )
        .unwrap();
        assert_eq!(
            vault.find("inside").unwrap().jump.as_deref(),
            Some("bastion (Fleet)")
        );
        assert!(refill(&mut vault, "Fleet", Vec::new()).is_err());
        assert_eq!(vault.group_of("inside"), Some("Fleet"));
        // A host others go through stays while they do.
        let mut outside = host("outside", "10.0.0.5");
        outside.jump = Some("bastion (Fleet)".into());
        vault.save(None, outside, "Office");
        refill(&mut vault, "Fleet", vec![host("inside", "10.9.0.2")]).unwrap();
        assert!(vault.find("bastion (Fleet)").is_some());
    }

    #[test]
    fn keeps_commands_by_group() {
        let mut sources = Sources::default();
        sources.set("Fleet", " tailscale status ");
        sources.set("Lab", "cat lab.conf");
        sources.set("Fleet", "tailscale status --json");
        assert_eq!(sources.command("Fleet"), Some("tailscale status --json"));
        sources.set("Lab", "  ");
        assert_eq!(sources.command("Lab"), None);
        let path = std::env::temp_dir().join(format!("tern-sources-{}.json", std::process::id()));
        sources.save(&path).unwrap();
        assert_eq!(Sources::load(&path), sources);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn runs_commands_in_this_computers_shell() {
        assert_eq!(run("echo hello", WAIT).unwrap().trim(), "hello");
        let failed = run("exit 3", WAIT).unwrap_err();
        assert_eq!(failed, "It ended with status 3");
        let slow = if cfg!(windows) {
            "ping -n 5 127.0.0.1"
        } else {
            "sleep 5"
        };
        assert_eq!(
            run(slow, Duration::from_millis(300)).unwrap_err(),
            "It took too long"
        );
    }

    #[test]
    fn a_command_that_leaves_a_program_running_still_ends_in_time() {
        // It ends at once, but leaves something holding its output open.
        let leaves = if cfg!(windows) {
            "start /b ping -n 6 127.0.0.1"
        } else {
            "sleep 5 &"
        };
        let started = Instant::now();
        assert_eq!(
            run(leaves, Duration::from_millis(800)).unwrap_err(),
            "It took too long"
        );
        assert!(started.elapsed() < Duration::from_secs(3));

        // And it keeps to so much of what it prints.
        let says = if cfg!(windows) {
            "for /l %i in (1,1,300) do @echo line %i"
        } else {
            "for i in $(seq 300); do echo line $i; done"
        };
        assert!(run_printing(says, WAIT, 64 << 10).is_ok());
        assert_eq!(
            run_printing(says, WAIT, 100).unwrap_err(),
            "It printed too much"
        );
    }
}
