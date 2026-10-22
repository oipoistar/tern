//! Sync in the app: its state as the header and the Settings dialog show
//! it, the secrets store the passphrase lives in, and one sync run, done
//! off the UI thread.

use std::sync::{Arc, Mutex};

use gpui_kit::*;
use tern_core::Vault;
use tern_core::host::now_ms;
use tern_core::secrets::{Secrets, SystemSecrets};
use tern_core::settings::Settings;
use tern_core::sync::{self, Merged, SyncedKey, Syncer};

/// How sync stands.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Status {
    /// No folder chosen.
    #[default]
    Off,
    Syncing,
    /// Last synced then, in milliseconds since the Unix epoch.
    Synced(u64),
    /// Why the last sync didn't work, in one line.
    Failed(SharedString),
}

/// What the last sync found, for the header and the Sync page.
#[derive(Clone, Debug, Default)]
pub struct SyncState {
    pub status: Status,
    /// The other devices: name and when each last wrote.
    pub devices: Vec<(String, u64)>,
}

impl Global for SyncState {}

pub fn state(cx: &App) -> SyncState {
    cx.try_global::<SyncState>().cloned().unwrap_or_default()
}

pub fn set_state(cx: &mut App, state: SyncState) {
    cx.set_global(state);
    cx.refresh_windows();
}

/// The secrets store in effect.
pub struct SecretsStore(pub Arc<dyn Secrets>);

impl Global for SecretsStore {}

/// The system's store in the app; tests set their own, and never reach
/// the real one.
pub fn secrets(cx: &App) -> Arc<dyn Secrets> {
    if let Some(store) = cx.try_global::<SecretsStore>() {
        return store.0.clone();
    }
    if cfg!(test) {
        Arc::new(tern_core::secrets::MemorySecrets::default())
    } else {
        Arc::new(SystemSecrets)
    }
}

/// One sync: reads the other devices' files, merges them into `vault` and
/// `settings`, and writes the result as this device's file. Blocking (it
/// derives keys and touches files); run it off the UI thread.
pub fn run(
    syncer: &Mutex<Syncer>,
    vault: &Vault,
    settings: &Settings,
    keys: Vec<SyncedKey>,
) -> Result<(Merged, Vec<(String, u64)>), String> {
    let folder = settings.sync.folder.clone().ok_or("No sync folder")?;
    let mut syncer = syncer.lock().unwrap();
    let others = syncer.read(&folder, &settings.sync.device_id)?;
    let merged = sync::merge(vault, settings, &others);
    // Ours carries what was merged, newer settings included.
    let mut ours = settings.clone();
    if let Some(newer) = &merged.settings {
        ours.appearance = newer.appearance;
        ours.terminal = newer.terminal.clone();
        ours.updated = newer.updated;
    }
    syncer.write(&folder, &sync::snapshot(&merged.vault, &ours, keys))?;
    let devices = others
        .iter()
        .map(|snapshot| (snapshot.device.clone(), snapshot.written))
        .collect();
    Ok((merged, devices))
}

/// "just now", "2 min ago"… for a time in milliseconds.
pub fn ago(ms: u64) -> String {
    tern_core::history::ago(ms / 1000, now_ms() / 1000)
}
