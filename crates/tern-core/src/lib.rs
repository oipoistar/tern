//! Tern's UI-free core. The GPUI app renders what lives here; SSH, the vault
//! and sync will land in this crate so any front end can reuse them.

pub mod draft;
pub mod forward;
pub mod history;
pub mod host;
pub mod keys;
pub mod known_hosts;
pub mod paths;
pub mod putty;
pub mod randomart;
pub mod reach;
pub mod settings;
pub mod sftp;
pub mod snippet;
pub mod ssh;
pub mod vault;

pub use host::{Auth, Host, HostGroup, HostStatus};
pub use vault::Vault;
