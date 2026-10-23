//! Sync through storage the user already has: a folder that a desktop sync
//! client (Google Drive, Dropbox, OneDrive, iCloud Drive, Nextcloud…)
//! keeps in step between computers. No Tern servers and no accounts.
//!
//! Each device writes its own file, `<folder>/Tern/<device id>.tern`, so
//! two computers never write the same file and sync clients never make
//! conflicted copies. A sync reads every other device's file, merges them
//! into the local vault (the newest change of each host or snippet wins,
//! deletions included), and writes the result as this device's file.
//!
//! Files are encrypted on the device with the sync passphrase: Argon2id
//! derives a key from it and a random salt, and XChaCha20-Poly1305 seals
//! the JSON. A file is `TERNSYN1`, the salt (16 bytes), the nonce (24) and
//! the sealed data.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::Aead as _;
use chacha20poly1305::{KeyInit as _, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

use crate::host::{Host, HostGroup, new_id, now_ms};
use crate::identity::Identity;
use crate::keys::{self, KeyPair};
use crate::paths;
use crate::settings::{Appearance, Settings, TerminalSettings};
use crate::snippet::Snippet;
use crate::vault::{Tombstone, Vault};

/// The folder inside the chosen one that holds the devices' files.
pub const FOLDER: &str = "Tern";
const MAGIC: &[u8; 8] = b"TERNSYN1";
const SALT: usize = 16;
const NONCE: usize = 24;
/// Argon2id's cost: 64 MiB, three passes, one lane.
const MEMORY_KIB: u32 = 64 * 1024;
const PASSES: u32 = 3;

/// How this device syncs, kept with the settings (not synced itself).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncSettings {
    /// The folder a sync client keeps in step; `None` while sync is off.
    pub folder: Option<PathBuf>,
    /// This device's name, as other devices list it.
    pub device: String,
    /// Names this device's file; made when sync is first set up.
    pub device_id: String,
    pub hosts: bool,
    pub snippets: bool,
    pub forwards: bool,
    pub settings: bool,
    /// Fingerprints of the private keys that sync, chosen one by one.
    pub keys: Vec<String>,
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self {
            folder: None,
            device: default_device_name(),
            device_id: String::new(),
            hosts: true,
            snippets: true,
            forwards: true,
            settings: true,
            keys: Vec::new(),
        }
    }
}

impl SyncSettings {
    /// Gives this device an id if it has none yet.
    pub fn ensure_device_id(&mut self) {
        if self.device_id.is_empty() {
            self.device_id = new_id();
        }
    }
}

/// This computer's name, as the system knows it.
pub fn default_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "This computer".into())
}

/// What one device wrote.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub device: String,
    pub device_id: String,
    /// When it was written, in milliseconds since the Unix epoch.
    pub written: u64,
    /// Which kinds it carries; a kind it leaves out says nothing about
    /// what the others have.
    #[serde(default)]
    pub has_hosts: bool,
    #[serde(default)]
    pub has_snippets: bool,
    #[serde(default)]
    pub has_forwards: bool,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub hosts: Vec<SyncedHost>,
    #[serde(default)]
    pub snippets: Vec<Snippet>,
    /// Identities travel with hosts.
    #[serde(default)]
    pub identities: Vec<Identity>,
    #[serde(default)]
    pub deleted: Vec<Tombstone>,
    #[serde(default)]
    pub settings: Option<SyncedSettings>,
    #[serde(default)]
    pub keys: Vec<SyncedKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncedHost {
    pub group: String,
    #[serde(flatten)]
    pub host: Host,
}

/// The settings that follow you between computers: how Tern looks and its
/// terminals behave.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SyncedSettings {
    pub appearance: Appearance,
    pub terminal: TerminalSettings,
    pub updated: u64,
}

/// A private key chosen to sync, as its files hold it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncedKey {
    pub name: String,
    pub private: String,
    pub public: String,
    pub fingerprint: String,
}

/// What a sync brings in, merged with what's here.
#[derive(Clone, Debug, PartialEq)]
pub struct Merged {
    pub vault: Vault,
    /// Newer settings from another device, to take.
    pub settings: Option<SyncedSettings>,
    /// Keys other devices sync, for this one to keep.
    pub keys: Vec<SyncedKey>,
}

/// What this device writes: the kinds it syncs from `vault` and
/// `settings`, and `keys`.
pub fn snapshot(vault: &Vault, settings: &Settings, keys: Vec<SyncedKey>) -> Snapshot {
    let sync = &settings.sync;
    let hosts = if sync.hosts {
        vault
            .groups
            .iter()
            .flat_map(|group| {
                group.hosts.iter().map(|host| {
                    let mut host = host.clone();
                    if !sync.forwards {
                        host.forwards.clear();
                    }
                    SyncedHost {
                        group: group.name.clone(),
                        host,
                    }
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    Snapshot {
        device: sync.device.clone(),
        device_id: sync.device_id.clone(),
        written: now_ms(),
        has_hosts: sync.hosts,
        has_snippets: sync.snippets,
        has_forwards: sync.hosts && sync.forwards,
        groups: if sync.hosts {
            vault
                .groups
                .iter()
                .map(|group| group.name.clone())
                .collect()
        } else {
            Vec::new()
        },
        hosts,
        snippets: if sync.snippets {
            vault.snippets.clone()
        } else {
            Vec::new()
        },
        identities: if sync.hosts {
            vault.identities.clone()
        } else {
            Vec::new()
        },
        deleted: if sync.hosts || sync.snippets {
            vault.deleted.clone()
        } else {
            Vec::new()
        },
        settings: sync.settings.then(|| SyncedSettings {
            appearance: settings.appearance,
            terminal: settings.terminal.clone(),
            updated: settings.updated,
        }),
        keys,
    }
}

/// Merges other devices' snapshots into `local`: for each host and snippet
/// the newest change wins, a deletion newer than a change removes it, and
/// kinds this device doesn't sync are left as they are. Hosts that end up
/// sharing a name get " (2)"… in an order every device agrees on.
pub fn merge(local: &Vault, settings: &Settings, others: &[Snapshot]) -> Merged {
    let sync = &settings.sync;
    let mut deleted: HashMap<String, u64> = local
        .deleted
        .iter()
        .map(|tombstone| (tombstone.id.clone(), tombstone.at))
        .collect();
    for snapshot in others {
        for tombstone in &snapshot.deleted {
            let at = deleted.entry(tombstone.id.clone()).or_default();
            *at = (*at).max(tombstone.at);
        }
    }
    let gone = |id: &str, updated: u64| deleted.get(id).is_some_and(|&at| at >= updated);

    // Hosts, in local order, then newcomers in the order they're met.
    let mut order: Vec<String> = local
        .groups
        .iter()
        .map(|group| group.name.clone())
        .collect();
    let mut hosts: Vec<(String, Host)> = local
        .groups
        .iter()
        .flat_map(|group| {
            group
                .hosts
                .iter()
                .map(|host| (group.name.clone(), host.clone()))
        })
        .collect();
    if sync.hosts {
        for snapshot in others.iter().filter(|snapshot| snapshot.has_hosts) {
            for group in &snapshot.groups {
                if !order.contains(group) {
                    order.push(group.clone());
                }
            }
            for incoming in &snapshot.hosts {
                let take_forwards = sync.forwards && snapshot.has_forwards;
                match hosts
                    .iter()
                    .position(|(_, host)| host.id == incoming.host.id)
                {
                    Some(index) if incoming.host.updated > hosts[index].1.updated => {
                        let mut host = incoming.host.clone();
                        if !take_forwards {
                            host.forwards = hosts[index].1.forwards.clone();
                        }
                        hosts[index] = (incoming.group.clone(), host);
                    }
                    Some(_) => {}
                    None => {
                        let mut host = incoming.host.clone();
                        if !take_forwards {
                            host.forwards.clear();
                        }
                        if !order.contains(&incoming.group) {
                            order.push(incoming.group.clone());
                        }
                        hosts.push((incoming.group.clone(), host));
                    }
                }
            }
        }
    }
    hosts.retain(|(_, host)| !gone(&host.id, host.updated));
    tell_apart(&mut hosts);
    let groups = order
        .into_iter()
        .map(|name| HostGroup {
            hosts: hosts
                .iter()
                .filter(|(group, _)| *group == name)
                .map(|(_, host)| host.clone())
                .collect(),
            name,
        })
        .filter(|group| !group.hosts.is_empty())
        .collect();

    let mut snippets = local.snippets.clone();
    if sync.snippets {
        for snapshot in others.iter().filter(|snapshot| snapshot.has_snippets) {
            for incoming in &snapshot.snippets {
                match snippets
                    .iter()
                    .position(|snippet| snippet.id == incoming.id)
                {
                    Some(index) if incoming.updated > snippets[index].updated => {
                        snippets[index] = incoming.clone()
                    }
                    Some(_) => {}
                    None => snippets.push(incoming.clone()),
                }
            }
        }
    }
    snippets.retain(|snippet| !gone(&snippet.id, snippet.updated));

    let mut identities = local.identities.clone();
    if sync.hosts {
        for snapshot in others.iter().filter(|snapshot| snapshot.has_hosts) {
            for incoming in &snapshot.identities {
                match identities
                    .iter()
                    .position(|identity| identity.id == incoming.id)
                {
                    Some(index) if incoming.updated > identities[index].updated => {
                        identities[index] = incoming.clone()
                    }
                    Some(_) => {}
                    None => identities.push(incoming.clone()),
                }
            }
        }
    }
    identities.retain(|identity| !gone(&identity.id, identity.updated));

    let mut tombstones: Vec<Tombstone> = deleted
        .into_iter()
        .map(|(id, at)| Tombstone { id, at })
        .collect();
    tombstones.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| a.id.cmp(&b.id)));
    tombstones.truncate(1000);

    let newer = sync
        .settings
        .then(|| {
            others
                .iter()
                .filter_map(|snapshot| snapshot.settings.as_ref())
                .filter(|synced| synced.updated > settings.updated)
                .max_by_key(|synced| synced.updated)
                .cloned()
        })
        .flatten();

    let mut keys: Vec<SyncedKey> = Vec::new();
    for key in others.iter().flat_map(|snapshot| &snapshot.keys) {
        if !keys.iter().any(|have| have.fingerprint == key.fingerprint) {
            keys.push(key.clone());
        }
    }

    Merged {
        vault: Vault {
            groups,
            snippets,
            identities,
            deleted: tombstones,
        },
        settings: newer,
        keys,
    }
}

/// Gives hosts that share a name (ignoring case) distinct ones: the
/// earliest changed keeps it, the rest get " (2)", " (3)"…
fn tell_apart(hosts: &mut [(String, Host)]) {
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, (_, host)) in hosts.iter().enumerate() {
        by_name
            .entry(host.name.to_lowercase())
            .or_default()
            .push(index);
    }
    for mut same in by_name.into_values().filter(|same| same.len() > 1) {
        same.sort_by(|&a, &b| {
            let (a, b) = (&hosts[a].1, &hosts[b].1);
            a.updated.cmp(&b.updated).then_with(|| a.id.cmp(&b.id))
        });
        for (n, &index) in same.iter().enumerate().skip(1) {
            let host = &mut hosts[index].1;
            host.name = format!("{} ({})", host.name, n + 1);
        }
    }
}

/// The private keys `fingerprints` names, from `pairs`, to sync.
pub fn keys_to_sync(pairs: &[KeyPair], fingerprints: &[String]) -> Vec<SyncedKey> {
    pairs
        .iter()
        .filter(|pair| fingerprints.contains(&pair.fingerprint))
        .filter_map(|pair| {
            Some(SyncedKey {
                name: pair.name.clone(),
                private: std::fs::read_to_string(&pair.path).ok()?,
                public: pair.public.clone(),
                fingerprint: pair.fingerprint.clone(),
            })
        })
        .collect()
}

/// Writes synced keys this device lacks into Tern's key folder `dir`, under
/// free names. Returns how many it wrote.
pub fn receive_keys(dir: &Path, synced: &[SyncedKey], have: &[KeyPair]) -> Result<usize, String> {
    let mut written = 0;
    for key in synced
        .iter()
        .filter(|key| !have.iter().any(|pair| pair.fingerprint == key.fingerprint))
    {
        let path = dir.join(keys::free_name(dir, &key.name));
        keys::write_pair(&path, key.private.as_bytes(), &key.public)?;
        written += 1;
    }
    Ok(written)
}

/// Seals and opens sync files with one passphrase, remembering the keys it
/// derived so a sync doesn't redo Argon2 for every file.
pub struct Syncer {
    passphrase: String,
    derived: HashMap<[u8; SALT], [u8; 32]>,
    /// The salt of this device's own file, kept for the session.
    own_salt: [u8; SALT],
}

impl Syncer {
    pub fn new(passphrase: &str) -> Self {
        let mut own_salt = [0u8; SALT];
        rand::fill(&mut own_salt);
        Self {
            passphrase: passphrase.into(),
            derived: HashMap::new(),
            own_salt,
        }
    }

    fn key(&mut self, salt: [u8; SALT]) -> Result<[u8; 32], String> {
        if let Some(key) = self.derived.get(&salt) {
            return Ok(*key);
        }
        let params =
            Params::new(MEMORY_KIB, PASSES, 1, Some(32)).map_err(|error| error.to_string())?;
        let mut key = [0u8; 32];
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(self.passphrase.as_bytes(), &salt, &mut key)
            .map_err(|error| error.to_string())?;
        self.derived.insert(salt, key);
        Ok(key)
    }

    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        let salt = self.own_salt;
        let key = self.key(salt)?;
        let mut nonce = [0u8; NONCE];
        rand::fill(&mut nonce);
        let sealed = XChaCha20Poly1305::new_from_slice(&key)
            .map_err(|error| error.to_string())?
            .encrypt(&XNonce::from(nonce), plaintext)
            .map_err(|_| "can't encrypt".to_string())?;
        Ok([MAGIC.as_slice(), &salt, &nonce, &sealed].concat())
    }

    pub fn unseal(&mut self, file: &[u8]) -> Result<Vec<u8>, String> {
        let rest = file
            .strip_prefix(MAGIC.as_slice())
            .filter(|rest| rest.len() > SALT + NONCE)
            .ok_or("not a Tern sync file")?;
        let (salt, rest) = rest.split_at(SALT);
        let (nonce, sealed) = rest.split_at(NONCE);
        let key = self.key(salt.try_into().expect("the salt's length"))?;
        let nonce: [u8; NONCE] = nonce.try_into().expect("the nonce's length");
        XChaCha20Poly1305::new_from_slice(&key)
            .map_err(|error| error.to_string())?
            .decrypt(&XNonce::from(nonce), sealed)
            .map_err(|_| "the passphrase doesn't open the other devices' files".to_string())
    }

    /// Every other device's snapshot in `folder`.
    pub fn read(&mut self, folder: &Path, own_id: &str) -> Result<Vec<Snapshot>, String> {
        if !folder.is_dir() {
            return Err(format!("{} isn't there", folder.display()));
        }
        let dir = folder.join(FOLDER);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut snapshots = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let own = path.file_stem().and_then(|stem| stem.to_str()) == Some(own_id);
            if own || path.extension().and_then(|extension| extension.to_str()) != Some("tern") {
                continue;
            }
            let Ok(file) = std::fs::read(&path) else {
                // Still arriving from the sync client; next time.
                continue;
            };
            let opened = self.unseal(&file)?;
            let snapshot: Snapshot =
                serde_json::from_slice(&opened).map_err(|error| error.to_string())?;
            snapshots.push(snapshot);
        }
        snapshots.sort_by(|a, b| a.device.cmp(&b.device));
        Ok(snapshots)
    }

    /// Writes this device's snapshot into `folder`.
    pub fn write(&mut self, folder: &Path, snapshot: &Snapshot) -> Result<(), String> {
        let json = serde_json::to_vec(snapshot).map_err(|error| error.to_string())?;
        let sealed = self.seal(&json)?;
        let path = folder
            .join(FOLDER)
            .join(format!("{}.tern", snapshot.device_id));
        paths::write_atomically(&path, &sealed).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Auth;

    fn settings(device: &str) -> Settings {
        Settings {
            sync: SyncSettings {
                device: device.into(),
                device_id: device.to_lowercase(),
                ..SyncSettings::default()
            },
            ..Settings::default()
        }
    }

    /// Two devices that synced once: both have the sample vault.
    fn pair() -> (Vault, Vault) {
        let vault = Vault::sample();
        (vault.clone(), vault)
    }

    /// Lets the clock move on, so the next change is the newer one.
    fn later() {
        std::thread::sleep(std::time::Duration::from_millis(3));
    }

    #[test]
    fn seals_with_the_passphrase_only() {
        let mut syncer = Syncer::new("correct horse");
        let sealed = syncer.seal(b"hosts").unwrap();
        assert!(sealed.starts_with(MAGIC));
        assert_eq!(
            Syncer::new("correct horse").unseal(&sealed).unwrap(),
            b"hosts"
        );
        assert!(Syncer::new("wrong").unseal(&sealed).is_err());
        assert!(Syncer::new("x").unseal(b"TERNSYN1short").is_err());
    }

    #[test]
    fn identities_travel_with_hosts() {
        use crate::host::Auth;
        use crate::identity::Identity;

        let (here, mut there) = pair();
        let ops = Identity::new("ops", "operator", Auth::Agent);
        there.save_identity(ops.clone());
        let from_there = snapshot(&there, &settings("There"), Vec::new());
        let merged = merge(&here, &settings("Here"), &[from_there]).vault;
        assert_eq!(merged.identities.len(), 1);
        assert_eq!(merged.identities[0].id, ops.id);

        // A deletion there removes it here too.
        there.delete_identity(&ops.id).unwrap();
        let from_there = snapshot(&there, &settings("There"), Vec::new());
        let merged = merge(&merged, &settings("Here"), &[from_there]).vault;
        assert!(merged.identities.is_empty());
    }

    #[test]
    fn the_newest_change_wins_and_deletions_travel() {
        let (mut here, mut there) = pair();
        // There: bastion moves to port 2200; here: prod-db-01 is deleted,
        // and a host is added.
        let mut bastion = there.find("bastion").unwrap().clone();
        bastion.port = 2200;
        there.save(Some("bastion"), bastion, "Production");
        here.delete("prod-db-01").unwrap();
        here.save(None, Host::new("nas", "admin", "nas.lan", 22), "Home");

        let from_there = snapshot(&there, &settings("There"), Vec::new());
        let merged = merge(&here, &settings("Here"), &[from_there]).vault;
        assert_eq!(merged.find("bastion").unwrap().port, 2200);
        assert!(merged.find("prod-db-01").is_none());
        assert_eq!(merged.group_of("nas"), Some("Home"));

        // And back: there learns about the deletion and the new host.
        let from_here = snapshot(&merged, &settings("Here"), Vec::new());
        let back = merge(&there, &settings("There"), &[from_here]).vault;
        assert!(back.find("prod-db-01").is_none());
        assert!(back.find("nas").is_some());
        assert_eq!(back.find("bastion").unwrap().port, 2200);

        // An older edit of the same host loses.
        let pi = here.find("pi-homelab").unwrap().clone();
        here.save(Some("pi-homelab"), pi.clone(), "Staging & lab");
        let mut stale = Vault::sample();
        stale.groups[1].hosts[2] = Host {
            user: "stale".into(),
            updated: 1,
            ..pi
        };
        let merged = merge(
            &here,
            &settings("Here"),
            &[snapshot(&stale, &settings("Old"), Vec::new())],
        );
        assert_eq!(merged.vault.find("pi-homelab").unwrap().user, "pi");
    }

    #[test]
    fn kinds_left_out_stay_as_they_are() {
        let (here, mut there) = pair();
        let mut with_rule = there.find("bastion").unwrap().clone();
        with_rule.auth = Auth::Password;
        there.save(Some("bastion"), with_rule, "Production");
        there
            .save_snippet(None, &Snippet::new("Uptime", "uptime"))
            .unwrap();

        // Here doesn't sync snippets; there doesn't send hosts.
        let mut quiet = settings("Here");
        quiet.sync.snippets = false;
        let mut no_hosts = settings("There");
        no_hosts.sync.hosts = false;
        let merged = merge(&here, &quiet, &[snapshot(&there, &no_hosts, Vec::new())]).vault;
        assert_eq!(merged.find("bastion").unwrap().auth, Auth::Agent);
        assert_eq!(merged.snippets.len(), 2);
    }

    #[test]
    fn rules_only_travel_when_both_sides_sync_them() {
        use crate::forward::{Forward, ForwardKind};

        let (mut here, mut there) = pair();
        let rule = Forward::typed(ForwardKind::Local, "8080", "localhost:80").unwrap();
        here.save_forward("bastion", None, rule.clone()).unwrap();
        // There changes bastion later but doesn't sync rules.
        later();
        let mut changed = there.find("bastion").unwrap().clone();
        changed.user = "root".into();
        there.save(Some("bastion"), changed, "Production");
        let mut no_rules = settings("There");
        no_rules.sync.forwards = false;
        let merged = merge(
            &here,
            &settings("Here"),
            &[snapshot(&there, &no_rules, Vec::new())],
        );
        let bastion = merged.vault.find("bastion").unwrap();
        assert_eq!((bastion.user.as_str(), bastion.forwards.len()), ("root", 1));
    }

    #[test]
    fn same_named_hosts_from_two_devices_are_told_apart() {
        let (mut here, mut there) = pair();
        here.save(None, Host::new("nas", "admin", "10.0.0.2", 22), "Home");
        later();
        there.save(None, Host::new("nas", "root", "10.0.0.3", 22), "Home");
        let merged = merge(
            &here,
            &settings("Here"),
            &[snapshot(&there, &settings("There"), Vec::new())],
        );
        let names: Vec<&str> = merged
            .vault
            .groups
            .last()
            .unwrap()
            .hosts
            .iter()
            .map(|h| h.name.as_str())
            .collect();
        assert_eq!(names, ["nas", "nas (2)"]);
    }

    #[test]
    fn newer_settings_come_across() {
        let here = settings("Here");
        let mut there = settings("There");
        there.appearance = Appearance::Dark;
        there.updated = here.updated + 10;
        let merged = merge(
            &Vault::default(),
            &here,
            &[snapshot(&Vault::default(), &there, Vec::new())],
        );
        assert_eq!(
            merged.settings.map(|synced| synced.appearance),
            Some(Appearance::Dark)
        );
        let mut off = here.clone();
        off.sync.settings = false;
        assert!(
            merge(
                &Vault::default(),
                &off,
                &[snapshot(&Vault::default(), &there, Vec::new())]
            )
            .settings
            .is_none()
        );
    }

    #[test]
    fn devices_meet_in_the_folder() {
        let folder = std::env::temp_dir().join(format!("tern-sync-folder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let (here, there) = (settings("Here"), settings("There"));

        let mut theirs = Syncer::new("open sesame");
        theirs
            .write(&folder, &snapshot(&Vault::sample(), &there, Vec::new()))
            .unwrap();
        let mut ours = Syncer::new("open sesame");
        ours.write(&folder, &snapshot(&Vault::default(), &here, Vec::new()))
            .unwrap();
        let read = ours.read(&folder, &here.sync.device_id).unwrap();
        assert_eq!(read.len(), 1, "our own file isn't read back");
        assert_eq!(read[0].device, "There");
        let merged = merge(&Vault::default(), &here, &read).vault;
        assert!(merged.find("bastion").is_some());

        assert!(Syncer::new("wrong").read(&folder, "here").is_err());
        assert!(ours.read(&folder.join("missing"), "here").is_err());
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn keys_travel_only_when_chosen() {
        let (ours, theirs) = (
            std::env::temp_dir().join(format!("tern-sync-keys-a-{}", std::process::id())),
            std::env::temp_dir().join(format!("tern-sync-keys-b-{}", std::process::id())),
        );
        for dir in [&ours, &theirs] {
            let _ = std::fs::remove_dir_all(dir);
        }
        let chosen = keys::generate(&ours, "deploy", None, "").unwrap();
        keys::generate(&ours, "private", None, "").unwrap();
        let pairs = keys::list_in(&[(ours.clone(), true)]);
        let synced = keys_to_sync(&pairs, std::slice::from_ref(&chosen.fingerprint));
        assert_eq!(synced.len(), 1);

        assert_eq!(receive_keys(&theirs, &synced, &[]).unwrap(), 1);
        let arrived = keys::list_in(&[(theirs.clone(), true)]);
        assert_eq!(arrived[0].fingerprint, chosen.fingerprint);
        assert_eq!(receive_keys(&theirs, &synced, &arrived).unwrap(), 0);
        for dir in [ours, theirs] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
