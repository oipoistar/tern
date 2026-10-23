//! Tern's colour tokens, mirroring the mockup canvas (light and dark).
//!
//! The chrome is monochrome: ink (`text`) on paper (`card`). Solid ink marks
//! only what is selected or primary; colour is left to terminal content.

use std::rc::Rc;

use gpui_kit::{App, BoxShadow, Global, Hsla, Rgba, hsla, px, rgb, rgba};

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

/// Colours for terminal content: a neutral ink ground in both appearances,
/// with the usual ANSI hues for what programs print.
pub struct TermPalette {
    pub background: Hsla,
    pub foreground: Hsla,
    pub cursor: Hsla,
    /// The 16 ANSI colours: normal 0–7, then bright 8–15.
    pub ansi: [Hsla; 16],
}

impl TermPalette {
    /// The palette terminals draw with now: the chosen scheme, or Tern's
    /// own for the app's look.
    pub fn current(cx: &App) -> Self {
        let theme = current(cx);
        match crate::settings::current(cx).terminal.colors.as_deref() {
            Some(name) => tern_core::schemes::find(name)
                .map(Self::for_scheme)
                .unwrap_or_else(|| Self::for_theme(&theme)),
            None => Self::for_theme(&theme),
        }
    }

    pub fn for_scheme(scheme: &tern_core::schemes::Scheme) -> Self {
        Self {
            background: c(rgb(scheme.background)),
            foreground: c(rgb(scheme.foreground)),
            cursor: c(rgb(scheme.cursor)),
            ansi: scheme.ansi.map(|hex| c(rgb(hex))),
        }
    }

    pub fn for_theme(theme: &Theme) -> Self {
        let ansi = [
            0x2A2A2D, 0xF07178, 0x8CD3A5, 0xFFCB6B, 0x82AAFF, 0xC792EA, 0x89DDFF,
            0xD9D9DC, // normal
            0x6E6E73, 0xFF8A8A, 0xB4E6C4, 0xFFDD8E, 0xA6C4FF, 0xDDB3F5, 0xB2EBFF,
            0xFFFFFF, // bright
        ]
        .map(|hex| c(rgb(hex)));
        Self {
            background: c(rgb(if theme.dark { 0x0E0E10 } else { 0x141414 })),
            foreground: c(rgb(0xE6E6E8)),
            cursor: c(rgb(0xE6E6E8)),
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
    /// A switch's track while off.
    pub track: Hsla,
    pub danger: Hsla,
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
            line: c(rgba(0x00000017)),
            track: c(rgb(0xD4D4D9)),
            danger: c(rgb(0xC4321F)),
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
            line: c(rgba(0xFFFFFF1A)),
            track: c(rgb(0x3A3B40)),
            danger: c(rgb(0xFF8A80)),
        }
    }

    /// Colours for something shown inverted, like the selected host: ink
    /// becomes the surface and paper the text.
    pub fn inverse(&self) -> Self {
        // Secondary text and hairlines are the paper (or ink) at reduced
        // strength, so they sit on the inverted surface the way the normal
        // ones sit on a card.
        let (text2, text3, line) = if self.dark {
            (
                c(rgba(0x141416A0)),
                c(rgba(0x14141666)),
                c(rgba(0x00000024)),
            )
        } else {
            (
                c(rgba(0xFFFFFFA3)),
                c(rgba(0xFFFFFF6B)),
                c(rgba(0xFFFFFF2E)),
            )
        };
        Self {
            card: self.text,
            text: self.card,
            text2,
            text3,
            line,
            ..self.clone()
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

    /// For a dialog floating over the screen.
    pub fn shadow_lg(&self) -> Vec<BoxShadow> {
        if self.dark {
            vec![BoxShadow::new(px(0.), px(30.), hsla(0., 0., 0., 0.6)).blur_radius(px(80.))]
        } else {
            vec![
                BoxShadow::new(px(0.), px(2.), hsla(0., 0., 0., 0.04)).blur_radius(px(6.)),
                BoxShadow::new(px(0.), px(30.), hsla(0., 0., 0., 0.10)).blur_radius(px(80.)),
            ]
        }
    }

    /// Dims the screen behind a dialog.
    pub fn scrim(&self) -> Hsla {
        hsla(0., 0., 0., if self.dark { 0.5 } else { 0.18 })
    }
}
