//! Tern's colour tokens, mirroring the mockup canvas (light and dark).

use std::rc::Rc;

use gpui_kit::{App, BoxShadow, Global, Hsla, Rgba, hsla, point, px, rgb, rgba};

/// Light or dark, shared by every view. Change it with [`set_dark`].
#[derive(Clone, Copy)]
pub struct Appearance {
    pub dark: bool,
}

impl Global for Appearance {}

/// The theme for the current appearance.
pub fn current(cx: &App) -> Rc<Theme> {
    let dark = cx.try_global::<Appearance>().is_some_and(|a| a.dark);
    Rc::new(if dark { Theme::dark() } else { Theme::light() })
}

pub fn set_dark(dark: bool, cx: &mut App) {
    cx.set_global(Appearance { dark });
    // Keep GPUI Kit's own components (the text input) in the same mode.
    use gpui_kit::component::{Theme as KitTheme, ThemeMode};
    KitTheme::change(
        if dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        None,
        cx,
    );
    // Cached views don't watch the global, so redraw everything once.
    cx.refresh_windows();
}

/// Colours for terminal content. The dark-teal ground matches the mockup's
/// terminal in both appearances.
pub struct TermPalette {
    pub background: Hsla,
    pub foreground: Hsla,
    pub cursor: Hsla,
    /// The 16 ANSI colours: normal 0–7, then bright 8–15.
    pub ansi: [Hsla; 16],
}

impl TermPalette {
    pub fn for_theme(theme: &Theme) -> Self {
        let ansi = [
            0x1B3A3B, 0xF07178, 0x7FE0B4, 0xFFCB6B, 0x82AAFF, 0xC792EA, 0x89DDFF,
            0xD3E6E3, // normal
            0x557774, 0xFF8A8A, 0xA6F0CD, 0xFFDD8E, 0xA6C4FF, 0xDDB3F5, 0xB2EBFF,
            0xFFFFFF, // bright
        ]
        .map(|hex| c(rgb(hex)));
        Self {
            background: c(rgb(if theme.dark { 0x0A1A1B } else { 0x10292A })),
            foreground: c(rgb(0xD3E6E3)),
            cursor: c(rgb(0x7FE0B4)),
            ansi,
        }
    }
}

/// Monospace family for addresses, fingerprints and terminal text.
pub const MONO: &str = if cfg!(target_os = "macos") {
    "Menlo"
} else if cfg!(target_os = "windows") {
    "Consolas"
} else {
    "DejaVu Sans Mono"
};

fn c(color: Rgba) -> Hsla {
    color.into()
}

/// Voxel colour family for a glass surface: brand / primary action, healthy,
/// or in progress. (The mockup's orange "warn" tone joins with the keychain.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Brand,
    Ok,
    Info,
}

impl Tone {
    /// Base hue (degrees) and saturation (0..1) the voxels are drawn around.
    pub fn hue_sat(self) -> (f32, f32) {
        match self {
            Tone::Brand => (250., 0.72),
            Tone::Ok => (150., 0.50),
            Tone::Info => (214., 0.80),
        }
    }
}

/// How opaque the milky layer over the voxels is. Chips use `Strong` so text
/// stays readable at small sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Milk {
    Regular,
    Strong,
}

#[derive(Clone, Debug)]
pub struct Theme {
    pub dark: bool,
    pub win: Hsla,
    pub card: Hsla,
    pub card2: Hsla,
    pub text: Hsla,
    pub text2: Hsla,
    pub text3: Hsla,
    pub line: Hsla,
    pub glass_text: Hsla,
    pub ok_glyph: Hsla,
    pub ok_halo: Hsla,
    pub info_glyph: Hsla,
    pub danger: Hsla,
    pub selection: Hsla,
}

impl Theme {
    pub fn light() -> Self {
        Self {
            dark: false,
            win: c(rgb(0xF7F7F7)),
            card: c(rgb(0xFFFFFF)),
            card2: c(rgb(0xEDEDED)),
            text: c(rgb(0x141414)),
            text2: c(rgb(0x5E5E5E)),
            text3: c(rgb(0x737373)),
            line: c(rgba(0x00000012)),
            glass_text: c(rgb(0x15151C)),
            ok_glyph: c(rgb(0x13804A)),
            ok_halo: c(rgba(0x13804A29)),
            info_glyph: c(rgb(0x1D5FD1)),
            danger: c(rgb(0xC4321F)),
            selection: c(rgb(0x6C63FF)),
        }
    }

    pub fn dark() -> Self {
        Self {
            dark: true,
            win: c(rgb(0x131416)),
            card: c(rgb(0x1C1D20)),
            card2: c(rgb(0x26272B)),
            text: c(rgb(0xF2F2F3)),
            text2: c(rgb(0xA6A6AE)),
            text3: c(rgb(0x8E8E96)),
            line: c(rgba(0xFFFFFF12)),
            glass_text: c(rgb(0xF6F6F8)),
            ok_glyph: c(rgb(0x6BE39A)),
            ok_halo: c(rgba(0x6BE39A38)),
            info_glyph: c(rgb(0x8DBBFF)),
            danger: c(rgb(0xFF8A80)),
            selection: c(rgb(0x8F86FF)),
        }
    }

    pub fn shadow(&self) -> Vec<BoxShadow> {
        if self.dark {
            vec![
                BoxShadow::new(px(0.), px(1.), hsla(0., 0., 0., 0.5)).blur_radius(px(2.)),
                BoxShadow::new(px(0.), px(8.), hsla(0., 0., 0., 0.35)).blur_radius(px(24.)),
            ]
        } else {
            vec![
                BoxShadow::new(px(0.), px(1.), hsla(0., 0., 0., 0.04)).blur_radius(px(2.)),
                BoxShadow::new(px(0.), px(8.), hsla(0., 0., 0., 0.06)).blur_radius(px(24.)),
            ]
        }
    }

    pub fn shadow_sm(&self) -> Vec<BoxShadow> {
        let alpha = if self.dark { 0.5 } else { 0.06 };
        vec![BoxShadow::new(px(0.), px(1.), hsla(0., 0., 0., alpha)).blur_radius(px(2.))]
    }

    /// Outer shadow for a glass surface; slightly tinted so it reads as lit.
    pub fn glass_shadow(&self) -> Vec<BoxShadow> {
        if self.dark {
            vec![BoxShadow::new(px(0.), px(4.), hsla(0., 0., 0., 0.4)).blur_radius(px(12.))]
        } else {
            vec![
                BoxShadow::new(px(0.), px(3.), hsla(240. / 360., 0.38, 0.25, 0.12))
                    .blur_radius(px(10.)),
                BoxShadow::new(px(0.), px(1.), hsla(0., 0., 0., 0.05)).blur_radius(px(1.)),
            ]
        }
    }

    /// Liquid-glass rim: a bright top edge, a thin all-round ring and a soft
    /// inner glow (light) or inner shade (dark).
    pub fn glass_edge(&self) -> Vec<BoxShadow> {
        if self.dark {
            vec![
                BoxShadow::new(px(0.), px(1.), hsla(0., 0., 1., 0.30)).inset(),
                BoxShadow::new(px(0.), px(0.), hsla(0., 0., 1., 0.10))
                    .spread_radius(px(1.))
                    .inset(),
                BoxShadow::new(px(0.), px(-10.), hsla(0., 0., 0., 0.22))
                    .blur_radius(px(18.))
                    .inset(),
            ]
        } else {
            vec![
                BoxShadow::new(px(0.), px(1.), hsla(0., 0., 1., 0.95)).inset(),
                BoxShadow::new(px(0.), px(0.), hsla(0., 0., 1., 0.45))
                    .spread_radius(px(1.))
                    .inset(),
                BoxShadow::new(px(0.), px(-8.), hsla(0., 0., 1., 0.28))
                    .blur_radius(px(16.))
                    .inset(),
            ]
        }
    }

    /// The milky layer laid over the voxels.
    pub fn milk(&self, milk: Milk) -> Hsla {
        match (self.dark, milk) {
            (false, Milk::Regular) => hsla(0., 0., 1., 0.42),
            (false, Milk::Strong) => hsla(0., 0., 1., 0.62),
            (true, Milk::Regular) => hsla(240. / 360., 0.12, 0.10, 0.30),
            (true, Milk::Strong) => hsla(240. / 360., 0.12, 0.10, 0.50),
        }
    }

    /// Selection ring around the chosen card.
    pub fn selected_shadow(&self) -> Vec<BoxShadow> {
        let mut shadows =
            vec![BoxShadow::new(px(0.), px(0.), self.selection).spread_radius(px(2.))];
        shadows.extend(self.shadow());
        shadows
    }

    pub fn status_dot_halo(&self) -> BoxShadow {
        BoxShadow {
            color: self.ok_halo,
            offset: point(px(0.), px(0.)),
            blur_radius: px(0.),
            spread_radius: px(3.),
            inset: false,
        }
    }
}
