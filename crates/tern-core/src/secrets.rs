//! Secrets kept in the system's own store (Windows Credential Manager,
//! macOS Keychain, the Secret Service on Linux), never in Tern's files: the
//! sync passphrase and saved host passwords.

use std::collections::HashMap;
use std::sync::Mutex;

/// Where secrets live; the system's store in the app, memory in tests.
pub trait Secrets: Send + Sync {
    fn store(&self, name: &str, secret: &str) -> Result<(), String>;
    fn load(&self, name: &str) -> Option<String>;
    /// Forgets a secret; one that isn't there is already forgotten.
    fn forget(&self, name: &str) -> Result<(), String>;
}

/// The name the first vault's sync passphrase is kept under.
pub const SYNC_PASSPHRASE: &str = "sync passphrase";

/// The name the vault `id`'s sync passphrase is kept under.
pub fn sync_passphrase(id: &str) -> String {
    if id == crate::vaults::FIRST {
        SYNC_PASSPHRASE.into()
    } else {
        format!("{SYNC_PASSPHRASE} {id}")
    }
}

/// The name a saved host's password is kept under, by the host's id so a
/// rename keeps it.
pub fn host_password(host_id: &str) -> String {
    format!("password {host_id}")
}

/// The system's credential store, as the service "Tern".
pub struct SystemSecrets;

const SERVICE: &str = "Tern";

impl Secrets for SystemSecrets {
    fn store(&self, name: &str, secret: &str) -> Result<(), String> {
        keyring::Entry::new(SERVICE, name)
            .and_then(|entry| entry.set_password(secret))
            .map_err(|error| error.to_string())
    }

    fn load(&self, name: &str) -> Option<String> {
        keyring::Entry::new(SERVICE, name)
            .and_then(|entry| entry.get_password())
            .ok()
    }

    fn forget(&self, name: &str) -> Result<(), String> {
        match keyring::Entry::new(SERVICE, name).and_then(|entry| entry.delete_credential()) {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// Secrets in memory, for tests.
#[derive(Default)]
pub struct MemorySecrets(Mutex<HashMap<String, String>>);

impl Secrets for MemorySecrets {
    fn store(&self, name: &str, secret: &str) -> Result<(), String> {
        self.0.lock().unwrap().insert(name.into(), secret.into());
        Ok(())
    }

    fn load(&self, name: &str) -> Option<String> {
        self.0.lock().unwrap().get(name).cloned()
    }

    fn forget(&self, name: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(name);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_secrets_store_load_and_forget() {
        let secrets = MemorySecrets::default();
        assert_eq!(secrets.load(SYNC_PASSPHRASE), None);
        secrets.store(SYNC_PASSPHRASE, "open sesame").unwrap();
        assert_eq!(
            secrets.load(SYNC_PASSPHRASE).as_deref(),
            Some("open sesame")
        );
        secrets.forget(SYNC_PASSPHRASE).unwrap();
        secrets.forget(SYNC_PASSPHRASE).unwrap();
        assert_eq!(secrets.load(SYNC_PASSPHRASE), None);
        assert_eq!(host_password("abc"), "password abc");
        assert_eq!(sync_passphrase(crate::vaults::FIRST), SYNC_PASSPHRASE);
        assert_eq!(sync_passphrase("0123abcd"), "sync passphrase 0123abcd");
    }
}
