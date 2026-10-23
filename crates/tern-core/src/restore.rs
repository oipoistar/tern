//! The tabs open when Tern last quit, in `tabs.json` in the data folder,
//! so the next start can open them again. A saved host is kept by its id
//! and name, a typed connection by what was typed, and never a password.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::host::Auth;
use crate::paths;
use crate::ssh::{Connection, Target};
use crate::vault::Vault;

/// What a pane showed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Opened {
    /// A saved host.
    Host { id: Option<String>, name: String },
    /// A connection typed in quick connect, with the key file it named.
    Typed {
        user: String,
        host: String,
        port: u16,
        key: Option<PathBuf>,
    },
    /// A shell on this computer.
    Local,
}

impl Opened {
    /// What opens `connection` again.
    pub fn of(connection: &Connection) -> Self {
        if connection.local {
            return Self::Local;
        }
        if let Some(name) = &connection.host {
            return Self::Host {
                id: connection.host_id.clone(),
                name: name.clone(),
            };
        }
        let target = &connection.target;
        Self::Typed {
            user: target.user.clone(),
            host: target.host.clone(),
            port: target.port,
            key: match &connection.auth {
                Auth::Key(path) => Some(path.clone()),
                _ => None,
            },
        }
    }

    /// The connection that opens it again; `None` for a saved host that's
    /// gone, or can't be connected to as it's saved now.
    pub fn connection(&self, vault: &Vault) -> Option<Connection> {
        match self {
            Self::Local => Some(Connection::local()),
            Self::Host { id, name } => {
                // The id survives a rename synced in from elsewhere.
                let host = id
                    .as_ref()
                    .and_then(|id| vault.hosts().find(|host| &host.id == id))
                    .or_else(|| vault.find(name))?;
                vault.connection(host).ok()
            }
            Self::Typed {
                user,
                host,
                port,
                key,
            } => Some(Connection::typed(
                Target {
                    host: host.clone(),
                    port: *port,
                    user: user.clone(),
                },
                key.clone(),
            )),
        }
    }
}

/// The open tabs, each a list of its panes, and the one on screen.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenTabs {
    pub tabs: Vec<Vec<Opened>>,
    /// `None` is the vault.
    pub active: Option<usize>,
}

impl OpenTabs {
    /// The tabs saved at `path`; none when it's missing or unreadable.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(text.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        paths::write_atomically(path, json.as_bytes())
    }

    /// Each tab's connections, without panes whose hosts are gone or tabs
    /// left with none, and where the tab on screen ends up.
    pub fn connections(&self, vault: &Vault) -> (Vec<Vec<Connection>>, Option<usize>) {
        let mut tabs = Vec::new();
        let mut active = None;
        for (index, panes) in self.tabs.iter().enumerate() {
            let connections: Vec<Connection> = panes
                .iter()
                .filter_map(|pane| pane.connection(vault))
                .collect();
            if connections.is_empty() {
                continue;
            }
            if self.active == Some(index) {
                active = Some(tabs.len());
            }
            tabs.push(connections);
        }
        (tabs, active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Host, HostGroup};

    fn vault() -> Vault {
        Vault {
            groups: vec![HostGroup {
                name: "Hosts".into(),
                hosts: vec![Host::new("db", "deploy", "10.0.0.5", 22)],
            }],
            ..Vault::default()
        }
    }

    fn typed() -> Connection {
        let target = Target {
            host: "example.com".into(),
            port: 2222,
            user: "me".into(),
        };
        Connection::typed(target, Some(PathBuf::from("/keys/id_ed25519")))
    }

    #[test]
    fn opens_what_each_pane_showed_again() {
        let vault = vault();
        let host = vault.find("db").unwrap();
        let db = vault.connection(host).unwrap();
        assert_eq!(
            Opened::of(&db),
            Opened::Host {
                id: Some(host.id.clone()),
                name: "db".into()
            }
        );
        assert_eq!(Opened::of(&Connection::local()), Opened::Local);
        for connection in [db, typed(), Connection::local()] {
            assert_eq!(Opened::of(&connection).connection(&vault), Some(connection));
        }
    }

    #[test]
    fn follows_renamed_hosts_and_leaves_out_deleted_ones() {
        let mut vault = vault();
        let db = vault.connection(vault.find("db").unwrap()).unwrap();
        let gone = Opened::Host {
            id: Some("gone".into()),
            name: "gone".into(),
        };
        let open = OpenTabs {
            tabs: vec![
                vec![Opened::of(&db)],
                vec![gone.clone()],
                vec![Opened::Local, gone],
            ],
            active: Some(2),
        };
        vault.groups[0].hosts[0].name = "database".into();
        let (tabs, active) = open.connections(&vault);
        let hosts: Vec<Vec<Option<&str>>> = tabs
            .iter()
            .map(|panes| panes.iter().map(|pane| pane.host.as_deref()).collect())
            .collect();
        assert_eq!(hosts, [vec![Some("database")], vec![None]]);
        assert!(tabs[1][0].local);
        assert_eq!(active, Some(1));

        // The tab on screen gone, the vault shows.
        let open = OpenTabs {
            active: Some(1),
            ..open
        };
        assert_eq!(open.connections(&vault).1, None);
    }

    #[test]
    fn saves_and_loads() {
        let dir = std::env::temp_dir().join(format!("tern-tabs-{}", std::process::id()));
        let path = dir.join("tabs.json");
        assert_eq!(OpenTabs::load(&path), OpenTabs::default());

        let open = OpenTabs {
            tabs: vec![vec![Opened::Local, Opened::of(&typed())], vec![]],
            active: Some(0),
        };
        open.save(&path).unwrap();
        assert_eq!(OpenTabs::load(&path), open);

        std::fs::write(&path, "not json").unwrap();
        assert_eq!(OpenTabs::load(&path), OpenTabs::default());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
