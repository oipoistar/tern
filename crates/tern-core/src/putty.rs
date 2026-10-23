//! PuTTY's saved sessions, for importing them as hosts. PuTTY keeps them in
//! the registry on Windows and in `~/.putty/sessions` elsewhere; Tern only
//! reads them.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::host::{Auth, Host};
use crate::vault::Vault;

/// The group imported hosts land in.
pub const GROUP: &str = "PuTTY";

/// One saved session's settings, under PuTTY's names (`HostName`,
/// `PortNumber`, …).
pub type Settings = HashMap<String, String>;

/// Saved PuTTY sessions on this computer that aren't in the vault yet, as
/// hosts for the [`GROUP`].
pub fn importable_here(vault: &Vault) -> Vec<Host> {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    importable(vault, &saved_sessions(), &user)
}

/// The sessions' hosts, minus those the vault already has (same user,
/// address and port). A name another host already uses becomes a free
/// one: "name (PuTTY)", then "name (PuTTY) 2" and so on.
pub fn importable(vault: &Vault, sessions: &[(String, Settings)], user: &str) -> Vec<Host> {
    let hosts = sessions
        .iter()
        .filter_map(|(name, settings)| to_host(name, settings, user))
        .collect();
    crate::openssh::new_hosts(vault, hosts, GROUP)
}

/// The host a session describes, or `None` for sessions Tern can't open
/// (telnet, serial, raw) or that name no address, like PuTTY's "Default
/// Settings". PuTTY asks for a login name when none is saved; Tern needs one
/// up front, so it takes `user` (yours), as `ssh` does.
pub fn to_host(name: &str, settings: &Settings, user: &str) -> Option<Host> {
    let setting = |key: &str| {
        settings
            .get(key)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
    };
    if setting("Protocol").unwrap_or("ssh") != "ssh" {
        return None;
    }
    let (login, address) = match setting("HostName")?.rsplit_once('@') {
        Some((login, address)) => (Some(login), address),
        None => (None, setting("HostName")?),
    };
    let user = setting("UserName")
        .or(login)
        .or(Some(user.trim()).filter(|user| !user.is_empty()))?;
    let port = setting("PortNumber")
        .and_then(|port| port.parse::<u16>().ok())
        .filter(|&port| port > 0)
        .unwrap_or(22);
    let auth = match setting("PublicKeyFile") {
        // PuTTY's own .ppk keys can't be read here, but Pageant serves them
        // and Tern signs in through it as the agent.
        Some(path) if !path.to_lowercase().ends_with(".ppk") => Auth::Key(PathBuf::from(path)),
        _ => Auth::Agent,
    };
    (!address.is_empty()).then(|| Host {
        auth,
        ..Host::new(&decode_name(name), user, address, port)
    })
}

/// PuTTY escapes session names like URLs: `My%20Server`.
pub fn decode_name(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A session file from `~/.putty/sessions`: `Key=Value` lines.
pub fn parse_session_file(text: &str) -> Settings {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// Every saved PuTTY session on this computer, by its raw name.
pub fn saved_sessions() -> Vec<(String, Settings)> {
    #[cfg(windows)]
    {
        read_registry()
    }
    #[cfg(not(windows))]
    {
        read_session_files()
    }
}

#[cfg(windows)]
fn read_registry() -> Vec<(String, Settings)> {
    let Ok(sessions) = windows_registry::CURRENT_USER.open(r"Software\SimonTatham\PuTTY\Sessions")
    else {
        return Vec::new();
    };
    let Ok(names) = sessions.keys() else {
        return Vec::new();
    };
    names
        .filter_map(|name| {
            let session = sessions.open(&name).ok()?;
            let mut settings = Settings::new();
            for key in ["HostName", "UserName", "Protocol", "PublicKeyFile"] {
                if let Ok(value) = session.get_string(key) {
                    settings.insert(key.into(), value);
                }
            }
            if let Ok(port) = session.get_u32("PortNumber") {
                settings.insert("PortNumber".into(), port.to_string());
            }
            Some((name, settings))
        })
        .collect()
}

#[cfg(not(windows))]
fn read_session_files() -> Vec<(String, Settings)> {
    let Some(dir) = std::env::home_dir().map(|home| home.join(".putty").join("sessions")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let text = std::fs::read_to_string(entry.path()).ok()?;
            Some((
                entry.file_name().to_string_lossy().into_owned(),
                parse_session_file(&text),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str, pairs: &[(&str, &str)]) -> (String, Settings) {
        let settings = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        (name.to_string(), settings)
    }

    #[test]
    fn decodes_escaped_names() {
        assert_eq!(decode_name("My%20Server%2Ebox"), "My Server.box");
        assert_eq!(decode_name("100%"), "100%");
        assert_eq!(decode_name("plain"), "plain");
    }

    #[test]
    fn reads_session_files() {
        let settings = parse_session_file("HostName=db.lan\nPortNumber=2200\nWeird line\n");
        assert_eq!(settings["HostName"], "db.lan");
        assert_eq!(settings["PortNumber"], "2200");
        assert_eq!(settings.len(), 2);
    }

    #[test]
    fn turns_ssh_sessions_into_hosts() {
        let (name, settings) = session(
            "My%20NAS",
            &[
                ("HostName", "admin@nas.lan"),
                ("PortNumber", "2222"),
                ("Protocol", "ssh"),
                ("PublicKeyFile", r"C:\keys\nas.ppk"),
            ],
        );
        let host = to_host(&name, &settings, "me").unwrap();
        assert_eq!(host.name, "My NAS");
        assert_eq!(host.target(), "admin@nas.lan:2222");
        assert_eq!(host.auth, Auth::Agent);

        // A saved user name wins over the one in HostName; an OpenSSH key is
        // used directly; with no user at all, yours is taken.
        let (_, settings) = session(
            "pi",
            &[
                ("HostName", "x@192.168.1.40"),
                ("UserName", "pi"),
                ("PublicKeyFile", "/home/me/.ssh/id_ed25519"),
            ],
        );
        let host = to_host("pi", &settings, "me").unwrap();
        assert_eq!(host.target(), "pi@192.168.1.40");
        assert_eq!(
            host.auth,
            Auth::Key(PathBuf::from("/home/me/.ssh/id_ed25519"))
        );
        let (_, settings) = session("box", &[("HostName", "box.lan")]);
        assert_eq!(to_host("box", &settings, "me").unwrap().user, "me");
        assert!(to_host("box", &settings, "").is_none());
    }

    #[test]
    fn skips_what_tern_cant_open() {
        let telnet = session(
            "router",
            &[("HostName", "10.0.0.1"), ("Protocol", "telnet")],
        );
        let defaults = session("Default%20Settings", &[("HostName", "")]);
        assert!(to_host(&telnet.0, &telnet.1, "me").is_none());
        assert!(to_host(&defaults.0, &defaults.1, "me").is_none());
    }

    #[test]
    fn imports_only_what_the_vault_lacks() {
        let vault = Vault::sample();
        let sessions = [
            // Already saved as prod-db-01.
            session("db", &[("HostName", "deploy@10.0.4.21")]),
            // A new place under a name the vault already uses.
            session("bastion", &[("HostName", "ops@gw.example.com")]),
            session("nas", &[("HostName", "admin@nas.lan")]),
            // The same place twice in PuTTY.
            session("nas-again", &[("HostName", "admin@nas.lan")]),
        ];
        let hosts = importable(&vault, &sessions, "me");
        let names: Vec<&str> = hosts.iter().map(|host| host.name.as_str()).collect();
        assert_eq!(names, ["bastion (PuTTY)", "nas"]);
    }

    #[test]
    fn picks_a_free_name_for_each_clash() {
        let mut vault = Vault::sample();
        vault.save(
            None,
            Host::new("bastion (PuTTY)", "x", "old.lan", 22),
            GROUP,
        );
        let sessions = [
            session("Bastion", &[("HostName", "ops@gw.example.com")]),
            // Two sessions whose names read the same once decoded.
            session("My%20NAS", &[("HostName", "admin@nas.lan")]),
            session("My NAS", &[("HostName", "admin@nas2.lan")]),
        ];
        let hosts = importable(&vault, &sessions, "me");
        let names: Vec<&str> = hosts.iter().map(|host| host.name.as_str()).collect();
        assert_eq!(names, ["Bastion (PuTTY) 2", "My NAS", "My NAS (PuTTY)"]);
    }
}
