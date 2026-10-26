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
//!
//! Devices on different versions of Tern sync safely. Hosts, snippets,
//! identities, their proxies and rules, and terminal settings keep fields
//! this version doesn't know ([`Unknown`](crate::host::Unknown)), so they go
//! back out as they came. A change that can't travel that way raises
//! [`FORMAT`], and a device that meets a newer format stops syncing until
//! it's updated, rather than merge what it may get wrong.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::Aead as _;
use chacha20poly1305::{KeyInit as _, XChaCha20Poly1305, XNonce};
use russh::keys::PublicKey;
use serde::{Deserialize, Serialize};

use crate::host::{Auth, Host, HostGroup, new_id, now_ms};
use crate::identity::Identity;
use crate::keys::{self, KeyPair};
use crate::paths;
use crate::settings::{Appearance, Settings, TerminalSettings};
use crate::snippet::Snippet;
use crate::ssh::fingerprint;
use crate::vault::{Tombstone, Vault};

/// The folder inside the chosen one that holds the devices' files.
pub const FOLDER: &str = "Tern";
const MAGIC: &[u8; 8] = b"TERNSYN1";
/// The format this version writes. Raise it for a change an older Tern
/// would get wrong even carrying what it doesn't know: a new kind of value
/// (another way to sign in, another kind of rule), a field inside a type
/// that doesn't keep unknown ones, or a changed meaning. Added fields don't
/// need it.
pub const FORMAT: u32 = 1;
const SALT: usize = 16;
/// How long an unchanged file is left as it is: sync clients upload every
/// write, but other devices list when each last wrote.
pub const REWRITE_AFTER: std::time::Duration = std::time::Duration::from_secs(15 * 60);
const NONCE: usize = 24;
/// Argon2id's cost: 64 MiB, three passes, one lane.
const MEMORY_KIB: u32 = 64 * 1024;
const PASSES: u32 = 3;

/// The fewest characters a new sync passphrase may have. It's all that
/// guards the files in a folder a cloud keeps, private keys among them.
pub const MIN_PASSPHRASE: usize = 12;

/// Why `typed` won't do as a new sync passphrase, in one line.
pub fn passphrase_problem(typed: &str) -> Option<&'static str> {
    (typed.chars().count() < MIN_PASSPHRASE).then_some("Use at least 12 characters")
}

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
    /// [`FORMAT`] as the device that wrote it knew it; none before formats.
    #[serde(default)]
    pub format: u32,
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
    /// The key files its hosts and identities sign in with, so a device
    /// that keeps those keys elsewhere finds them there ([`relink`]).
    #[serde(default)]
    pub key_files: Vec<KeyFile>,
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

/// A key file that hosts or identities sign in with, as the device that
/// wrote a snapshot keeps it: where, and the key's fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyFile {
    pub path: PathBuf,
    pub fingerprint: String,
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
        format: FORMAT,
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
        key_files: Vec::new(),
    }
}

/// Merges other devices' snapshots into `local`: for each host and snippet
/// the newest change wins, a deletion newer than a change removes it, and
/// kinds this device doesn't sync are left as they are, deletions
/// included. A snapshot's deletions count only for the kinds it carries:
/// one that leaves hosts out says nothing about them. Hosts that end up
/// sharing a name get " (2)"… in an order every device agrees on.
pub fn merge(local: &Vault, settings: &Settings, others: &[Snapshot]) -> Merged {
    let sync = &settings.sync;
    let ours: HashMap<String, u64> = local
        .deleted
        .iter()
        .map(|tombstone| (tombstone.id.clone(), tombstone.at))
        .collect();
    // The deletions that count for a kind: this device's own and those of
    // the snapshots that carry it.
    let deletions = |carries: fn(&Snapshot) -> bool| {
        let mut deleted = ours.clone();
        for snapshot in others.iter().filter(|snapshot| carries(snapshot)) {
            for tombstone in &snapshot.deleted {
                let at = deleted.entry(tombstone.id.clone()).or_default();
                *at = (*at).max(tombstone.at);
            }
        }
        deleted
    };
    let (host_deletions, snippet_deletions) = (
        deletions(|snapshot| snapshot.has_hosts),
        deletions(|snapshot| snapshot.has_snippets),
    );
    let gone = |deleted: &HashMap<String, u64>, id: &str, updated: u64| {
        deleted.get(id).is_some_and(|&at| at >= updated)
    };

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
    if sync.hosts {
        hosts.retain(|(_, host)| !gone(&host_deletions, &host.id, host.updated));
    }
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
    if sync.snippets {
        snippets.retain(|snippet| !gone(&snippet_deletions, &snippet.id, snippet.updated));
    }

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
    if sync.hosts {
        identities.retain(|identity| !gone(&host_deletions, &identity.id, identity.updated));
    }

    // Every deletion is kept and passed on, a kind this device doesn't
    // sync too: they count once it does.
    let mut deleted = ours.clone();
    for tombstone in others.iter().flat_map(|snapshot| &snapshot.deleted) {
        let at = deleted.entry(tombstone.id.clone()).or_default();
        *at = (*at).max(tombstone.at);
    }
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

    // Only the same key as written twice is left out: a file can't hide
    // another's key by naming its fingerprint first.
    let mut keys: Vec<SyncedKey> = Vec::new();
    for key in others.iter().flat_map(|snapshot| &snapshot.keys) {
        if !keys.contains(key) {
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
/// earliest changed keeps it, the rest get the first of " (2)", " (3)"…
/// that no host has, in an order every device agrees on.
fn tell_apart(hosts: &mut [(String, Host)]) {
    let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, (_, host)) in hosts.iter().enumerate() {
        by_name
            .entry(host.name.to_lowercase())
            .or_default()
            .push(index);
    }
    let mut taken: HashSet<String> = by_name.keys().cloned().collect();
    for mut same in by_name.into_values().filter(|same| same.len() > 1) {
        same.sort_by(|&a, &b| {
            let (a, b) = (&hosts[a].1, &hosts[b].1);
            a.updated.cmp(&b.updated).then_with(|| a.id.cmp(&b.id))
        });
        for &index in same.iter().skip(1) {
            let host = &mut hosts[index].1;
            let free = (2..)
                .map(|n| format!("{} ({n})", host.name))
                .find(|name| !taken.contains(&name.to_lowercase()))
                .expect("a free name");
            taken.insert(free.to_lowercase());
            host.name = free;
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

/// The key files `vault`'s hosts and identities sign in with, each once.
fn key_paths(vault: &Vault) -> Vec<&PathBuf> {
    let mut paths: Vec<&PathBuf> = Vec::new();
    let auths = vault
        .hosts()
        .map(|host| &host.auth)
        .chain(vault.identities.iter().map(|identity| &identity.auth));
    for auth in auths {
        if let Auth::Key(path) = auth
            && !paths.contains(&path)
        {
            paths.push(path);
        }
    }
    paths
}

/// The key files `vault`'s hosts and identities sign in with that are keys
/// this device lists (`pairs`), which are those that can sync, each with
/// its key's fingerprint: for this device's snapshot.
pub fn key_files(vault: &Vault, pairs: &[KeyPair]) -> Vec<KeyFile> {
    let canonical = |path: &Path| std::fs::canonicalize(path).ok();
    key_paths(vault)
        .into_iter()
        .filter_map(|path| {
            let pair = pairs.iter().find(|pair| {
                pair.path == *path
                    || canonical(&pair.path).is_some_and(|it| Some(it) == canonical(path))
            })?;
            Some(KeyFile {
                path: path.clone(),
                fingerprint: pair.fingerprint.clone(),
            })
        })
        .collect()
}

/// Where this device keeps keys that `vault`'s hosts and identities name by
/// another device's path: each key file that isn't here which a snapshot
/// says holds a key this device has (`pairs`), with that key's path here.
/// Files read; run it off the UI thread.
pub fn relink(vault: &Vault, others: &[Snapshot], pairs: &[KeyPair]) -> Vec<(PathBuf, PathBuf)> {
    key_paths(vault)
        .into_iter()
        .filter(|path| !path.exists())
        .filter_map(|path| {
            let here = others
                .iter()
                .flat_map(|snapshot| &snapshot.key_files)
                .filter(|file| file.path == *path)
                .find_map(|file| {
                    pairs
                        .iter()
                        .find(|pair| pair.fingerprint == file.fingerprint)
                })?;
            Some((path.clone(), here.path.clone()))
        })
        .collect()
}

/// Writes synced keys this device lacks where Tern puts keys
/// ([`keys::home`] among `dirs`), under free names. A key that isn't the
/// key pair it says it is is refused, and the others still arrive; the
/// first refusal is the error, in one line. Returns how many it wrote.
pub fn receive_keys(
    dirs: &[(PathBuf, bool)],
    synced: &[SyncedKey],
    have: &[KeyPair],
) -> Result<usize, String> {
    let mut written: Vec<&str> = Vec::new();
    let mut refused = None;
    for key in synced
        .iter()
        .filter(|key| !have.iter().any(|pair| pair.fingerprint == key.fingerprint))
    {
        if written.contains(&key.fingerprint.as_str()) {
            continue;
        }
        let named = PublicKey::from_openssh(key.public.trim())
            .is_ok_and(|public| fingerprint(&public) == key.fingerprint);
        let kept = match named {
            true => keys::add(dirs, &key.name, key.private.as_bytes(), &key.public).map(|_| ()),
            false => Err("it isn't the key it names".into()),
        };
        match kept {
            Ok(()) => written.push(&key.fingerprint),
            Err(reason) => {
                refused.get_or_insert(format!("A synced key was refused: {reason}"));
            }
        }
    }
    refused.map_or(Ok(written.len()), Err)
}

/// What every format of a sync file starts with.
#[derive(Deserialize)]
struct Header {
    #[serde(default)]
    format: u32,
    #[serde(default)]
    device: String,
}

/// Seals and opens sync files with one passphrase, remembering the keys it
/// derived so a sync doesn't redo Argon2 for every file.
pub struct Syncer {
    passphrase: String,
    derived: HashMap<[u8; SALT], [u8; 32]>,
    /// The salt of this device's own file, kept for the session.
    own_salt: [u8; SALT],
    /// The file last written, what it held (but when), and when.
    last: Option<(PathBuf, Vec<u8>, std::time::Instant)>,
}

impl Syncer {
    pub fn new(passphrase: &str) -> Self {
        let mut own_salt = [0u8; SALT];
        rand::fill(&mut own_salt);
        Self {
            passphrase: passphrase.into(),
            derived: HashMap::new(),
            own_salt,
            last: None,
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
            // The format first: a newer one may not read as a snapshot here.
            let header: Header =
                serde_json::from_slice(&opened).map_err(|error| error.to_string())?;
            if header.format > FORMAT {
                return Err(format!(
                    "{} syncs with a newer Tern: update this one to sync",
                    header.device
                ));
            }
            let snapshot: Snapshot =
                serde_json::from_slice(&opened).map_err(|error| error.to_string())?;
            snapshots.push(snapshot);
        }
        snapshots.sort_by(|a, b| a.device.cmp(&b.device));
        Ok(snapshots)
    }

    /// Writes this device's snapshot into `folder`.
    /// Writes `snapshot` as this device's file, unless the file there
    /// already holds it, written less than [`REWRITE_AFTER`] ago.
    pub fn write(&mut self, folder: &Path, snapshot: &Snapshot) -> Result<(), String> {
        let path = folder
            .join(FOLDER)
            .join(format!("{}.tern", snapshot.device_id));
        let held = serde_json::to_vec(&Snapshot {
            written: 0,
            ..snapshot.clone()
        })
        .map_err(|error| error.to_string())?;
        if let Some((last, same, at)) = &self.last
            && *last == path
            && *same == held
            && at.elapsed() < REWRITE_AFTER
            && path.is_file()
        {
            return Ok(());
        }
        let json = serde_json::to_vec(snapshot).map_err(|error| error.to_string())?;
        let sealed = self.seal(&json)?;
        paths::write_atomically(&path, &sealed).map_err(|error| error.to_string())?;
        self.last = Some((path, held, std::time::Instant::now()));
        Ok(())
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

        // A name given to tell two apart is never one a host already has.
        let mut first = Host::new("web", "a", "10.0.0.4", 22);
        first.updated = 1;
        let mut second = Host::new("web", "b", "10.0.0.5", 22);
        second.updated = 2;
        let here = Vault {
            groups: vec![HostGroup {
                name: "Web".into(),
                hosts: vec![first, Host::new("Web (2)", "c", "10.0.0.6", 22)],
            }],
            ..Vault::default()
        };
        let there = Vault {
            groups: vec![HostGroup {
                name: "Web".into(),
                hosts: vec![second],
            }],
            ..Vault::default()
        };
        let merged = merge(
            &here,
            &settings("Here"),
            &[snapshot(&there, &settings("There"), Vec::new())],
        );
        let names: Vec<&str> = merged.vault.hosts().map(|h| h.name.as_str()).collect();
        assert_eq!(names, ["web", "Web (2)", "web (3)"]);
    }

    #[test]
    fn deletions_count_only_for_kinds_both_sides_sync() {
        // Here doesn't sync hosts: a host deleted there stays here...
        let (here, mut there) = pair();
        there.delete("prod-db-01").unwrap();
        let mut no_hosts = settings("Here");
        no_hosts.sync.hosts = false;
        let from_there = snapshot(&there, &settings("There"), Vec::new());
        let merged = merge(&here, &no_hosts, &[from_there]).vault;
        assert!(merged.find("prod-db-01").is_some());
        // ...until it does: the deletion it kept counts then.
        let merged = merge(&merged, &settings("Here"), &[]).vault;
        assert!(merged.find("prod-db-01").is_none());

        // There doesn't sync hosts, and deletes one of its own: its file,
        // which carries snippets and deletions, leaves the host here.
        let (here, mut there) = pair();
        there.delete("prod-db-01").unwrap();
        let mut no_hosts = settings("There");
        no_hosts.sync.hosts = false;
        let from_there = snapshot(&there, &no_hosts, Vec::new());
        assert!(!from_there.deleted.is_empty());
        let merged = merge(&here, &settings("Here"), &[from_there]).vault;
        assert!(merged.find("prod-db-01").is_some());
    }

    #[test]
    fn a_renamed_jump_host_takes_the_hosts_behind_it_along() {
        let mut synced = Vault::sample();
        let mut db = synced.find("prod-db-01").unwrap().clone();
        db.jump = Some("bastion".into());
        synced.save(Some("prod-db-01"), db, "Production");
        let (here, mut there) = (synced.clone(), synced);
        later();
        let mut bastion = there.find("bastion").unwrap().clone();
        bastion.name = "gateway".into();
        there.save(Some("bastion"), bastion, "Production");
        let merged = merge(
            &here,
            &settings("Here"),
            &[snapshot(&there, &settings("There"), Vec::new())],
        )
        .vault;
        let db = merged.find("prod-db-01").unwrap();
        assert_eq!(db.jump.as_deref(), Some("gateway"));
        assert!(merged.connection(db).is_ok());
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
    fn a_passphrase_needs_some_length() {
        assert!(passphrase_problem("hunter2").is_some());
        assert!(passphrase_problem("eleven char").is_some());
        assert_eq!(passphrase_problem("twelve chars"), None);
        assert_eq!(passphrase_problem("čšžćđ čšžćđ!"), None);
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

        // Nothing changed: the file stays as it was, until something does.
        let file = folder
            .join(FOLDER)
            .join(format!("{}.tern", here.sync.device_id));
        let before = std::fs::read(&file).unwrap();
        ours.write(&folder, &snapshot(&Vault::default(), &here, Vec::new()))
            .unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), before);
        ours.write(&folder, &snapshot(&Vault::sample(), &here, Vec::new()))
            .unwrap();
        assert_ne!(std::fs::read(&file).unwrap(), before);
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn carries_what_it_doesnt_know_through() {
        // A newer Tern's file: fields this one has never heard of, on a
        // host, its proxy and rule, a snippet and an identity.
        let mut newer = Vault::sample();
        let rule = crate::forward::Forward::typed(
            crate::forward::ForwardKind::Local,
            "8080",
            "localhost:80",
        );
        newer.groups[0].hosts[0].forwards.push(rule.unwrap());
        newer.groups[0].hosts[0].proxy =
            Some(crate::proxy::Proxy::parse("socks5://p.lan").unwrap());
        newer
            .identities
            .push(Identity::new("ops", "deploy", Auth::Agent));
        // Its settings changed last, so they come across.
        let laptop = Settings {
            updated: 1,
            ..settings("Laptop")
        };
        let mut file = serde_json::to_value(snapshot(&newer, &laptop, Vec::new())).unwrap();
        file["hosts"][0]["shell"] = "fish".into();
        file["hosts"][0]["proxy"]["login"] = "ops".into();
        file["hosts"][0]["forwards"][0]["label"] = "web".into();
        file["snippets"][0]["pinned"] = true.into();
        file["identities"][0]["fido"] = "yes".into();
        file["settings"]["terminal"]["ligatures"] = true.into();
        let file: Snapshot = serde_json::from_value(file).unwrap();

        // Merged here, then written for the others: all of it goes back out.
        let here = settings("Desk");
        let merged = merge(&Vault::default(), &here, &[file]);
        let mut ours = here.clone();
        if let Some(synced) = merged.settings {
            ours.terminal = synced.terminal;
        }
        let out = serde_json::to_value(snapshot(&merged.vault, &ours, Vec::new())).unwrap();
        assert_eq!(out["format"], FORMAT);
        assert_eq!(out["hosts"][0]["shell"], "fish");
        assert_eq!(out["hosts"][0]["proxy"]["login"], "ops");
        assert_eq!(out["hosts"][0]["forwards"][0]["label"], "web");
        assert_eq!(out["snippets"][0]["pinned"], true);
        assert_eq!(out["identities"][0]["fido"], "yes");
        assert_eq!(out["settings"]["terminal"]["ligatures"], true);
    }

    #[test]
    fn stops_at_a_newer_format() {
        let folder = std::env::temp_dir().join(format!("tern-sync-format-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        let mut theirs = Syncer::new("open sesame");
        let read = |folder: &Path| Syncer::new("open sesame").read(folder, "desk");

        // A file from before formats reads as it did.
        let mut older = snapshot(&Vault::sample(), &settings("Old"), Vec::new());
        older.format = 0;
        theirs.write(&folder, &older).unwrap();
        assert_eq!(read(&folder).unwrap().len(), 1);

        // A newer one stops the sync, even one that isn't a snapshot here.
        let newer = br#"{"format": 2, "device": "Laptop", "hosts": {"changed": true}}"#;
        let path = folder.join(FOLDER).join("laptop.tern");
        std::fs::write(&path, theirs.seal(newer).unwrap()).unwrap();
        assert_eq!(
            read(&folder).unwrap_err(),
            "Laptop syncs with a newer Tern: update this one to sync"
        );
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
        let (here, there) = ([(ours.clone(), true)], [(theirs.clone(), true)]);
        let chosen = keys::generate(&here, "deploy", None, "").unwrap();
        keys::generate(&here, "private", None, "").unwrap();
        let pairs = keys::list_in(&here);
        let synced = keys_to_sync(&pairs, std::slice::from_ref(&chosen.fingerprint));
        assert_eq!(synced.len(), 1);

        assert_eq!(receive_keys(&there, &synced, &[]).unwrap(), 1);
        let arrived = keys::list_in(&there);
        assert_eq!(arrived[0].fingerprint, chosen.fingerprint);
        assert_eq!(receive_keys(&there, &synced, &arrived).unwrap(), 0);
        for dir in [ours, theirs] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn hosts_find_a_synced_key_where_this_device_keeps_it() {
        let (ours, theirs) = (
            std::env::temp_dir().join(format!("tern-sync-relink-a-{}", std::process::id())),
            std::env::temp_dir().join(format!("tern-sync-relink-b-{}", std::process::id())),
        );
        for dir in [&ours, &theirs] {
            let _ = std::fs::remove_dir_all(dir);
        }
        let (here, there) = ([(ours.clone(), true)], [(theirs.clone(), true)]);
        // There: a host signs in with a key there, which syncs.
        let key = keys::generate(&there, "deploy", None, "").unwrap();
        let mut vault = Vault::default();
        let mut db = Host::new("db", "ops", "db.lan", 22);
        db.auth = Auth::Key(key.path.clone());
        vault.save(None, db, "Prod");
        let pairs = keys::list_in(&there);
        let chosen = std::slice::from_ref(&key.fingerprint);
        let mut from_there = snapshot(&vault, &settings("There"), keys_to_sync(&pairs, chosen));
        from_there.key_files = key_files(&vault, &pairs);
        assert_eq!(
            from_there.key_files,
            [KeyFile {
                path: key.path.clone(),
                fingerprint: key.fingerprint.clone(),
            }]
        );

        // Here, the key goes where this device keeps keys, and the path
        // there isn't one here: the host finds the key here instead.
        std::fs::remove_dir_all(&theirs).unwrap();
        let merged = merge(&Vault::default(), &settings("Here"), &[from_there.clone()]);
        receive_keys(&here, &merged.keys, &[]).unwrap();
        let pairs = keys::list_in(&here);
        let moves = relink(&merged.vault, &[from_there.clone()], &pairs);
        assert_eq!(moves, [(key.path.clone(), pairs[0].path.clone())]);
        let mut vault = merged.vault;
        let updated = vault.find("db").unwrap().updated;
        assert!(vault.relink_keys(&moves));
        let db = vault.find("db").unwrap();
        assert_eq!(db.auth, Auth::Key(pairs[0].path.clone()));
        // The key is the same, so nothing changed for other devices.
        assert_eq!(db.updated, updated);
        // A key file that's here is left as it is.
        assert!(relink(&vault, &[from_there], &pairs).is_empty());
        std::fs::remove_dir_all(ours).unwrap();
    }

    #[test]
    fn a_synced_key_must_be_the_key_it_names() {
        let (ours, theirs) = (
            std::env::temp_dir().join(format!("tern-sync-forged-a-{}", std::process::id())),
            std::env::temp_dir().join(format!("tern-sync-forged-b-{}", std::process::id())),
        );
        for dir in [&ours, &theirs] {
            let _ = std::fs::remove_dir_all(dir);
        }
        let (here, there) = ([(ours.clone(), true)], [(theirs.clone(), true)]);
        let chosen = keys::generate(&here, "deploy", None, "").unwrap();
        let good = keys_to_sync(&keys::list_in(&here), &[chosen.fingerprint])[0].clone();
        // Ssh's settings, sent as that key; and the key under a fingerprint
        // that isn't its own.
        let forged = SyncedKey {
            name: "config".into(),
            private: "Host *\n  ProxyCommand sh\n".into(),
            ..good.clone()
        };
        let lying = SyncedKey {
            fingerprint: "SHA256:another".into(),
            ..good.clone()
        };
        let carrying = |keys: Vec<SyncedKey>| {
            let mut snapshot = snapshot(&Vault::default(), &settings("Laptop"), keys);
            snapshot.device_id = new_id();
            snapshot
        };
        // The forged one can't hide the real one by coming first.
        let merged = merge(
            &Vault::default(),
            &settings("Desk"),
            &[carrying(vec![forged]), carrying(vec![good, lying])],
        );
        assert_eq!(merged.keys.len(), 3);
        assert_eq!(
            receive_keys(&there, &merged.keys, &[]).unwrap_err(),
            "A synced key was refused: it isn't a private key"
        );
        let arrived = keys::list_in(&there);
        assert_eq!(arrived.len(), 1);
        assert_eq!(arrived[0].name, "deploy");
        assert!(!theirs.join("config").exists());
        for dir in [ours, theirs] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
