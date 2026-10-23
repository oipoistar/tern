//! Snippets: saved commands, typed into a session from its snippet bar.
//! They live in the vault file beside the hosts.

use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::host::{Unknown, new_id};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snippet {
    /// Stays through renames, so sync can tell snippets apart.
    #[serde(default = "new_id")]
    pub id: String,
    pub name: String,
    pub command: String,
    /// When it last changed, in milliseconds since the Unix epoch.
    #[serde(default)]
    pub updated: u64,
    /// What a newer Tern added that this one doesn't know, kept as it came.
    #[serde(flatten)]
    pub unknown: Unknown,
}

impl Snippet {
    /// A new snippet, as typed.
    pub fn new(name: &str, command: &str) -> Self {
        Self {
            id: new_id(),
            name: name.into(),
            command: command.into(),
            updated: 0,
            unknown: Unknown::new(),
        }
    }

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

    /// The command as one line for a shell, its lines joined by returns.
    pub fn line(&self) -> String {
        self.command
            .trim_end_matches(['\r', '\n'])
            .replace("\r\n", "\n")
            .replace('\n', "\r")
    }

    /// The `{{name}}` placeholders in the command, each once, in order:
    /// values to ask for before typing it.
    pub fn variables(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for (_, name) in placeholders(&self.command) {
            if !names.iter().any(|have| have == name) {
                names.push(name.to_string());
            }
        }
        names
    }

    /// A copy with each `{{name}}` replaced by its value; placeholders
    /// without one stay as they are.
    pub fn filled(&self, values: &[(String, String)]) -> Snippet {
        let mut command = String::new();
        let mut copied = 0;
        for (range, name) in placeholders(&self.command) {
            if let Some((_, value)) = values.iter().find(|(have, _)| have == name) {
                command.push_str(&self.command[copied..range.start]);
                command.push_str(value);
                copied = range.end;
            }
        }
        command.push_str(&self.command[copied..]);
        Snippet {
            command,
            ..self.clone()
        }
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

/// Each `{{name}}` placeholder in `command`, with where it is: a name is a
/// letter or `_`, then letters, digits, `_` or `-`, spaced inside the
/// braces as it likes. Other braces, like a Go template's `{{.Names}}`,
/// are only text.
fn placeholders(command: &str) -> Vec<(Range<usize>, &str)> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(start) = command[from..].find("{{").map(|at| from + at) {
        let inside = start + 2;
        let Some(length) = command[inside..].find("}}") else {
            break;
        };
        let name = command[inside..inside + length].trim_matches(' ');
        let is_name = name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if is_name {
            let end = inside + length + 2;
            found.push((start..end, name));
            from = end;
        } else {
            from = inside;
        }
    }
    found
}

/// Checks a snippet as typed against the saved ones: a name no other
/// snippet has (ignoring case) and a command, both trimmed. `replacing` is
/// the name before editing, whose id the result keeps. The error is one
/// line.
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
    let replaced = replacing.and_then(|old| saved.iter().find(|snippet| snippet.name == old));
    Ok(Snippet {
        id: replaced.map_or_else(|| typed.id.clone(), |old| old.id.clone()),
        name: name.into(),
        command: command.into(),
        updated: typed.updated,
        unknown: replaced.map(|old| old.unknown.clone()).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snippet(name: &str, command: &str) -> Snippet {
        Snippet::new(name, command)
    }

    #[test]
    fn asks_for_its_variables() {
        let deploy = snippet(
            "deploy",
            "cd {{app}} && git checkout {{ branch }} && make {{app}}",
        );
        assert_eq!(deploy.variables(), ["app", "branch"]);
        let values = [
            ("app".to_string(), "/srv/web".to_string()),
            ("branch".to_string(), "main".to_string()),
        ];
        assert_eq!(
            deploy.filled(&values).command,
            "cd /srv/web && git checkout main && make /srv/web"
        );
        assert!(snippet("plain", "df -h").variables().is_empty());
        assert!(
            snippet("braces", "echo {{}} {{ }} {{open")
                .variables()
                .is_empty()
        );
    }

    #[test]
    fn leaves_other_braces_alone() {
        // Any spacing inside the braces, filled the same.
        let forget = snippet("forget", "ssh-keygen -R {{ host}} && ssh {{host  }}");
        assert_eq!(forget.variables(), ["host"]);
        let values = [("host".to_string(), "web-01".to_string())];
        assert_eq!(
            forget.filled(&values).command,
            "ssh-keygen -R web-01 && ssh web-01"
        );
        // Go templates aren't names, so they're typed as written.
        let docker = snippet(
            "names",
            "docker ps --format '{{.Names}} {{json .}}' {{ {{tag}} }}",
        );
        assert_eq!(docker.variables(), ["tag"]);
        let values = [("tag".to_string(), "v2".to_string())];
        assert_eq!(
            docker.filled(&values).command,
            "docker ps --format '{{.Names}} {{json .}}' {{ v2 }}"
        );
    }

    #[test]
    fn an_edit_keeps_what_this_tern_doesnt_know() {
        let mut disk = snippet("Disk", "df -h");
        disk.unknown.insert("pinned".into(), true.into());
        let saved = [disk.clone()];
        let edited = check(&saved, Some("Disk"), &snippet("Disk", "df -hT")).unwrap();
        assert_eq!(
            (edited.command.as_str(), &edited.unknown),
            ("df -hT", &disk.unknown)
        );
    }

    #[test]
    fn joins_its_lines_with_returns() {
        let disk = snippet("disk", "df -h\nuptime\n");
        assert_eq!(disk.line(), "df -h\ruptime");
    }

    fn named(result: Result<Snippet, String>) -> Result<(String, String), String> {
        result.map(|snippet| (snippet.name, snippet.command))
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
            named(check(&saved, None, &snippet(" Logs ", " tail -f x "))),
            Ok(("Logs".into(), "tail -f x".into()))
        );
        assert!(check(&saved, None, &snippet(" ", "ls")).is_err());
        assert!(check(&saved, None, &snippet("ls", " ")).is_err());
        assert_eq!(
            check(&saved, None, &snippet("disk", "du")),
            Err("Another snippet is already called that".into())
        );
        // Editing keeps its own name, and its id.
        let edited = check(&saved, Some("Disk"), &snippet("DISK", "df")).unwrap();
        assert_eq!(edited.id, saved[0].id);
        assert!(snippet("Disk", "df -h").matches("DF disk"));
    }
}
