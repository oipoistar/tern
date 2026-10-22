use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Reachability of a saved host, as last observed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HostStatus {
    /// Never checked, like a host that was just added.
    #[default]
    Unknown,
    /// Answering; with the round trip in milliseconds when it was measured.
    Online {
        latency_ms: Option<u32>,
    },
    Connecting,
    Offline,
}

impl HostStatus {
    pub const fn online(latency_ms: u32) -> Self {
        Self::Online {
            latency_ms: Some(latency_ms),
        }
    }
}

/// How Tern signs in to a host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Auth {
    /// The SSH agent, then the usual key files in `~/.ssh`.
    #[default]
    Agent,
    /// This key file first, then the agent and the usual keys.
    Key(PathBuf),
    /// Ask for a password when connecting.
    Password,
}

/// A saved host. Everything but `status` is kept in the vault file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    pub name: String,
    pub user: String,
    pub address: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Observed while Tern runs, never saved.
    #[serde(skip)]
    pub status: HostStatus,
    #[serde(default)]
    pub auth: Auth,
    /// Another saved host to connect through first, by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<String>,
    /// Ping the server every 30 seconds so idle connections stay up.
    #[serde(default = "on")]
    pub keep_alive: bool,
    /// Typed into the shell once it starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_login: Option<String>,
}

fn on() -> bool {
    true
}

impl Host {
    /// A host with Tern's defaults for everything but where it is.
    pub fn new(name: &str, user: &str, address: &str, port: u16) -> Self {
        Self {
            name: name.into(),
            user: user.into(),
            address: address.into(),
            port,
            os: String::new(),
            tags: Vec::new(),
            status: HostStatus::Unknown,
            auth: Auth::Agent,
            jump: None,
            keep_alive: true,
            on_login: None,
        }
    }

    /// Where to connect for an SSH session.
    pub fn ssh_target(&self) -> crate::ssh::Target {
        crate::ssh::Target {
            host: self.address.clone(),
            port: self.port,
            user: self.user.clone(),
        }
    }

    /// `user@address`, with the port appended only when it isn't 22.
    pub fn target(&self) -> String {
        if self.port == 22 {
            format!("{}@{}", self.user, self.address)
        } else {
            format!("{}@{}:{}", self.user, self.address, self.port)
        }
    }

    /// Whether a search query matches this host. Each whitespace-separated
    /// term must appear (case-insensitively) in the name, `user@address` or
    /// a tag; an empty query matches everything.
    pub fn matches(&self, query: &str) -> bool {
        let haystack = format!(
            "{} {}@{} {}",
            self.name,
            self.user,
            self.address,
            self.tags.join(" ")
        )
        .to_lowercase();
        query
            .split_whitespace()
            .all(|term| haystack.contains(&term.to_lowercase()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostGroup {
    pub name: String,
    pub hosts: Vec<Host>,
}

/// Sample hosts with observed statuses, for tests and screenshots. A real
/// vault starts empty.
#[cfg(any(test, feature = "test-support"))]
pub fn sample_groups() -> Vec<HostGroup> {
    let host = |name: &str, user, address, port, os: &str, tags: &[&str], status| Host {
        os: os.into(),
        tags: tags.iter().map(|t| t.to_string()).collect(),
        status,
        ..Host::new(name, user, address, port)
    };
    vec![
        HostGroup {
            name: "Production".into(),
            hosts: vec![
                host(
                    "prod-db-01",
                    "deploy",
                    "10.0.4.21",
                    22,
                    "Ubuntu 24.04",
                    &["postgres", "eu-west"],
                    HostStatus::online(14),
                ),
                host(
                    "prod-web-01",
                    "deploy",
                    "web-01.internal",
                    22,
                    "Debian 12",
                    &["nginx", "eu-west"],
                    HostStatus::online(9),
                ),
                host(
                    "prod-web-02",
                    "deploy",
                    "web-02.internal",
                    22,
                    "Debian 12",
                    &["nginx", "eu-west"],
                    HostStatus::Connecting,
                ),
                host(
                    "bastion",
                    "admin",
                    "bastion.example.com",
                    2222,
                    "Alpine 3.20",
                    &["jump host"],
                    HostStatus::online(38),
                ),
            ],
        },
        HostGroup {
            name: "Staging & lab".into(),
            hosts: vec![
                host(
                    "stage-api",
                    "ubuntu",
                    "stage-api.local",
                    22,
                    "Ubuntu 22.04",
                    &["docker"],
                    HostStatus::online(3),
                ),
                host(
                    "stage-worker",
                    "ubuntu",
                    "stage-worker.local",
                    22,
                    "Ubuntu 22.04",
                    &["docker"],
                    HostStatus::Offline,
                ),
                host(
                    "pi-homelab",
                    "pi",
                    "192.168.1.40",
                    22,
                    "Raspberry Pi OS",
                    &["raspberry pi"],
                    HostStatus::online(1),
                ),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_hides_default_port() {
        assert_eq!(
            Host::new("a", "deploy", "10.0.0.1", 22).target(),
            "deploy@10.0.0.1"
        );
        assert_eq!(
            Host::new("a", "deploy", "10.0.0.1", 2222).target(),
            "deploy@10.0.0.1:2222"
        );
    }

    #[test]
    fn matches_every_term_anywhere() {
        let mut db = Host::new("prod-db-01", "deploy", "10.0.0.1", 22);
        db.tags = vec!["postgres".into(), "eu-west".into()];
        assert!(db.matches(""));
        assert!(db.matches("DB"));
        assert!(db.matches("deploy@10.0"));
        assert!(db.matches("postgres eu"));
        assert!(!db.matches("postgres us"));
    }
}
