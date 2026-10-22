//! Tern's UI-free core. The GPUI app renders what lives here; SSH, the vault
//! and sync will land in this crate so any front end can reuse them.

pub mod draft;
pub mod host;
pub mod paths;
pub mod randomart;
pub mod ssh;
pub mod vault;

pub use host::{Auth, Host, HostGroup, HostStatus};
pub use vault::Vault;
