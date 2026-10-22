//! Snippets: saved commands, typed into a session from its snippet bar.
//! They live in the vault file beside the hosts.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snippet {
    pub name: String,
    pub command: String,
}

impl Snippet {
    /// What typing it into a shell sends: each line, then Enter.
    pub fn keystrokes(&self) -> Vec<u8> {
        let mut text = self
            .command
            .trim_end_matches(['\r', '\n'])
            .replace("\r\n", "\n")
            .replace('\n', "\r");
        text.push('\r');
        text.into_bytes()
    }

    /// Whether every whitespace-separated term of `query` appears
    /// (case-insensitively) in the name or command.
    pub fn matches(&self, query: &str) -> bool {
        let haystack = format!("{} {}", self.name, self.command).to_lowercase();
        query
            .split_whitespace()
            .all(|term| haystack.contains(&term.to_lowercase()))
    }
}

/// Checks a snippet as typed against the saved ones: a name no other
/// snippet has (ignoring case) and a command, both trimmed. `replacing` is
/// the name before editing. The error is one line.
pub fn check(
    saved: &[Snippet],
    replacing: Option<&str>,
    typed: &Snippet,
) -> Result<Snippet, String> {
    let name = typed.name.trim();
    let command = typed.command.trim();
    if name.is_empty() {
        return Err("Name the snippet".into());
    }
    if command.is_empty() {
        return Err("A snippet needs a command".into());
    }
    if saved.iter().any(|other| {
        Some(other.name.as_str()) != replacing && other.name.eq_ignore_ascii_case(name)
    }) {
        return Err("Another snippet is already called that".into());
    }
    Ok(Snippet {
        name: name.into(),
        command: command.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snippet(name: &str, command: &str) -> Snippet {
        Snippet {
            name: name.into(),
            command: command.into(),
        }
    }

    #[test]
    fn types_each_line_then_enter() {
        assert_eq!(snippet("up", "uptime").keystrokes(), b"uptime\r");
        assert_eq!(
            snippet("two", "cd /srv\r\nls -la\n").keystrokes(),
            b"cd /srv\rls -la\r"
        );
    }

    #[test]
    fn checks_names_and_commands() {
        let saved = [snippet("Disk", "df -h"), snippet("Load", "uptime")];
        assert_eq!(
            check(&saved, None, &snippet(" Logs ", " tail -f x ")),
            Ok(snippet("Logs", "tail -f x"))
        );
        assert!(check(&saved, None, &snippet(" ", "ls")).is_err());
        assert!(check(&saved, None, &snippet("ls", " ")).is_err());
        assert_eq!(
            check(&saved, None, &snippet("disk", "du")),
            Err("Another snippet is already called that".into())
        );
        // Editing keeps its own name.
        assert!(check(&saved, Some("Disk"), &snippet("DISK", "df")).is_ok());
        assert!(snippet("Disk", "df -h").matches("DF disk"));
    }
}
