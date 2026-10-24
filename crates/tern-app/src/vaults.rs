//! The vaults this Tern keeps and the one open, shared as a global and
//! saved to `vaults.json` on each change; and how the open vault syncs:
//! the first vault's sync lives in the settings, as it always did, the
//! others' in the list, each with a passphrase of its own.

use std::path::PathBuf;

use gpui_kit::*;
use tern_core::paths;
use tern_core::secrets;
use tern_core::settings::Settings;
use tern_core::sync::SyncSettings;
use tern_core::vaults::{FIRST, Vaults};

use crate::settings;

/// The vaults, and where the list is saved (`None` in tests).
struct Open {
    vaults: Vaults,
    file: Option<PathBuf>,
}

impl Global for Open {}

/// Loads the list of vaults.
pub fn init(cx: &mut App) {
    let file = paths::vaults();
    cx.set_global(Open {
        vaults: Vaults::load(&file),
        file: Some(file),
    });
}

/// The vaults; the first alone before [`init`].
pub fn list(cx: &App) -> Vaults {
    cx.try_global::<Open>()
        .map(|open| open.vaults.clone())
        .unwrap_or_default()
}

/// The id of the vault open.
pub fn current(cx: &App) -> String {
    cx.try_global::<Open>()
        .map_or_else(|| FIRST.into(), |open| open.vaults.current.clone())
}

/// Changes the list, saves it and redraws.
pub fn update(cx: &mut App, change: impl FnOnce(&mut Vaults)) {
    let file = cx.try_global::<Open>().and_then(|open| open.file.clone());
    let mut vaults = list(cx);
    change(&mut vaults);
    if let Some(file) = &file {
        // Not written down, the change only doesn't outlast Tern.
        let _ = vaults.save(file);
    }
    cx.set_global(Open { vaults, file });
    cx.refresh_windows();
}

/// How the open vault syncs. Settings sync with the first vault only.
pub fn sync_settings(cx: &App) -> SyncSettings {
    let vaults = list(cx);
    if vaults.current == FIRST {
        return settings::current(cx).sync;
    }
    let mut sync = vaults.current().sync.clone().unwrap_or_default();
    sync.settings = false;
    sync
}

/// Changes how the open vault syncs.
pub fn update_sync(cx: &mut App, change: impl FnOnce(&mut SyncSettings)) {
    let current = current(cx);
    if current == FIRST {
        settings::update(cx, |settings| change(&mut settings.sync));
        return;
    }
    let mut sync = sync_settings(cx);
    change(&mut sync);
    update(cx, |vaults| {
        if let Some(vault) = vaults.vaults.iter_mut().find(|vault| vault.id == current) {
            vault.sync = Some(sync);
        }
    });
}

/// The name the open vault's sync passphrase is kept under.
pub fn passphrase(cx: &App) -> String {
    secrets::sync_passphrase(&current(cx))
}

/// The settings as the open vault's sync takes them: with its own way of
/// syncing.
pub fn sync_view(cx: &App) -> Settings {
    Settings {
        sync: sync_settings(cx),
        ..settings::current(cx)
    }
}

/// Sets the list for a test, kept in memory.
#[cfg(test)]
pub fn set_for_test(cx: &mut App, vaults: Vaults) {
    cx.set_global(Open { vaults, file: None });
}

#[cfg(test)]
mod tests {
    // Explicit imports: `gpui_kit::*` would shadow Rust's `#[test]`.
    use gpui::TestAppContext;
    use tern_core::vaults::{FIRST, Vaults};

    use super::{passphrase, set_for_test, sync_settings, sync_view, update, update_sync};
    use crate::settings;

    #[gpui::test]
    fn each_vault_syncs_its_own_way(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut list = Vaults::default();
            let work = list.add("Work");
            list.current = work.clone();
            set_for_test(cx, list);

            update_sync(cx, |sync| {
                sync.folder = Some("W:/sync".into());
                sync.settings = true;
            });
            assert_eq!(sync_settings(cx).folder, Some("W:/sync".into()));
            // Settings sync with the first vault only, whose sync is apart.
            assert!(!sync_view(cx).sync.settings);
            assert_eq!(settings::current(cx).sync.folder, None);
            assert_eq!(passphrase(cx), format!("sync passphrase {work}"));

            update(cx, |vaults| vaults.current = FIRST.into());
            assert_eq!(sync_settings(cx).folder, None);
            assert_eq!(passphrase(cx), "sync passphrase");
            update_sync(cx, |sync| sync.folder = Some("P:/sync".into()));
            assert_eq!(settings::current(cx).sync.folder, Some("P:/sync".into()));
        });
    }
}
