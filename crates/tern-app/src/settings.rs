//! The settings in effect, shared by every view as a global and saved to
//! `settings.json` on each change, and the Settings dialog that changes
//! them: General, Terminal and Appearance.

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::paths;
use tern_core::secrets::SYNC_PASSPHRASE;
use tern_core::settings::{Appearance, Bell, CursorStyle, Settings, TerminalSettings};
use tern_core::sync::SyncedSettings;

use crate::sync::{self, Status, SyncState};
use crate::theme::{self, MONO, TermPalette, Theme};
use crate::ui::{button, icon, ink_button, switch};

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
    let before = (settings.appearance, settings.terminal.clone());
    change(&mut settings);
    settings.terminal = settings.terminal.clamped();
    // What syncs carries a change time, so the newest wins.
    if (settings.appearance, settings.terminal.clone()) != before {
        settings.updated = tern_core::host::now_ms();
    }
    if let Some(file) = &file {
        // A setting that can't be saved still applies until Tern quits.
        let _ = settings.save(file);
    }
    cx.set_global(Current { settings, file });
    cx.refresh_windows();
}

/// Takes settings another device synced, with their change time.
pub fn take_synced(cx: &mut App, synced: &SyncedSettings) {
    let file = cx
        .try_global::<Current>()
        .and_then(|current| current.file.clone());
    let mut settings = current(cx);
    settings.appearance = synced.appearance;
    settings.terminal = synced.terminal.clone().clamped();
    settings.updated = synced.updated;
    if let Some(file) = &file {
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
    /// Sync now, after the sync settings changed or on request.
    SyncNow,
}

impl EventEmitter<SettingsEvent> for SettingsView {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    General,
    Terminal,
    Appearance,
    Sync,
}

pub struct SettingsView {
    page: Page,
    font_family: Entity<InputState>,
    font_size: Entity<InputState>,
    scrollback: Entity<InputState>,
    passphrase: Entity<InputState>,
    device: Entity<InputState>,
    /// A sync passphrase is kept in the system's store.
    passphrase_saved: bool,
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(page: Page, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let terminal = current(cx).terminal;
        let device_name = current(cx).sync.device;
        let passphrase_saved = sync::secrets(cx).load(SYNC_PASSPHRASE).is_some();
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
        let passphrase = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(if passphrase_saved {
                    "••••••••"
                } else {
                    "Passphrase"
                })
                .masked(true)
        });
        let device = input(device_name, "", window, cx);
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
            cx.subscribe(&device, |_, input, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    let name = input.read(cx).value().trim().to_string();
                    if !name.is_empty() {
                        update(cx, |settings| settings.sync.device = name);
                    }
                }
            }),
            cx.subscribe(&passphrase, |this: &mut Self, _, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.save_passphrase(cx);
                }
            }),
        ];
        Self {
            page,
            font_family,
            font_size,
            scrollback,
            passphrase,
            device,
            passphrase_saved,
            focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus.focus(window, cx);
    }

    /// Keeps the typed passphrase in the system's store and syncs.
    fn save_passphrase(&mut self, cx: &mut Context<Self>) {
        let typed = self.passphrase.read(cx).value().to_string();
        if typed.is_empty() {
            return;
        }
        if sync::secrets(cx).store(SYNC_PASSPHRASE, &typed).is_ok() {
            self.passphrase_saved = true;
            cx.emit(SettingsEvent::SyncNow);
        }
        cx.notify();
    }

    fn choose_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Sync here".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = chosen.await
                && let Some(folder) = paths.into_iter().next()
            {
                let _ = this.update(cx, |_, cx| {
                    update(cx, |settings| {
                        settings.sync.folder = Some(folder);
                        settings.sync.ensure_device_id();
                    });
                    cx.emit(SettingsEvent::SyncNow);
                });
            }
        })
        .detach();
    }

    /// Turns sync off here and forgets the passphrase; the folder and the
    /// other devices keep their files.
    fn stop_syncing(&mut self, cx: &mut Context<Self>) {
        update(cx, |settings| settings.sync.folder = None);
        let _ = sync::secrets(cx).forget(SYNC_PASSPHRASE);
        self.passphrase_saved = false;
        sync::set_state(cx, SyncState::default());
        cx.notify();
    }

    fn sync_page(&self, settings: &Settings, t: &Theme, cx: &Context<Self>) -> Div {
        let sync_settings = &settings.sync;
        let state = sync::state(cx);
        let folder = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.))
            .when_some(sync_settings.folder.clone(), |d, folder| {
                d.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_family(MONO)
                        .text_size(px(12.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis_start()
                        .child(folder.display().to_string()),
                )
            })
            .child(
                button("sync-choose", t)
                    .debug_selector(|| "sync-choose".into())
                    .child(icon(IconName::FolderOpen, 14., t.text))
                    .child("Choose")
                    .on_click(cx.listener(|this, _, window, cx| this.choose_folder(window, cx))),
            );
        let passphrase = div()
            .flex()
            .items_center()
            .gap(px(8.))
            .child(Self::field(&self.passphrase, 220., t))
            .child(
                button("sync-save-passphrase", t)
                    .debug_selector(|| "sync-save-passphrase".into())
                    .child("Save")
                    .on_click(cx.listener(|this, _, _, cx| this.save_passphrase(cx))),
            );
        let switch_row =
            |label: &'static str, id: &'static str, on: bool, set: fn(&mut Settings)| {
                Self::row(
                    label,
                    switch(id, on, t)
                        .debug_selector(move || id.into())
                        .on_click(move |_, _, cx| update(cx, set)),
                    t,
                )
            };
        let status: AnyElement = match &state.status {
            Status::Off => div().into_any_element(),
            Status::Syncing => icon(IconName::LoaderCircle, 14., t.text2).into_any_element(),
            Status::Synced(at) => div()
                .text_size(px(13.))
                .text_color(t.text2)
                .child(format!("Synced {}", sync::ago(*at)))
                .into_any_element(),
            Status::Failed(reason) => div()
                .text_size(px(13.))
                .text_color(t.danger)
                .truncate()
                .child(reason.clone())
                .into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .child(Self::row("Folder", folder, t))
            .child(Self::row("Passphrase", passphrase, t))
            .when(sync_settings.folder.is_some(), |d| {
                d.child(Self::row(
                    "This device",
                    Self::field(&self.device, 220., t),
                    t,
                ))
                .child(switch_row(
                    "Hosts and groups",
                    "sync-hosts",
                    sync_settings.hosts,
                    |settings| settings.sync.hosts = !settings.sync.hosts,
                ))
                .child(switch_row(
                    "Snippets",
                    "sync-snippets",
                    sync_settings.snippets,
                    |settings| settings.sync.snippets = !settings.sync.snippets,
                ))
                .child(switch_row(
                    "Port forwards",
                    "sync-forwards",
                    sync_settings.forwards,
                    |settings| settings.sync.forwards = !settings.sync.forwards,
                ))
                .child(switch_row(
                    "Settings",
                    "sync-settings",
                    sync_settings.settings,
                    |settings| settings.sync.settings = !settings.sync.settings,
                ))
                .when(!state.devices.is_empty(), |d| {
                    d.child(Self::row(
                        "Devices",
                        div().flex_1().flex().flex_col().gap(px(4.)).children(
                            state.devices.iter().map(|(name, written)| {
                                div()
                                    .flex()
                                    .gap(px(10.))
                                    .text_size(px(13.))
                                    .child(div().flex_1().min_w_0().truncate().child(name.clone()))
                                    .child(div().text_color(t.text2).child(sync::ago(*written)))
                            }),
                        ),
                        t,
                    ))
                })
                .child(
                    div()
                        .pt(px(10.))
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(div().flex_1().min_w_0().child(status))
                        .child(
                            div()
                                .id("sync-stop")
                                .debug_selector(|| "sync-stop".into())
                                .h(px(36.))
                                .px(px(8.))
                                .flex()
                                .items_center()
                                .text_size(px(13.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(t.danger)
                                .cursor_pointer()
                                .child("Stop syncing")
                                .on_click(cx.listener(|this, _, _, cx| this.stop_syncing(cx))),
                        )
                        .child(
                            ink_button("sync-now", t)
                                .debug_selector(|| "sync-now".into())
                                .child(icon(IconName::RefreshCw, 14., t.card))
                                .child("Sync now")
                                .on_click(
                                    cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::SyncNow)),
                                ),
                        ),
                )
            })
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

    /// Each terminal colour scheme as a swatch of itself: its background,
    /// its name in its own text colour, a row of its colours. Tern's own,
    /// which follows the app's look, comes first.
    fn schemes(chosen: Option<&str>, t: &Theme) -> Div {
        let swatches = std::iter::once(("Tern", None, TermPalette::for_theme(t))).chain(
            tern_core::schemes::SCHEMES.iter().map(|scheme| {
                (
                    scheme.name,
                    Some(scheme.name),
                    TermPalette::for_scheme(scheme),
                )
            }),
        );
        div()
            .flex()
            .flex_wrap()
            .gap(px(8.))
            .py(px(6.))
            .children(swatches.map(|(name, value, palette)| {
                let on = chosen == value;
                let id = format!("colors-{}", name.to_lowercase().replace(' ', "-"));
                div()
                    .id(SharedString::from(id.clone()))
                    .debug_selector(move || id)
                    .w(px(122.))
                    .h(px(56.))
                    .p(px(9.))
                    .flex()
                    .flex_col()
                    .justify_between()
                    .rounded(px(12.))
                    .bg(palette.background)
                    .border_2()
                    .border_color(if on { t.text } else { t.line })
                    .cursor_pointer()
                    .child(
                        div()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(palette.foreground)
                            .truncate()
                            .child(name),
                    )
                    .child(
                        div().flex().gap(px(4.)).children((1..7).map(|index| {
                            div().size(px(8.)).rounded_full().bg(palette.ansi[index])
                        })),
                    )
                    .on_click(move |_, _, cx| {
                        update(cx, |settings| {
                            settings.terminal.colors = value.map(String::from)
                        })
                    })
            }))
    }

    fn page(&self, settings: &Settings, t: &Theme, cx: &Context<Self>) -> Div {
        let terminal = &settings.terminal;
        match self.page {
            Page::General => {
                let data = paths::data_dir();
                div()
                    .child(Self::row(
                        "Reopen tabs",
                        switch("reopen-tabs", settings.reopen_tabs, t)
                            .debug_selector(|| "reopen-tabs".into())
                            .on_click(cx.listener(|_, _, _, cx| {
                                update(cx, |settings| settings.reopen_tabs = !settings.reopen_tabs)
                            })),
                        t,
                    ))
                    .child(Self::row(
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
                    "Colours",
                    Self::schemes(terminal.colors.as_deref(), t),
                    t,
                ))
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
                    "Confirm pastes",
                    switch("confirm-paste", terminal.confirm_paste, t)
                        .debug_selector(|| "confirm-paste".into())
                        .on_click(cx.listener(|_, _, _, cx| {
                            update(cx, |settings| {
                                settings.terminal.confirm_paste = !settings.terminal.confirm_paste
                            })
                        })),
                    t,
                ))
                .child(Self::row(
                    "Predict typing",
                    switch("predict-typing", terminal.predict_typing, t)
                        .debug_selector(|| "predict-typing".into())
                        .on_click(cx.listener(|_, _, _, cx| {
                            update(cx, |settings| {
                                settings.terminal.predict_typing = !settings.terminal.predict_typing
                            })
                        })),
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
            Page::Sync => self.sync_page(settings, t, cx),
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
            (Page::Sync, "Sync", "settings-sync"),
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
