//! Identities: a username and a way to sign in, saved once under a name
//! and used by any number of hosts, as Termius and OpenSSH's `User` plus
//! `IdentityFile` are. An identity's password, if it has one, lives in the
//! system's credential store, never in the vault file.

use serde::{Deserialize, Serialize};

use crate::host::{Auth, Unknown, new_id};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// Stays through renames, so hosts and sync can tell identities apart.
    #[serde(default = "new_id")]
    pub id: String,
    pub name: String,
    pub user: String,
    #[serde(default)]
    pub auth: Auth,
    /// When it last changed, in milliseconds since the Unix epoch.
    #[serde(default)]
    pub updated: u64,
    /// What a newer Tern added that this one doesn't know, kept as it came.
    #[serde(flatten)]
    pub unknown: Unknown,
}

impl Identity {
    pub fn new(name: &str, user: &str, auth: Auth) -> Self {
        Self {
            id: new_id(),
            name: name.into(),
            user: user.into(),
            auth,
            updated: 0,
            unknown: Unknown::new(),
        }
    }
}

/// The name an identity's password is kept under in the credential store.
pub fn identity_password(id: &str) -> String {
    format!("identity password {id}")
}

/// An identity as typed: checked against the others (`replacing` is the id
/// of the one being edited). Fails with a one-line reason.
pub fn check(
    identities: &[Identity],
    replacing: Option<&str>,
    name: &str,
    user: &str,
    auth: Auth,
) -> Result<Identity, String> {
    let (name, user) = (name.trim(), user.trim());
    if name.is_empty() {
        return Err("Name is required".into());
    }
    if identities
        .iter()
        .any(|other| Some(other.id.as_str()) != replacing && other.name.eq_ignore_ascii_case(name))
    {
        return Err("Another identity is already called that".into());
    }
    if user.is_empty() {
        return Err("Username is required".into());
    }
    if user.contains(char::is_whitespace) {
        return Err("Username can't contain spaces".into());
    }
    if let Auth::Key(path) = &auth {
        if path.as_os_str().is_empty() {
            return Err("Choose a key file".into());
        }
        if !path.exists() {
            return Err("That key file doesn't exist".into());
        }
    }
    let previous = replacing.and_then(|id| identities.iter().find(|other| other.id == id));
    Ok(Identity {
        id: previous.map_or_else(new_id, |identity| identity.id.clone()),
        name: name.into(),
        user: user.into(),
        auth,
        updated: previous.map_or(0, |identity| identity.updated),
        unknown: previous
            .map(|identity| identity.unknown.clone())
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_edit_keeps_what_this_tern_doesnt_know() {
        let mut ops = Identity::new("ops", "deploy", Auth::Agent);
        ops.unknown.insert("fido".into(), "yes".into());
        let all = [ops.clone()];
        let edited = check(&all, Some(&ops.id), "ops", "root", Auth::Agent).unwrap();
        assert_eq!(
            (edited.user.as_str(), &edited.unknown),
            ("root", &ops.unknown)
        );
    }

    #[test]
    fn checks_what_an_identity_needs() {
        let ops = Identity::new("ops", "deploy", Auth::Agent);
        let all = [ops.clone()];
        assert_eq!(
            check(&all, None, " ", "x", Auth::Agent).unwrap_err(),
            "Name is required"
        );
        assert!(
            check(&all, None, "OPS", "x", Auth::Agent)
                .unwrap_err()
                .contains("already")
        );
        assert!(check(&all, None, "web", "two words", Auth::Agent).is_err());
        assert!(check(&all, None, "web", "x", Auth::Key("nowhere/key".into())).is_err());
        // Editing keeps the id, and its own name is fine.
        let edited = check(&all, Some(&ops.id), "ops", " root ", Auth::Password).unwrap();
        assert_eq!(
            (edited.id, edited.user, edited.auth),
            (ops.id, "root".to_string(), Auth::Password)
        );
        assert_eq!(identity_password("abc"), "identity password abc");
    }
}
