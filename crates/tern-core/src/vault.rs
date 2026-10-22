//! The saved hosts, in groups, and snippets, and `vault.json`, the file
//! they're kept in.
//!
//! The file holds no secrets (names, addresses, users and key-file paths,
//! like `~/.ssh/config`); encrypting it belongs to sync. What checks observe
//! (see [`crate::reach`]) isn't saved.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::forward::{Forward, ForwardId};
use crate::host::{Host, HostGroup, now_ms};
use crate::paths;
use crate::reach::Place;
use crate::snippet::{self, Snippet};
use crate::ssh::{Connection, Jump};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Vault {
    pub groups: Vec<HostGroup>,
    pub snippets: Vec<Snippet>,
    /// Hosts and snippets deleted here, so sync deletes them elsewhere too.
    pub deleted: Vec<Tombstone>,
}

/// A deleted host's or snippet's id and when it went, in milliseconds
/// since the Unix epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub id: String,
    pub at: u64,
}

/// Tombstones kept, newest first; older deletions are long synced.
const KEEP_TOMBSTONES: usize = 1000;

impl Vault {
    /// Sample hosts and snippets, for tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn sample() -> Self {
        Self {
            groups: crate::host::sample_groups(),
            snippets: vec![
                Snippet::new("Disk space", "df -h"),
                Snippet::new("Follow syslog", "tail -f /var/log/syslog"),
            ],
            deleted: Vec::new(),
        }
    }

    /// Saves a snippet, replacing the one called `replacing` in place, after
    /// the checks in [`snippet::check`].
    pub fn save_snippet(&mut self, replacing: Option<&str>, typed: &Snippet) -> Result<(), String> {
        let mut snippet = snippet::check(&self.snippets, replacing, typed)?;
        snippet.updated = now_ms();
        match replacing.and_then(|old| self.snippets.iter().position(|s| s.name == old)) {
            Some(index) => self.snippets[index] = snippet,
            None => self.snippets.push(snippet),
        }
        Ok(())
    }

    pub fn delete_snippet(&mut self, name: &str) {
        let gone: Vec<String> = self
            .snippets
            .iter()
            .filter(|snippet| snippet.name == name)
            .map(|snippet| snippet.id.clone())
            .collect();
        self.snippets.retain(|snippet| snippet.name != name);
        for id in gone {
            self.bury(id);
        }
    }

    /// Records that the host or snippet `id` was deleted, for sync.
    pub fn bury(&mut self, id: String) {
        self.deleted.retain(|tombstone| tombstone.id != id);
        self.deleted.insert(0, Tombstone { id, at: now_ms() });
        self.deleted.truncate(KEEP_TOMBSTONES);
    }

    pub fn hosts(&self) -> impl Iterator<Item = &Host> {
        self.groups.iter().flat_map(|group| &group.hosts)
    }

    pub fn find(&self, name: &str) -> Option<&Host> {
        self.hosts().find(|host| host.name == name)
    }

    pub fn group_of(&self, name: &str) -> Option<&str> {
        self.groups
            .iter()
            .find(|group| group.hosts.iter().any(|host| host.name == name))
            .map(|group| group.name.as_str())
    }

    /// Hosts that connect through `name`.
    pub fn dependents(&self, name: &str) -> Vec<&str> {
        self.hosts()
            .filter(|host| host.jump.as_deref() == Some(name))
            .map(|host| host.name.as_str())
            .collect()
    }

    /// Saves `host` into `group`, creating the group if needed. With
    /// `replacing` (the host's name before editing) the old entry is updated
    /// in place, or moved when the group changed, and hosts that went through
    /// the old name follow a rename. A group left empty goes away.
    pub fn save(&mut self, replacing: Option<&str>, mut host: Host, group: &str) {
        host.updated = now_ms();
        let mut slot = None;
        if let Some(old) = replacing {
            for (g, existing) in self.groups.iter_mut().enumerate() {
                if let Some(h) = existing.hosts.iter().position(|host| host.name == old) {
                    existing.hosts.remove(h);
                    if existing.name == group {
                        slot = Some((g, h));
                    }
                    break;
                }
            }
            if old != host.name {
                for other in self.groups.iter_mut().flat_map(|group| &mut group.hosts) {
                    if other.jump.as_deref() == Some(old) {
                        other.jump = Some(host.name.clone());
                    }
                }
            }
        }
        match slot {
            Some((g, h)) => self.groups[g].hosts.insert(h, host),
            None => match self
                .groups
                .iter_mut()
                .find(|existing| existing.name == group)
            {
                Some(existing) => existing.hosts.push(host),
                None => self.groups.push(HostGroup {
                    name: group.into(),
                    hosts: vec![host],
                }),
            },
        }
        self.groups.retain(|group| !group.hosts.is_empty());
    }

    /// Removes a host. Refused while other hosts go through it; the error
    /// names them. A group left empty goes away.
    pub fn delete(&mut self, name: &str) -> Result<(), Vec<String>> {
        let dependents = self.dependents(name);
        if !dependents.is_empty() {
            return Err(dependents.into_iter().map(String::from).collect());
        }
        let gone: Vec<String> = self
            .hosts()
            .filter(|host| host.name == name)
            .map(|host| host.id.clone())
            .collect();
        for group in &mut self.groups {
            group.hosts.retain(|host| host.name != name);
        }
        self.groups.retain(|group| !group.hosts.is_empty());
        for id in gone {
            self.bury(id);
        }
        Ok(())
    }

    /// Where to check that `host` answers: its jump host, the first hop,
    /// when it goes through one, otherwise the host itself.
    pub fn first_hop(&self, host: &Host) -> Place {
        let hop = host
            .jump
            .as_deref()
            .and_then(|name| self.find(name))
            .unwrap_or(host);
        (hop.address.clone(), hop.port)
    }

    /// How to connect to `host`, with its jump host resolved.
    pub fn connection(&self, host: &Host) -> Result<Connection, String> {
        let jump = match &host.jump {
            None => None,
            Some(name) => {
                let via = self
                    .find(name)
                    .ok_or_else(|| format!("No saved host named {name}"))?;
                Some(Jump {
                    target: via.ssh_target(),
                    auth: via.auth.clone(),
                })
            }
        };
        Ok(Connection {
            host: Some(host.name.clone()),
            target: host.ssh_target(),
            auth: host.auth.clone(),
            jump,
            keep_alive: host.keep_alive,
            on_login: host.on_login.clone(),
            forwards: host
                .forwards
                .iter()
                .filter(|forward| forward.on)
                .cloned()
                .collect(),
        })
    }

    fn host_mut(&mut self, name: &str) -> Option<&mut Host> {
        self.groups
            .iter_mut()
            .flat_map(|group| &mut group.hosts)
            .find(|host| host.name == name)
    }

    /// Saves a port forwarding rule on `host`, replacing the rule
    /// `replacing` in place. Refused when another of its rules of the same
    /// kind already listens on that port.
    pub fn save_forward(
        &mut self,
        host: &str,
        replacing: Option<&ForwardId>,
        forward: Forward,
    ) -> Result<(), String> {
        let saved = self.host_mut(host).ok_or("No saved host by that name")?;
        let clash = saved.forwards.iter().any(|other| {
            Some(&other.id()) != replacing
                && other.kind == forward.kind
                && other.listen_port == forward.listen_port
        });
        if clash {
            return Err("Another rule already listens on that port".into());
        }
        match replacing.and_then(|id| saved.forwards.iter().position(|f| f.id() == *id)) {
            Some(index) => saved.forwards[index] = forward,
            None => saved.forwards.push(forward),
        }
        saved.updated = now_ms();
        Ok(())
    }

    pub fn delete_forward(&mut self, host: &str, id: &ForwardId) {
        if let Some(saved) = self.host_mut(host) {
            saved.forwards.retain(|forward| forward.id() != *id);
            saved.updated = now_ms();
        }
    }

    /// Switches a rule on or off; it then runs, or doesn't, whenever a
    /// session to the host is open.
    pub fn switch_forward(&mut self, host: &str, id: &ForwardId, on: bool) {
        if let Some(saved) = self.host_mut(host)
            && let Some(forward) = saved.forwards.iter_mut().find(|f| f.id() == *id)
        {
            forward.on = on;
            saved.updated = now_ms();
        }
    }

    /// Tags used on other hosts that `current` doesn't have, most used
    /// first.
    pub fn tag_suggestions(&self, current: &[String], limit: usize) -> Vec<String> {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for tag in self.hosts().flat_map(|host| &host.tags) {
            *counts.entry(tag.as_str()).or_default() += 1;
        }
        let mut tags: Vec<(&str, usize)> = counts
            .into_iter()
            .filter(|(tag, _)| !current.iter().any(|have| have.eq_ignore_ascii_case(tag)))
            .collect();
        tags.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        tags.into_iter()
            .take(limit)
            .map(|(tag, _)| tag.to_string())
            .collect()
    }

    /// Writes the vault to `path`: to a temporary file first, then renamed
    /// over the old one, so a crash mid-write never leaves half a file.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let mut json = serde_json::to_string_pretty(&FileOut {
            version: FILE_VERSION,
            groups: &self.groups,
            snippets: &self.snippets,
            deleted: &self.deleted,
        })?;
        json.push('\n');
        paths::write_atomically(path, json.as_bytes())
    }
}

/// The vault file's shape. Bump it when a change would make an older Tern
/// misread the file (dropping something it doesn't know when it saves);
/// older versions then leave it alone instead. 2 added snippets, 3 port
/// forwarding rules, 4 ids, change times and deletions for sync.
pub const FILE_VERSION: u32 = 4;

/// The vault file as written.
#[derive(Serialize)]
struct FileOut<'a> {
    version: u32,
    groups: &'a [HostGroup],
    snippets: &'a [Snippet],
    deleted: &'a [Tombstone],
}

/// The vault file as read, from any version up to [`FILE_VERSION`].
#[derive(Deserialize)]
struct FileIn {
    #[serde(default)]
    groups: Vec<HostGroup>,
    #[serde(default)]
    snippets: Vec<Snippet>,
    #[serde(default)]
    deleted: Vec<Tombstone>,
}

/// Just the version, read first so a newer file isn't misread.
#[derive(Deserialize)]
struct FileVersion {
    version: Option<u32>,
}

/// What opening the vault file found.
#[derive(Debug)]
pub struct Opened {
    pub vault: Vault,
    /// There was no file yet: a first run.
    pub fresh: bool,
    /// Where to save changes. `None` when saving would replace a file this
    /// Tern can't use, which is never overwritten.
    pub path: Option<PathBuf>,
    /// One line for the user when the file couldn't be used.
    pub notice: Option<String>,
}

/// Opens the vault file at `path`. A missing file is a fresh, empty vault.
/// A file that can't be read is moved aside (as `vault.json.unreadable`,
/// numbered if that exists) and the vault starts empty; one from a newer
/// Tern stays where it is and nothing is saved over it.
pub fn open(path: &Path) -> Opened {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Opened {
                vault: Vault::default(),
                fresh: true,
                path: Some(path.to_path_buf()),
                notice: None,
            };
        }
        Err(error) => return set_aside(path, &error.to_string()),
    };
    match parse(&text) {
        Ok(vault) => Opened {
            vault,
            fresh: false,
            path: Some(path.to_path_buf()),
            notice: None,
        },
        Err(Unusable::Newer(version)) => Opened {
            vault: Vault::default(),
            fresh: false,
            path: None,
            notice: Some(format!(
                "{} is from a newer Tern (version {version}), so changes here aren't saved",
                path.display()
            )),
        },
        Err(Unusable::Broken(reason)) => set_aside(path, &reason),
    }
}

enum Unusable {
    Newer(u32),
    Broken(String),
}

fn parse(text: &str) -> Result<Vault, Unusable> {
    // Some Windows editors start UTF-8 files with a byte-order mark.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let broken = |error: serde_json::Error| Unusable::Broken(error.to_string());
    match serde_json::from_str::<FileVersion>(text)
        .map_err(broken)?
        .version
    {
        None => return Err(Unusable::Broken("no version".into())),
        Some(version) if version > FILE_VERSION => return Err(Unusable::Newer(version)),
        Some(_) => {}
    }
    let file: FileIn = serde_json::from_str(text).map_err(broken)?;
    Ok(Vault {
        groups: file.groups,
        snippets: file.snippets,
        deleted: file.deleted,
    })
}

/// Moves an unreadable vault file out of the way, never over another file,
/// and starts empty.
fn set_aside(path: &Path, reason: &str) -> Opened {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let aside = (1..)
        .map(|n| match n {
            1 => path.with_file_name(format!("{name}.unreadable")),
            n => path.with_file_name(format!("{name}.unreadable.{n}")),
        })
        .find(|candidate| !candidate.exists())
        .expect("an unused name");
    match std::fs::rename(path, &aside) {
        Ok(()) => Opened {
            vault: Vault::default(),
            fresh: false,
            path: Some(path.to_path_buf()),
            notice: Some(format!(
                "Couldn't read {name} ({reason}); it was moved to {}",
                aside.display()
            )),
        },
        // It can't even be moved: leave it alone and save nothing over it.
        Err(error) => Opened {
            vault: Vault::default(),
            fresh: false,
            path: None,
            notice: Some(format!(
                "Couldn't read {} ({reason}) or move it aside ({error})",
                path.display()
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Auth;

    fn names(vault: &Vault, group: usize) -> Vec<&str> {
        vault.groups[group]
            .hosts
            .iter()
            .map(|host| host.name.as_str())
            .collect()
    }

    /// A fresh folder for one test's files.
    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tern-vault-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_adds_edits_in_place_and_moves_between_groups() {
        let mut vault = Vault::sample();
        vault.save(None, Host::new("new", "root", "10.0.0.9", 22), "Production");
        assert_eq!(names(&vault, 0).last(), Some(&"new"));

        let mut web = vault.find("prod-web-01").unwrap().clone();
        web.port = 2200;
        vault.save(Some("prod-web-01"), web, "Production");
        assert_eq!(names(&vault, 0)[1], "prod-web-01");
        assert_eq!(vault.find("prod-web-01").unwrap().port, 2200);

        let api = vault.find("stage-api").unwrap().clone();
        vault.save(Some("stage-api"), api, "Production");
        assert_eq!(vault.group_of("stage-api"), Some("Production"));
        assert!(!names(&vault, 1).contains(&"stage-api"));

        vault.save(None, Host::new("nas", "admin", "nas.lan", 22), "Home");
        assert_eq!(vault.group_of("nas"), Some("Home"));
    }

    #[test]
    fn a_group_goes_when_its_last_host_does() {
        let mut vault = Vault::sample();
        vault.save(None, Host::new("nas", "admin", "nas.lan", 22), "Home");
        let nas = vault.find("nas").unwrap().clone();
        vault.save(Some("nas"), nas, "Production");
        assert!(vault.groups.iter().all(|group| group.name != "Home"));

        for name in ["stage-api", "stage-worker", "pi-homelab"] {
            vault.delete(name).unwrap();
        }
        assert_eq!(vault.groups.len(), 1);
    }

    #[test]
    fn renaming_a_jump_host_keeps_the_hosts_behind_it() {
        let mut vault = Vault::sample();
        let mut db = vault.find("prod-db-01").unwrap().clone();
        db.jump = Some("bastion".into());
        vault.save(Some("prod-db-01"), db, "Production");

        let mut bastion = vault.find("bastion").unwrap().clone();
        bastion.name = "gateway".into();
        vault.save(Some("bastion"), bastion, "Production");
        assert_eq!(
            vault.find("prod-db-01").unwrap().jump.as_deref(),
            Some("gateway")
        );

        assert_eq!(vault.delete("gateway"), Err(vec!["prod-db-01".to_string()]));
        assert!(vault.find("gateway").is_some());
        assert_eq!(vault.delete("prod-db-01"), Ok(()));
        assert!(vault.find("prod-db-01").is_none());
    }

    #[test]
    fn connection_resolves_the_jump_host() {
        let vault = Vault::sample();
        let mut db = vault.find("prod-db-01").unwrap().clone();
        db.jump = Some("bastion".into());
        db.auth = Auth::Password;
        db.on_login = Some("cd /srv/app".into());

        let connection = vault.connection(&db).unwrap();
        let jump = connection.jump.unwrap();
        assert_eq!(
            (jump.target.host.as_str(), jump.target.port),
            ("bastion.example.com", 2222)
        );
        assert_eq!(jump.auth, Auth::Agent);
        assert_eq!(connection.auth, Auth::Password);
        assert_eq!(connection.on_login.as_deref(), Some("cd /srv/app"));

        db.jump = Some("gone".into());
        assert!(vault.connection(&db).is_err());
    }

    #[test]
    fn suggests_the_most_used_tags_this_host_lacks() {
        let vault = Vault::sample();
        let have = vec!["Docker".to_string()];
        assert_eq!(vault.tag_suggestions(&have, 2), ["eu-west", "nginx"]);
    }

    #[test]
    fn a_host_is_checked_at_its_first_hop() {
        let mut vault = Vault::sample();
        let db = vault.find("prod-db-01").unwrap().clone();
        assert_eq!(vault.first_hop(&db), ("10.0.4.21".to_string(), 22));
        let mut db = db;
        db.jump = Some("bastion".into());
        vault.save(Some("prod-db-01"), db.clone(), "Production");
        assert_eq!(
            vault.first_hop(&db),
            ("bastion.example.com".to_string(), 2222)
        );
    }

    #[test]
    fn the_file_keeps_every_host_setting() {
        let dir = scratch("round-trip");
        let path = dir.join("vault.json");
        let mut vault = Vault::sample();
        let mut db = vault.find("prod-db-01").unwrap().clone();
        db.auth = Auth::Key(PathBuf::from("keys/id_ed25519"));
        db.jump = Some("bastion".into());
        db.on_login = Some("cd /srv".into());
        db.keep_alive = false;
        vault.save(Some("prod-db-01"), db, "Production");

        vault.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\": 4"), "{text}");
        assert!(text.contains("df -h"), "snippets are kept too");
        assert!(!text.contains("latency") && !text.contains("status"));
        assert!(!dir.join("vault.json.tmp").exists());

        let opened = open(&path);
        assert!(!opened.fresh && opened.notice.is_none());
        assert_eq!(opened.path.as_deref(), Some(path.as_path()));
        assert_eq!(opened.vault, vault);

        // Saving again replaces the file.
        vault.delete("pi-homelab").unwrap();
        vault.save_to(&path).unwrap();
        assert!(open(&path).vault.find("pi-homelab").is_none());

        // A byte-order mark from a hand edit is fine.
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("\u{feff}{text}")).unwrap();
        let opened = open(&path);
        assert!(opened.notice.is_none() && opened.vault.find("bastion").is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_port_forwarding_rules_per_host() {
        use crate::forward::ForwardKind;

        let mut vault = Vault::sample();
        let db = Forward::typed(ForwardKind::Local, "5432", "localhost:5432").unwrap();
        let socks = Forward {
            on: false,
            ..Forward::typed(ForwardKind::Dynamic, "1080", "").unwrap()
        };
        vault.save_forward("bastion", None, db.clone()).unwrap();
        vault.save_forward("bastion", None, socks.clone()).unwrap();
        assert!(vault.save_forward("nowhere", None, db.clone()).is_err());
        let again = Forward::typed(ForwardKind::Local, "5432", "other:1").unwrap();
        assert!(vault.save_forward("bastion", None, again.clone()).is_err());
        // Editing a rule may keep its own port.
        vault
            .save_forward("bastion", Some(&db.id()), again.clone())
            .unwrap();

        // A session gets the rules that are on.
        let bastion = vault.find("bastion").unwrap();
        assert_eq!(
            vault.connection(bastion).unwrap().forwards,
            std::slice::from_ref(&again)
        );
        vault.switch_forward("bastion", &socks.id(), true);
        vault.switch_forward("bastion", &again.id(), false);
        let bastion = vault.find("bastion").unwrap();
        let on: Vec<u16> = vault
            .connection(bastion)
            .unwrap()
            .forwards
            .iter()
            .map(|forward| forward.listen_port)
            .collect();
        assert_eq!(on, [1080]);

        // Editing the host keeps them; deleting one removes it.
        let edited = crate::draft::HostDraft::edit(bastion, "Production")
            .check(&vault, Some("bastion"))
            .unwrap();
        assert_eq!(edited.forwards.len(), 2);
        vault.delete_forward("bastion", &socks.id());
        assert_eq!(vault.find("bastion").unwrap().forwards.len(), 1);
    }

    #[test]
    fn saves_and_deletes_snippets() {
        let mut vault = Vault::sample();
        let typed = Snippet::new;
        let shown = |snippet: &Snippet| (snippet.name.clone(), snippet.command.clone());
        vault
            .save_snippet(None, &typed(" Load ", "uptime"))
            .unwrap();
        assert_eq!(
            vault.snippets.last().map(shown),
            Some(("Load".into(), "uptime".into()))
        );
        assert!(vault.snippets.last().unwrap().updated > 0);
        assert!(vault.save_snippet(None, &typed("load", "w")).is_err());
        let disk = vault.snippets[0].id.clone();
        vault
            .save_snippet(Some("Disk space"), &typed("Disk", "df -h /"))
            .unwrap();
        assert_eq!(shown(&vault.snippets[0]), ("Disk".into(), "df -h /".into()));
        assert_eq!(vault.snippets[0].id, disk, "an edit keeps the id");
        vault.delete_snippet("Disk");
        assert_eq!(vault.snippets.len(), 2);
        assert_eq!(vault.deleted[0].id, disk, "a deletion is remembered");
    }

    #[test]
    fn a_version_1_file_still_opens() {
        let dir = scratch("version-1");
        let path = dir.join("vault.json");
        let old = r#"{"version": 1, "groups": [{"name": "Lab", "hosts": [
            {"name": "pi", "user": "pi", "address": "10.0.0.2", "port": 22}]}]}"#;
        std::fs::write(&path, old).unwrap();
        let opened = open(&path);
        assert!(opened.notice.is_none());
        assert_eq!(opened.vault.find("pi").map(|pi| pi.keep_alive), Some(true));
        assert!(opened.vault.snippets.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_file_is_a_fresh_empty_vault() {
        let dir = scratch("fresh");
        let opened = open(&dir.join("vault.json"));
        assert!(opened.fresh && opened.notice.is_none() && opened.path.is_some());
        assert_eq!(opened.vault, Vault::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unreadable_file_is_moved_aside_never_over_another() {
        let dir = scratch("unreadable");
        let path = dir.join("vault.json");
        std::fs::write(&path, "{ not json").unwrap();
        let opened = open(&path);
        assert!(!opened.fresh);
        assert_eq!(opened.vault, Vault::default());
        assert_eq!(opened.path.as_deref(), Some(path.as_path()));
        assert!(opened.notice.unwrap().contains("vault.json.unreadable"));
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("vault.json.unreadable")).unwrap(),
            "{ not json"
        );

        // A second bad file doesn't replace the first one set aside.
        std::fs::write(&path, r#"{"groups": []}"#).unwrap();
        assert!(open(&path).notice.unwrap().contains("no version"));
        assert!(dir.join("vault.json.unreadable.2").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("vault.json.unreadable")).unwrap(),
            "{ not json"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_newer_file_is_left_alone() {
        let dir = scratch("newer");
        let path = dir.join("vault.json");
        let newer = r#"{"version": 99, "groups": [], "everything": "else"}"#;
        std::fs::write(&path, newer).unwrap();
        let opened = open(&path);
        assert!(opened.path.is_none(), "nothing may be saved over it");
        assert!(opened.notice.unwrap().contains("newer Tern"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
