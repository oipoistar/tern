//! Recent connections, newest first, for the History screen: which host,
//! when, how long the shell was up and how it ended. Kept in
//! `history.json` in the data folder, capped at [`KEEP`]. Like the vault
//! file it holds no secrets.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::host::Auth;
use crate::paths;
use crate::ssh::{Connection, Target};
use crate::vault::Vault;

/// Visits kept; older ones drop off the end.
pub const KEEP: usize = 200;

/// One connection attempt, from starting to dial until it ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Visit {
    /// The saved host it was opened as, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub user: String,
    pub address: String,
    pub port: u16,
    #[serde(default)]
    pub auth: Auth,
    /// When it started, in seconds since the Unix epoch.
    pub started: u64,
    /// How long the shell was up, in seconds; `None` if it never was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lasted: Option<u64>,
    /// How it ended, in one line.
    pub ended: String,
}

impl Visit {
    /// The saved host's name, or `user@address`, with the port unless 22.
    pub fn label(&self) -> String {
        self.host.clone().unwrap_or_else(|| self.target())
    }

    /// `user@address`, with the port unless 22.
    pub fn target(&self) -> String {
        if self.port == 22 {
            format!("{}@{}", self.user, self.address)
        } else {
            format!("{}@{}:{}", self.user, self.address, self.port)
        }
    }

    /// How to go back: the saved host as it is now, if it still exists and
    /// resolves, otherwise what was dialled then.
    pub fn connection(&self, vault: &Vault) -> Connection {
        self.host
            .as_deref()
            .and_then(|name| vault.find(name))
            .and_then(|host| vault.connection(host).ok())
            .unwrap_or_else(|| Connection {
                auth: self.auth.clone(),
                ..Connection::typed(
                    Target {
                        host: self.address.clone(),
                        port: self.port,
                        user: self.user.clone(),
                    },
                    None,
                )
            })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    /// Newest first.
    pub visits: Vec<Visit>,
}

impl History {
    /// The history in `path`. A missing or unreadable file is an empty
    /// history: unlike the vault, losing it costs little.
    pub fn load(path: &Path) -> Self {
        paths::settle(path);
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        paths::write_atomically(path, json.as_bytes())
    }

    /// Adds `visit` as the newest, dropping the oldest past [`KEEP`].
    pub fn record(&mut self, visit: Visit) {
        self.visits.insert(0, visit);
        self.visits.truncate(KEEP);
    }
}

/// Seconds since the Unix epoch, now.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// How long ago `then` was at `now`, roughly: `just now`, `5 min ago`,
/// `3 h ago`, `2 days ago`.
pub fn ago(then: u64, now: u64) -> String {
    let seconds = now.saturating_sub(then);
    match seconds {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..86_400 => format!("{} h ago", seconds / 3600),
        86_400..172_800 => "yesterday".into(),
        _ => format!("{} days ago", seconds / 86_400),
    }
}

/// A duration in its two largest units: `45 s`, `12 min`, `1 h 5 min`.
pub fn span(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} min", seconds / 60),
        _ if seconds % 3600 < 60 => format!("{} h", seconds / 3600),
        _ => format!("{} h {} min", seconds / 3600, seconds % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visit(host: Option<&str>, address: &str) -> Visit {
        Visit {
            host: host.map(Into::into),
            user: "deploy".into(),
            address: address.into(),
            port: 22,
            auth: Auth::Password,
            started: 1_700_000_000,
            lasted: Some(90),
            ended: "Exited with status 0".into(),
        }
    }

    #[test]
    fn keeps_the_newest_and_survives_a_restart() {
        let mut history = History::default();
        for n in 0..KEEP + 3 {
            history.record(visit(None, &format!("10.0.0.{n}")));
        }
        assert_eq!(history.visits.len(), KEEP);
        assert_eq!(history.visits[0].address, format!("10.0.0.{}", KEEP + 2));

        let dir = std::env::temp_dir().join(format!("tern-history-{}", std::process::id()));
        let path = dir.join("history.json");
        history.save(&path).unwrap();
        assert_eq!(History::load(&path), history);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(History::load(&path), History::default());
        assert_eq!(History::load(&dir.join("missing")), History::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reconnects_as_the_host_is_now() {
        let vault = Vault::sample();
        // Still saved: its current settings, jump host and all.
        let saved = visit(Some("bastion"), "old-address");
        let connection = saved.connection(&vault);
        assert_eq!(connection.target.host, "bastion.example.com");
        assert_eq!(connection.auth, Auth::Agent);
        // Gone, or typed: what was dialled.
        for gone in [visit(Some("deleted"), "10.0.0.9"), visit(None, "10.0.0.9")] {
            let connection = gone.connection(&vault);
            assert_eq!(
                (connection.target.host.as_str(), connection.auth),
                ("10.0.0.9", Auth::Password)
            );
        }
        assert_eq!(saved.label(), "bastion");
        assert_eq!(visit(None, "10.0.0.9").label(), "deploy@10.0.0.9");
    }

    #[test]
    fn says_when_and_how_long_in_a_few_words() {
        let now = 1_700_000_000;
        assert_eq!(ago(now - 5, now), "just now");
        assert_eq!(ago(now - 125, now), "2 min ago");
        assert_eq!(ago(now - 3 * 3600, now), "3 h ago");
        assert_eq!(ago(now - 100_000, now), "yesterday");
        assert_eq!(ago(now - 5 * 86_400, now), "5 days ago");
        assert_eq!(ago(now + 10, now), "just now");
        assert_eq!(span(45), "45 s");
        assert_eq!(span(12 * 60 + 5), "12 min");
        assert_eq!(span(3600 + 5 * 60), "1 h 5 min");
        assert_eq!(span(7200 + 30), "2 h");
    }
}
