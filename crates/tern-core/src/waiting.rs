//! Commands waiting for a host that's down: run as soon as a background
//! check sees it answer again (after a reboot, say). They're kept on this
//! computer, in `waiting.json`, and never sync.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// A command waiting for a host, by the host's id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wait {
    pub host: String,
    pub command: String,
}

/// The commands waiting, as kept in `waiting.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waiting {
    pub waiting: Vec<Wait>,
}

impl Waiting {
    /// The commands in `path`; none if it's missing or unreadable.
    pub fn load(path: &Path) -> Self {
        crate::paths::settle(path);
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        crate::paths::write_atomically(path, format!("{json}\n").as_bytes())
    }

    /// The command waiting for the host `host`, if one is.
    pub fn command(&self, host: &str) -> Option<&str> {
        self.waiting
            .iter()
            .find(|wait| wait.host == host)
            .map(|wait| wait.command.as_str())
    }

    /// Has `command` wait for `host`, instead of what waited; an empty one
    /// stops waiting.
    pub fn set(&mut self, host: &str, command: &str) {
        self.waiting.retain(|wait| wait.host != host);
        let command = command.trim();
        if !command.is_empty() {
            self.waiting.push(Wait {
                host: host.into(),
                command: command.into(),
            });
        }
    }

    /// Takes the command waiting for `host`, which answered.
    pub fn take(&mut self, host: &str) -> Option<String> {
        let at = self.waiting.iter().position(|wait| wait.host == host)?;
        Some(self.waiting.remove(at).command)
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_waits_once_per_host() {
        let mut waiting = Waiting::default();
        assert!(waiting.is_empty());
        waiting.set("db", " uptime ");
        waiting.set("web", "systemctl --failed");
        waiting.set("db", "uptime; df -h");
        assert_eq!(waiting.command("db"), Some("uptime; df -h"));
        assert_eq!(waiting.take("db").as_deref(), Some("uptime; df -h"));
        assert_eq!(waiting.take("db"), None);
        waiting.set("web", "  ");
        assert!(waiting.is_empty());

        waiting.set("db", "uptime");
        let path = std::env::temp_dir().join(format!("tern-waiting-{}.json", std::process::id()));
        waiting.save(&path).unwrap();
        assert_eq!(Waiting::load(&path), waiting);
        let _ = std::fs::remove_file(&path);
        assert!(Waiting::load(&path).is_empty());
    }
}
