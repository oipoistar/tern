//! The settings in effect, shared by every view as a global and saved to
//! `settings.json` on each change, and the Settings dialog that changes
//! them: General, Terminal and Appearance.

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::paths;
use tern_core::settings::{Appearance, Bell, CursorStyle, Settings, TerminalSettings};

use crate::theme::{self, MONO, Theme};
use crate::ui::{button, icon, switch};

/// The settings in effect, and where they're saved (`None` in tests).
struct Current {
    settings: Settings,
    file: Option<PathBuf>,
}

impl Global for Current {}

/// Loads the saved settings into effect.
pub fn init(cx: &mut App) {
    let file = paths::settings();
    cx.set_global(Current {
        settings: Settings::load(&file),
        file: Some(file),
    });
}

/// The settings in effect; the defaults before [`init`].
pub fn current(cx: &App) -> Settings {
    cx.try_global::<Current>()
        .map(|current| current.settings.clone())
        .unwrap_or_default()
}

/// Changes the settings, keeps them in range, saves them and redraws.
pub fn update(cx: &mut App, change: impl FnOnce(&mut Settings)) {
    let file = cx
        .try_global::<Current>()
        .and_then(|current| current.file.clone());
    let mut settings = current(cx);
    change(&mut settings);
    settings.terminal = settings.terminal.clamped();
    if let Some(file) = &file {
        // A setting that can't be saved still applies until Tern quits.
        let _ = settings.save(file);
    }
    cx.set_global(Current { settings, file });
    cx.refresh_windows();
}

/// Light or dark as the settings say, following the system when they say
/// so. `TERN_THEME=light|dark` overrides both, for screenshots.
pub fn apply_appearance(window: &Window, cx: &mut App) {
    let dark = match std::env::var("TERN_THEME").as_deref() {
        Ok("dark") => true,
        Ok("light") => false,
        _ => match current(cx).appearance {
            Appearance::Dark => true,
            Appearance::Light => false,
            Appearance::System => matches!(
                window.appearance(),
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            ),
        },
    };
    theme::set_dark(dark, cx);
}

pub enum SettingsEvent {
    Close,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    General,
    Terminal,
    Appearance,
}

pub struct SettingsView {
    page: Page,
    font_family: Entity<InputState>,
    font_size: Entity<InputState>,
    scrollback: Entity<InputState>,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let terminal = current(cx).terminal;
        let input = |value: String,
                     placeholder: &'static str,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let font_family = input(terminal.font_family.clone(), MONO, window, cx);
        let font_size = input(terminal.font_size.to_string(), "", window, cx);
        let scrollback = input(terminal.scrollback.to_string(), "", window, cx);
        // Typed values apply as soon as they make sense.
        let subscriptions = vec![
            cx.subscribe(&font_family, |_, input, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    let family = input.read(cx).value().to_string();
                    update(cx, |settings| settings.terminal.font_family = family);
                }
            }),
            cx.subscribe(&font_size, |_, input, event: &InputEvent, cx| {
                if let InputEvent::Change = event
                    && let Ok(size) = input.read(cx).value().trim().parse::<f32>()
                    && (TerminalSettings::MIN_FONT_SIZE..=TerminalSettings::MAX_FONT_SIZE)
                        .contains(&size)
                {
                    update(cx, |settings| settings.terminal.font_size = size);
                }
            }),
            cx.subscribe(&scrollback, |_, input, event: &InputEvent, cx| {
                if let InputEvent::Change = event
                    && let Ok(lines) = input.read(cx).value().trim().parse::<usize>()
                {
                    update(cx, |settings| settings.terminal.scrollback = lines);
                }
            }),
        ];
        Self {
            page: Page::Terminal,
            font_family,
            font_size,
            scrollback,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus.focus(window, cx);
    }

    #[cfg(test)]
    pub fn font_size_input(&self) -> Entity<InputState> {
        self.font_size.clone()
    }

    /// A labelled row: the control's name, then the control.
    fn row(label: &'static str, control: impl IntoElement, t: &Theme) -> Div {
        div()
            .min_h(px(48.))
            .flex()
            .items_center()
            .gap(px(16.))
            .child(
                div()
                    .w(px(140.))
                    .flex_none()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.text2)
                    .child(label),
            )
            .child(div().flex_1().min_w_0().flex().child(control))
    }

    fn field(input: &Entity<InputState>, width: f32, t: &Theme) -> Div {
        div()
            .w(px(width))
            .h(px(40.))
            .px(px(10.))
            .flex()
            .items_center()
            .rounded(px(12.))
            .bg(t.card2)
            .font_family(MONO)
            .child(Input::new(input).appearance(false))
    }

    /// Choices side by side in a well, the chosen one raised.
    fn choice<T: Copy + PartialEq + 'static>(
        id: &'static str,
        options: &[(T, &'static str)],
        chosen: T,
        set: fn(&mut Settings, T),
        t: &Theme,
    ) -> Div {
        div()
            .flex()
            .gap(px(4.))
            .p(px(4.))
            .rounded(px(13.))
            .bg(t.card2)
            .children(options.iter().map(|&(value, label)| {
                let on = value == chosen;
                let id = format!("{id}-{}", label.to_lowercase());
                div()
                    .id(SharedString::from(id.clone()))
                    .debug_selector(move || id)
                    .h(px(32.))
                    .px(px(14.))
                    .flex()
                    .items_center()
                    .rounded(px(10.))
                    .text_size(px(13.))
                    .font_weight(if on {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::MEDIUM
                    })
                    .text_color(if on { t.text } else { t.text2 })
                    .when(on, |d| d.bg(t.card).shadow(t.shadow_sm()))
                    .cursor_pointer()
                    .child(label)
                    .on_click(move |_, window, cx| {
                        update(cx, |settings| set(settings, value));
                        apply_appearance(window, cx);
                    })
            }))
    }

    fn page(&self, settings: &Settings, t: &Theme, cx: &Context<Self>) -> Div {
        let terminal = &settings.terminal;
        match self.page {
            Page::General => {
                let data = paths::data_dir();
                div().child(Self::row(
                    "Data folder",
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .font_family(MONO)
                                .text_size(px(12.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis_start()
                                .child(data.display().to_string()),
                        )
                        .child(
                            button("open-data-folder", t)
                                .child(icon(IconName::FolderOpen, 14., t.text))
                                .child("Open")
                                .on_click(move |_, _, cx| cx.reveal_path(&data)),
                        ),
                    t,
                ))
            }
            Page::Terminal => div()
                .flex()
                .flex_col()
                .child(Self::row(
                    "Font",
                    Self::field(&self.font_family, 240., t),
                    t,
                ))
                .child(Self::row("Size", Self::field(&self.font_size, 90., t), t))
                .child(Self::row(
                    "Cursor",
                    Self::choice(
                        "cursor",
                        &[
                            (CursorStyle::Block, "Block"),
                            (CursorStyle::Beam, "Beam"),
                            (CursorStyle::Underline, "Underline"),
                        ],
                        terminal.cursor,
                        |settings, cursor| settings.terminal.cursor = cursor,
                        t,
                    ),
                    t,
                ))
                .child(Self::row(
                    "Scrollback",
                    Self::field(&self.scrollback, 120., t),
                    t,
                ))
                .child(Self::row(
                    "Bell",
                    Self::choice(
                        "bell",
                        &[(Bell::Visual, "Visual"), (Bell::Off, "Off")],
                        terminal.bell,
                        |settings, bell| settings.terminal.bell = bell,
                        t,
                    ),
                    t,
                ))
                .child(Self::row(
                    "Copy on select",
                    switch("copy-on-select", terminal.copy_on_select, t)
                        .debug_selector(|| "copy-on-select".into())
                        .on_click(cx.listener(|_, _, _, cx| {
                            update(cx, |settings| {
                                settings.terminal.copy_on_select = !settings.terminal.copy_on_select
                            })
                        })),
                    t,
                )),
            Page::Appearance => div().child(Self::row(
                "Theme",
                Self::choice(
                    "appearance",
                    &[
                        (Appearance::System, "System"),
                        (Appearance::Light, "Light"),
                        (Appearance::Dark, "Dark"),
                    ],
                    settings.appearance,
                    |settings, appearance| settings.appearance = appearance,
                    t,
                ),
                t,
            )),
        }
    }
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let t = &theme;
        let settings = current(cx);
        let pages = [
            (Page::General, "General", "settings-general"),
            (Page::Terminal, "Terminal", "settings-terminal"),
            (Page::Appearance, "Appearance", "settings-appearance"),
        ];
        div()
            .id("settings")
            .debug_selector(|| "settings".into())
            .key_context("Settings")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|_, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    cx.emit(SettingsEvent::Close);
                    cx.stop_propagation();
                }
            }))
            .w(px(640.))
            .max_w_full()
            .max_h_full()
            .flex()
            .flex_col()
            .gap(px(16.))
            .py(px(20.))
            .px(px(24.))
            .rounded(px(28.))
            .bg(t.win)
            .border_1()
            .border_color(t.line)
            .shadow(t.shadow_lg())
            .text_color(t.text)
            .text_size(px(14.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(20.))
                            .font_weight(FontWeight::BOLD)
                            .child("Settings"),
                    )
                    .child(
                        button("close-settings", t)
                            .debug_selector(|| "close-settings".into())
                            .size(px(40.))
                            .px_0()
                            .justify_center()
                            .child(icon(IconName::Close, 14., t.text2))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::Close))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap(px(4.))
                    .p(px(4.))
                    .rounded(px(16.))
                    .bg(t.card2)
                    .children(pages.into_iter().map(|(page, label, id)| {
                        let on = self.page == page;
                        div()
                            .id(id)
                            .debug_selector(move || id.into())
                            .flex_1()
                            .h(px(36.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(12.))
                            .text_size(px(13.))
                            .font_weight(if on {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::MEDIUM
                            })
                            .text_color(if on { t.text } else { t.text2 })
                            .when(on, |d| d.bg(t.card).shadow(t.shadow_sm()))
                            .cursor_pointer()
                            .child(label)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.page = page;
                                cx.notify();
                            }))
                    })),
            )
            .child(
                div()
                    .p(px(16.))
                    .rounded(px(22.))
                    .bg(t.card)
                    .shadow(t.shadow())
                    .child(self.page(&settings, t, cx)),
            )
    }
}
