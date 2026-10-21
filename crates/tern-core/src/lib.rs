//! Tern's UI-free core. The GPUI app renders what lives here; SSH, the vault
//! and sync will land in this crate so any front end can reuse them.

pub mod host;

pub use host::{Host, HostGroup, HostStatus};
