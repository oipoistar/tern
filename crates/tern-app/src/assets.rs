//! Icon assets: GPUI Kit's default component icons plus the few extra Lucide
//! icons Tern uses, embedded selectively so the binary stays small.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

gpui_kit::assets::icon_assets!(
    TernIcons,
    [
        ArrowLeftRight,
        Clock,
        Cloud,
        CodeXml,
        Funnel,
        Import,
        KeyRound,
        Laptop,
        LayoutGrid,
        List,
        Lock,
        Pencil,
        Server,
        Shield,
        ShieldCheck,
        Zap
    ]
);

pub struct TernAssets;

impl AssetSource for TernAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = TernIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(TernIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
