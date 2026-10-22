//! The host editor's form, checked before it becomes a saved [`Host`].

use crate::host::{Auth, Host};
use crate::vault::Vault;

/// A host as typed into the editor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostDraft {
    pub name: String,
    pub address: String,
    pub port: String,
    pub user: String,
    pub group: String,
    pub auth: Auth,
    /// `Some("")` while connecting through a jump host is switched on but
    /// none is chosen yet.
    pub jump: Option<String>,
    pub tags: Vec<String>,
    pub keep_alive: bool,
    pub on_login: String,
}

/// A form field a problem belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Field {
    Name,
    Address,
    Port,
    User,
    Key,
    Jump,
}

/// Something to fix before saving, in one line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub field: Field,
    pub message: String,
}

impl HostDraft {
    /// A blank draft for a new host in `group`.
    pub fn new(group: &str) -> Self {
        Self {
            name: String::new(),
            address: String::new(),
            port: "22".into(),
            user: String::new(),
            group: group.into(),
            auth: Auth::Agent,
            jump: None,
            tags: Vec::new(),
            keep_alive: true,
            on_login: String::new(),
        }
    }

    pub fn edit(host: &Host, group: &str) -> Self {
        Self {
            name: host.name.clone(),
            address: host.address.clone(),
            port: host.port.to_string(),
            user: host.user.clone(),
            group: group.into(),
            auth: host.auth.clone(),
            jump: host.jump.clone(),
            tags: host.tags.clone(),
            keep_alive: host.keep_alive,
            on_login: host.on_login.clone().unwrap_or_default(),
        }
    }

    /// Checks the draft against the vault. `replacing` is the name of the
    /// host being edited, so it doesn't clash with itself; its OS carries
    /// over. Every problem is reported, in form order.
    pub fn check(&self, vault: &Vault, replacing: Option<&str>) -> Result<Host, Vec<Problem>> {
        let mut problems = Vec::new();
        let mut problem = |field, message: &str| {
            problems.push(Problem {
                field,
                message: message.into(),
            })
        };
        let name = self.name.trim();
        if name.is_empty() {
            problem(Field::Name, "Name is required");
        } else if vault.hosts().any(|host| {
            Some(host.name.as_str()) != replacing && host.name.eq_ignore_ascii_case(name)
        }) {
            problem(Field::Name, "Another host is already called that");
        }
        let address = self.address.trim();
        if address.is_empty() {
            problem(Field::Address, "Host is required");
        } else if address.contains(char::is_whitespace) {
            problem(Field::Address, "Host can't contain spaces");
        }
        let port = self
            .port
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|&port| port > 0);
        if port.is_none() {
            problem(Field::Port, "Port must be a number from 1 to 65535");
        }
        let user = self.user.trim();
        if user.is_empty() {
            problem(Field::User, "Username is required");
        } else if user.contains(char::is_whitespace) {
            problem(Field::User, "Username can't contain spaces");
        }
        if let Auth::Key(path) = &self.auth {
            if path.as_os_str().is_empty() {
                problem(Field::Key, "Choose a key file");
            } else if !path.exists() {
                problem(Field::Key, "That key file doesn't exist");
            }
        }
        if let Some(jump) = self.jump.as_deref().map(str::trim) {
            match vault.find(jump) {
                _ if jump.is_empty() => problem(Field::Jump, "Choose a host to go through"),
                _ if jump.eq_ignore_ascii_case(name) || Some(jump) == replacing => {
                    problem(Field::Jump, "A host can't go through itself")
                }
                None => problem(Field::Jump, "No saved host by that name"),
                Some(via) if via.jump.is_some() => problem(
                    Field::Jump,
                    "That host goes through another one, and only one jump is supported",
                ),
                Some(via) if via.auth == Auth::Password => problem(
                    Field::Jump,
                    "That host signs in with a password, which a jump host can't use yet",
                ),
                Some(_) => {}
            }
        }
        if !problems.is_empty() {
            return Err(problems);
        }

        let previous = replacing.and_then(|old| vault.find(old));
        let on_login = self.on_login.trim();
        Ok(Host {
            name: name.into(),
            user: user.into(),
            address: address.into(),
            port: port.unwrap_or(22),
            os: previous.map(|host| host.os.clone()).unwrap_or_default(),
            tags: normalize_tags(&self.tags),
            auth: self.auth.clone(),
            jump: self.jump.as_deref().map(|jump| jump.trim().to_string()),
            keep_alive: self.keep_alive,
            on_login: (!on_login.is_empty()).then(|| on_login.to_string()),
        })
    }
}

/// Trims tags and drops empty ones and repeats (ignoring case), keeping the
/// first spelling.
pub fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for tag in tags
        .iter()
        .map(|tag| tag.trim())
        .filter(|tag| !tag.is_empty())
    {
        if !kept.iter().any(|have| have.eq_ignore_ascii_case(tag)) {
            kept.push(tag.to_string());
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(result: Result<Host, Vec<Problem>>) -> Vec<Field> {
        result
            .unwrap_err()
            .into_iter()
            .map(|problem| problem.field)
            .collect()
    }

    #[test]
    fn a_complete_draft_becomes_a_host() {
        let vault = Vault::sample();
        let mut draft = HostDraft::new("Production");
        draft.name = " db-new ".into();
        draft.address = "10.0.0.9".into();
        draft.port = "2200".into();
        draft.user = "root".into();
        draft.tags = vec!["SQL".into(), " ".into(), "sql".into(), "eu".into()];
        draft.on_login = "  ".into();
        let host = draft.check(&vault, None).unwrap();
        assert_eq!(
            (host.name.as_str(), host.port, host.user.as_str()),
            ("db-new", 2200, "root")
        );
        assert_eq!(host.tags, ["SQL", "eu"]);
        assert_eq!(host.on_login, None);
    }

    #[test]
    fn reports_every_problem_in_form_order() {
        let vault = Vault::sample();
        let mut draft = HostDraft::new("Production");
        draft.name = "PROD-DB-01".into();
        draft.address = "10.0.0 .9".into();
        draft.port = "0".into();
        draft.auth = Auth::Key(Default::default());
        assert_eq!(
            fields(draft.check(&vault, None)),
            [
                Field::Name,
                Field::Address,
                Field::Port,
                Field::User,
                Field::Key
            ]
        );
    }

    #[test]
    fn editing_changes_nothing_by_itself() {
        let vault = Vault::sample();
        let db = vault.find("prod-db-01").unwrap();
        let host = HostDraft::edit(db, "Production")
            .check(&vault, Some("prod-db-01"))
            .unwrap();
        assert_eq!(&host, db);
    }

    #[test]
    fn checks_the_jump_host() {
        let mut vault = Vault::sample();
        let db = vault.find("prod-db-01").unwrap().clone();
        let mut draft = HostDraft::edit(&db, "Production");
        let jump_problem = |draft: &HostDraft, vault: &Vault| {
            draft
                .check(vault, Some("prod-db-01"))
                .err()
                .map(|problems| problems[0].message.clone())
        };

        draft.jump = Some(String::new());
        assert_eq!(
            jump_problem(&draft, &vault).as_deref(),
            Some("Choose a host to go through")
        );
        draft.jump = Some("prod-db-01".into());
        assert_eq!(
            jump_problem(&draft, &vault).as_deref(),
            Some("A host can't go through itself")
        );
        draft.jump = Some("bastion".into());
        assert_eq!(jump_problem(&draft, &vault), None);

        let mut bastion = vault.find("bastion").unwrap().clone();
        bastion.auth = Auth::Password;
        vault.save(Some("bastion"), bastion, "Production");
        assert!(jump_problem(&draft, &vault).unwrap().contains("password"));
    }
}
