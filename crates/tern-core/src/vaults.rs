//! More than one vault, one open at a time. Each has its own hosts,
//! snippets, identities and rules, its own recent connections, and syncs
//! through a folder of its own with a passphrase of its own. The first is
//! the vault Tern always had: its files are in the data folder itself and
//! its sync is kept with the settings. Others keep their files in
//! `vaults/<id>/` there. The list is `vaults.json`, which never syncs.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::host::new_id;
use crate::sync::SyncSettings;

/// The first vault's id.
pub const FIRST: &str = "personal";

/// One vault in the list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultEntry {
    pub id: String,
    pub name: String,
    /// How it syncs; the first vault's is kept with the settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncSettings>,
}

/// The vaults, as kept in `vaults.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vaults {
    pub vaults: Vec<VaultEntry>,
    /// The id of the vault open.
    pub current: String,
}

impl Default for Vaults {
    fn default() -> Self {
        Self {
            vaults: vec![first()],
            current: FIRST.into(),
        }
    }
}

fn first() -> VaultEntry {
    VaultEntry {
        id: FIRST.into(),
        name: "Personal".into(),
        sync: None,
    }
}

/// Ids name folders: only what `new_id` makes, or the first's.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl Vaults {
    /// The list in `path`, always with the first vault in it and with the
    /// open one among them; just the first when there's no list yet.
    pub fn load(path: &Path) -> Self {
        let mut vaults: Self = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        vaults.vaults.retain(|vault| valid_id(&vault.id));
        if !vaults.vaults.iter().any(|vault| vault.id == FIRST) {
            vaults.vaults.insert(0, first());
        }
        if !vaults.vaults.iter().any(|vault| vault.id == vaults.current) {
            vaults.current = FIRST.into();
        }
        vaults
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        crate::paths::write_atomically(path, format!("{json}\n").as_bytes())
    }

    /// The vault open.
    pub fn current(&self) -> &VaultEntry {
        self.find(&self.current).unwrap_or(&self.vaults[0])
    }

    pub fn find(&self, id: &str) -> Option<&VaultEntry> {
        self.vaults.iter().find(|vault| vault.id == id)
    }

    /// Adds a vault called `name` (or "Vault"), made unlike the others'
    /// names; its id.
    pub fn add(&mut self, name: &str) -> String {
        let id = new_id();
        let name = self.free_name(name.trim(), None);
        self.vaults.push(VaultEntry {
            id: id.clone(),
            name,
            sync: None,
        });
        id
    }

    /// Renames the vault `id`, made unlike the others' names; a blank
    /// name changes nothing.
    pub fn rename(&mut self, id: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let name = self.free_name(name, Some(id));
        if let Some(vault) = self.vaults.iter_mut().find(|vault| vault.id == id) {
            vault.name = name;
        }
    }

    /// Takes the vault `id` off the list, never the first; the first opens
    /// if it was the one open. Whether it went.
    pub fn remove(&mut self, id: &str) -> bool {
        if id == FIRST {
            return false;
        }
        let before = self.vaults.len();
        self.vaults.retain(|vault| vault.id != id);
        if self.current == id {
            self.current = FIRST.into();
        }
        self.vaults.len() < before
    }

    /// The vault, other than the open one, whose file has the host
    /// `host_id`.
    pub fn holding(&self, host_id: &str) -> Option<String> {
        self.vaults
            .iter()
            .filter(|vault| vault.id != self.current)
            .find(|vault| {
                crate::vault::read(&crate::paths::vault(&vault.id))
                    .is_some_and(|read| read.hosts().any(|host| host.id == host_id))
            })
            .map(|vault| vault.id.clone())
    }

    /// `name`, or "name 2", "name 3"… as no vault but `except` has it.
    fn free_name(&self, name: &str, except: Option<&str>) -> String {
        let name = if name.is_empty() { "Vault" } else { name };
        let taken = |candidate: &str| {
            self.vaults
                .iter()
                .any(|vault| Some(vault.id.as_str()) != except && vault.name == candidate)
        };
        if !taken(name) {
            return name.into();
        }
        (2..)
            .map(|n| format!("{name} {n}"))
            .find(|candidate| !taken(candidate))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_vault_is_always_there() {
        let path = std::env::temp_dir().join(format!("tern-vaults-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let vaults = Vaults::load(&path);
        assert_eq!(vaults, Vaults::default());
        assert_eq!(vaults.current().name, "Personal");

        // A list without it, open on a vault that's gone, with an id that
        // would leave the data folder.
        std::fs::write(
            &path,
            r#"{"vaults":[{"id":"0123abcd","name":"Work"},{"id":"../../x","name":"Bad"}],"current":"gone"}"#,
        )
        .unwrap();
        let vaults = Vaults::load(&path);
        let ids: Vec<&str> = vaults
            .vaults
            .iter()
            .map(|vault| vault.id.as_str())
            .collect();
        assert_eq!(ids, [FIRST, "0123abcd"]);
        assert_eq!(vaults.current, FIRST);
        vaults.save(&path).unwrap();
        assert_eq!(Vaults::load(&path), vaults);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn adds_renames_and_removes_vaults() {
        let mut vaults = Vaults::default();
        let work = vaults.add(" Work ");
        let again = vaults.add("Work");
        assert_eq!(vaults.find(&work).unwrap().name, "Work");
        assert_eq!(vaults.find(&again).unwrap().name, "Work 2");
        assert!(valid_id(&work));

        vaults.rename(&again, "Personal");
        assert_eq!(vaults.find(&again).unwrap().name, "Personal 2");
        vaults.rename(&again, "  ");
        assert_eq!(vaults.find(&again).unwrap().name, "Personal 2");
        vaults.rename(&work, "Work");
        assert_eq!(vaults.find(&work).unwrap().name, "Work");

        vaults.current = work.clone();
        assert!(!vaults.remove(FIRST));
        assert!(vaults.remove(&work));
        assert_eq!(vaults.current, FIRST);
        assert!(!vaults.remove(&work));
        assert_eq!(vaults.vaults.len(), 2);
    }
}
