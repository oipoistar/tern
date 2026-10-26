//! Tern's settings, in `settings.json` in the data folder: the appearance
//! and how terminals look and behave. Values out of range are pulled back
//! in, and a missing or unreadable file means the defaults.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::host::Unknown;
use crate::paths;
use crate::sync::SyncSettings;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    pub terminal: TerminalSettings,
    /// How this device syncs; not synced itself.
    pub sync: SyncSettings,
    /// When the appearance or terminal settings last changed, in
    /// milliseconds since the Unix epoch; sync keeps the newest.
    pub updated: u64,
    /// Open the tabs that were open when Tern last quit; this device's
    /// own, not synced.
    pub reopen_tabs: bool,
    /// Agents may drive Tern through its MCP server (`tern --mcp`, see
    /// [`crate::mcp`]); this device's own, not synced.
    pub mcp: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            appearance: Appearance::default(),
            terminal: TerminalSettings::default(),
            sync: SyncSettings::default(),
            updated: 0,
            reopen_tabs: true,
            mcp: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    /// Follows the system's light or dark mode.
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorStyle {
    #[default]
    Block,
    Beam,
    Underline,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bell {
    /// The terminal flashes.
    #[default]
    Visual,
    Off,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalSettings {
    /// Empty for the platform's monospace font.
    pub font_family: String,
    pub font_size: f32,
    /// The cursor's shape unless a program asks for another.
    pub cursor: CursorStyle,
    /// Lines kept above the screen.
    pub scrollback: usize,
    pub bell: Bell,
    /// Selecting text copies it.
    pub copy_on_select: bool,
    /// Ask before pasting several lines into a shell that would run each
    /// as it arrives.
    pub confirm_paste: bool,
    /// On a slow connection, show what's typed before the server echoes
    /// it ([`crate::predict`]).
    pub predict_typing: bool,
    /// A colour scheme by name ([`crate::schemes`]); none follows the
    /// app's look.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colors: Option<String>,
    /// What a newer Tern added that this one doesn't know, kept as it came.
    #[serde(flatten)]
    pub unknown: Unknown,
}

impl TerminalSettings {
    pub const MIN_FONT_SIZE: f32 = 8.;
    pub const MAX_FONT_SIZE: f32 = 32.;
    pub const MAX_SCROLLBACK: usize = 100_000;

    /// The same settings with every value in range.
    pub fn clamped(mut self) -> Self {
        self.font_family = self.font_family.trim().to_string();
        self.font_size = if self.font_size.is_finite() {
            self.font_size
                .clamp(Self::MIN_FONT_SIZE, Self::MAX_FONT_SIZE)
        } else {
            Self::default().font_size
        };
        self.scrollback = self.scrollback.min(Self::MAX_SCROLLBACK);
        // A scheme this version doesn't have falls back to Tern's own.
        self.colors = self
            .colors
            .as_deref()
            .and_then(crate::schemes::find)
            .map(|scheme| scheme.name.to_string());
        self
    }

    /// Font zoom (Ctrl+= and Ctrl+-): `steps` points larger or smaller,
    /// kept in range; `None` goes back to the default size.
    pub fn zoom(&mut self, steps: Option<i8>) {
        self.font_size = match steps {
            Some(steps) => {
                (self.font_size + f32::from(steps)).clamp(Self::MIN_FONT_SIZE, Self::MAX_FONT_SIZE)
            }
            None => Self::default().font_size,
        };
    }
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            font_family: String::new(),
            font_size: 13.5,
            cursor: CursorStyle::Block,
            scrollback: 10_000,
            bell: Bell::Visual,
            copy_on_select: false,
            confirm_paste: true,
            predict_typing: true,
            colors: None,
            unknown: Unknown::new(),
        }
    }
}

impl Settings {
    /// The settings in `path`, in range; the defaults when there's no file
    /// or it can't be read.
    pub fn load(path: &Path) -> Self {
        paths::settle(path);
        let mut settings: Self = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(text.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default();
        settings.terminal = settings.terminal.clamped();
        settings
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        paths::write_atomically(path, json.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saves_loads_and_falls_back_to_defaults() {
        let dir = std::env::temp_dir().join(format!("tern-settings-{}", std::process::id()));
        let path = dir.join("settings.json");
        assert_eq!(Settings::load(&path), Settings::default());

        let settings = Settings {
            appearance: Appearance::Dark,
            terminal: TerminalSettings {
                cursor: CursorStyle::Beam,
                copy_on_select: true,
                ..TerminalSettings::default()
            },
            ..Settings::default()
        };
        settings.save(&path).unwrap();
        assert_eq!(Settings::load(&path), settings);

        // Half a file keeps what it says and defaults the rest.
        std::fs::write(&path, r#"{"terminal": {"font_size": 99, "bell": "off"}}"#).unwrap();
        let loaded = Settings::load(&path);
        assert_eq!(loaded.appearance, Appearance::System);
        assert_eq!(loaded.terminal.font_size, TerminalSettings::MAX_FONT_SIZE);
        assert_eq!(loaded.terminal.bell, Bell::Off);
        assert_eq!(loaded.terminal.scrollback, 10_000);

        std::fs::write(&path, "nonsense").unwrap();
        assert_eq!(Settings::load(&path), Settings::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_terminal_values_in_range() {
        let wild = TerminalSettings {
            font_family: "  Fira Code ".into(),
            font_size: f32::NAN,
            scrollback: 10_000_000,
            ..TerminalSettings::default()
        }
        .clamped();
        assert_eq!(wild.font_family, "Fira Code");
        assert_eq!(wild.font_size, 13.5);
        assert_eq!(wild.scrollback, TerminalSettings::MAX_SCROLLBACK);
        let tiny = TerminalSettings {
            font_size: 2.,
            ..TerminalSettings::default()
        };
        assert_eq!(tiny.clamped().font_size, TerminalSettings::MIN_FONT_SIZE);
        let schemed = |name: &str| {
            TerminalSettings {
                colors: Some(name.into()),
                ..TerminalSettings::default()
            }
            .clamped()
            .colors
        };
        assert_eq!(schemed("dracula").as_deref(), Some("Dracula"));
        assert_eq!(schemed("Something else"), None);
    }

    #[test]
    fn zooms_a_point_at_a_time_within_range() {
        let mut terminal = TerminalSettings::default();
        terminal.zoom(Some(1));
        assert_eq!(terminal.font_size, 14.5);
        terminal.zoom(Some(-3));
        assert_eq!(terminal.font_size, 11.5);
        terminal.zoom(None);
        assert_eq!(terminal.font_size, 13.5);
        for _ in 0..40 {
            terminal.zoom(Some(1));
        }
        assert_eq!(terminal.font_size, TerminalSettings::MAX_FONT_SIZE);
        for _ in 0..40 {
            terminal.zoom(Some(-1));
        }
        assert_eq!(terminal.font_size, TerminalSettings::MIN_FONT_SIZE);
    }
}
