use std::path::PathBuf;

/// Reachability of a saved host, as last observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostStatus {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    pub name: String,
    pub user: String,
    pub address: String,
    pub port: u16,
    pub os: String,
    pub tags: Vec<String>,
    pub status: HostStatus,
    /// Private key to log in with, before the agent and default keys.
    pub identity: Option<PathBuf>,
}

impl Host {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostGroup {
    pub name: String,
    pub hosts: Vec<Host>,
}

/// Placeholder vault contents until the encrypted vault exists.
pub fn sample_groups() -> Vec<HostGroup> {
    let host =
        |name: &str, user: &str, address: &str, port, os: &str, tags: &[&str], status| Host {
            name: name.into(),
            user: user.into(),
            address: address.into(),
            port,
            os: os.into(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            status,
            identity: None,
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

    fn host(name: &str, port: u16) -> Host {
        Host {
            name: name.into(),
            user: "deploy".into(),
            address: "10.0.0.1".into(),
            port,
            os: String::new(),
            tags: vec![],
            status: HostStatus::Offline,
            identity: None,
        }
    }

    #[test]
    fn target_hides_default_port() {
        assert_eq!(host("a", 22).target(), "deploy@10.0.0.1");
        assert_eq!(host("a", 2222).target(), "deploy@10.0.0.1:2222");
    }

    #[test]
    fn matches_every_term_anywhere() {
        let mut db = host("prod-db-01", 22);
        db.tags = vec!["postgres".into(), "eu-west".into()];
        assert!(db.matches(""));
        assert!(db.matches("DB"));
        assert!(db.matches("deploy@10.0"));
        assert!(db.matches("postgres eu"));
        assert!(!db.matches("postgres us"));
    }
}
