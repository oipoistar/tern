//! Tern's UI-free core. The GPUI app renders what lives here; SSH, the vault
//! and sync will land in this crate so any front end can reuse them.

pub mod agent;
pub mod certs;
pub mod draft;
pub mod env;
pub mod filter;
pub mod forward;
pub mod fuzzy;
pub mod history;
pub mod host;
pub mod identity;
pub mod keys;
pub mod known_hosts;
pub mod links;
pub mod local;
pub mod openssh;
pub mod paths;
pub mod proxy;
pub mod putty;
pub mod randomart;
pub mod reach;
pub mod reconnect;
pub mod restore;
pub mod schemes;
pub mod secrets;
pub mod settings;
pub mod sftp;
pub mod shells;
pub mod snippet;
pub mod ssh;
pub mod sync;
pub mod vault;
pub mod wol;

pub use host::{Auth, Host, HostGroup, HostStatus};
pub use vault::Vault;
