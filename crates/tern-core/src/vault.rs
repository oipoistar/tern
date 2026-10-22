//! The saved hosts, in groups. Kept in memory for now; the encrypted vault
//! file comes later.

use std::collections::HashMap;

use crate::host::{Host, HostGroup, sample_groups};
use crate::ssh::{Connection, Jump};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Vault {
    pub groups: Vec<HostGroup>,
}

impl Vault {
    pub fn sample() -> Self {
        Self {
            groups: sample_groups(),
        }
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
    /// the old name follow a rename.
    pub fn save(&mut self, replacing: Option<&str>, host: Host, group: &str) {
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
    }

    /// Removes a host. Refused while other hosts go through it; the error
    /// names them.
    pub fn delete(&mut self, name: &str) -> Result<(), Vec<String>> {
        let dependents = self.dependents(name);
        if !dependents.is_empty() {
            return Err(dependents.into_iter().map(String::from).collect());
        }
        for group in &mut self.groups {
            group.hosts.retain(|host| host.name != name);
        }
        Ok(())
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
            target: host.ssh_target(),
            auth: host.auth.clone(),
            jump,
            keep_alive: host.keep_alive,
            on_login: host.on_login.clone(),
        })
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
}
