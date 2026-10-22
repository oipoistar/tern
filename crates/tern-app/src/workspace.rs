//! The window's root: a header with tabs, the active screen (vault or a
//! terminal session), and the glass layer painted over both.
//!
//! Screens are cached views; only this thin root re-renders each animation
//! frame, so the glass can move without redrawing the screens.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::ssh::{Target, parse_command};

use crate::bench;
use crate::glass::{GlassSlots, glass_layer};
use crate::terminal::{Phase, TerminalView};
use crate::theme::{self, Milk, Tone};
use crate::ui::{Ui, icon, status_dot, tern_mark};
use crate::vault::{VaultContent, VaultEvent};

/// Voxel glass drifts slowly, so 20 fps is plenty; nothing else animates.
const FRAME: Duration = Duration::from_millis(50);

pub struct Workspace {
    vault: Entity<VaultContent>,
    sessions: Vec<Entity<TerminalView>>,
    /// `None` shows the vault; `Some(i)` the i-th session.
    active: Option<usize>,
    /// Glass in the header, which re-registers every frame.
    header_slots: GlassSlots,
    /// Glass on the vault screen, re-registered whenever the vault renders.
    screen_slots: GlassSlots,
    started: Instant,
    _ticker: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let screen_slots = GlassSlots::default();
        let vault = cx.new(|cx| VaultContent::new(screen_slots.clone(), window, cx));
        let connect =
            cx.subscribe_in(
                &vault,
                window,
                |this, _, event: &VaultEvent, window, cx| match event {
                    VaultEvent::Connect { target, identity } => {
                        this.open_session(target.clone(), identity.clone(), window, cx)
                    }
                },
            );
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(FRAME).await;
                // Freeze the glass while another app has focus.
                let alive = this.update_in(cx, |_, window, cx| {
                    if (window.is_window_active() || bench::enabled()) && !cx.reduce_motion() {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let mut workspace = Self {
            vault,
            sessions: Vec::new(),
            active: None,
            header_slots: GlassSlots::default(),
            screen_slots,
            started: Instant::now(),
            _ticker: ticker,
            _subscriptions: vec![connect],
        };
        // `tern --connect "ssh -p 2222 user@host"` opens a session at startup.
        let mut args = std::env::args()
            .skip_while(|arg| arg != "--connect")
            .skip(1);
        if let Some(command) = args.next() {
            match parse_command(&command) {
                Ok((target, identity)) => workspace.open_session(target, identity, window, cx),
                Err(error) => eprintln!("--connect: {error}"),
            }
        }
        workspace
    }

    fn open_session(
        &mut self,
        target: Target,
        identity: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let terminal = cx.new(|cx| TerminalView::new(target, identity, cx));
        // Tabs show each session's state and title.
        self._subscriptions
            .push(cx.observe(&terminal, |_, _, cx| cx.notify()));
        self.sessions.push(terminal);
        self.activate(Some(self.sessions.len() - 1), window, cx);
    }

    fn activate(&mut self, tab: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.active = tab;
        // Surfaces from the previous screen must not linger over the new one.
        self.screen_slots.clear();
        match tab {
            // The cached vault re-renders so its glass registers again.
            None => self.vault.update(cx, |_, cx| cx.notify()),
            Some(index) => {
                self.sessions[index].update(cx, |terminal, cx| terminal.focus(window, cx))
            }
        }
        cx.notify();
    }

    fn close_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        // Dropping the view drops the session's input, which disconnects.
        self.sessions.remove(index);
        let next = match self.active {
            Some(active) if active == index => {
                (!self.sessions.is_empty()).then(|| index.min(self.sessions.len() - 1))
            }
            Some(active) if active > index => Some(active - 1),
            other => other,
        };
        self.activate(next, window, cx);
    }

    fn header(&self, ui: &Ui, cx: &Context<Self>) -> impl IntoElement {
        let theme = ui.theme;
        let dark = theme.dark;
        div()
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(18.))
            .py(px(12.))
            .border_b_1()
            .border_color(theme.line)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        ui.glass("logo", Tone::Brand, Milk::Regular, 11., theme.win, 0., {
                            let mark = theme.glass_text;
                            move || {
                                div()
                                    .size(px(34.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(tern_mark(mark))
                            }
                        }),
                    )
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::BOLD)
                            .child("Tern"),
                    ),
            )
            .child(self.tabs(ui, cx))
            .child(div().flex_1())
            .child(
                ui.glass("synced", Tone::Ok, Milk::Strong, 12., theme.win, 1.4, {
                    let check = theme.ok_glyph;
                    move || {
                        div()
                            .h(px(34.))
                            .px(px(14.))
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(icon(IconName::Check, 14., check))
                            .child("Synced")
                    }
                }),
            )
            .child(
                div()
                    .id("theme-toggle")
                    .size(px(38.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(12.))
                    .border_1()
                    .border_color(theme.line)
                    .bg(theme.card)
                    .shadow(theme.shadow_sm())
                    .cursor_pointer()
                    .child(icon(
                        if dark { IconName::Sun } else { IconName::Moon },
                        18.,
                        theme.text2,
                    ))
                    .on_click(move |_, _, cx| theme::set_dark(!dark, cx)),
            )
    }

    fn tabs(&self, ui: &Ui, cx: &Context<Self>) -> impl IntoElement {
        let theme = ui.theme;
        let tab = |id: SharedString, active: bool| {
            div()
                .id(id)
                .h(px(34.))
                .px(px(12.))
                .flex()
                .items_center()
                .gap(px(8.))
                .rounded(px(12.))
                .text_size(px(13.))
                .cursor_pointer()
                .map(|d| {
                    if active {
                        d.bg(theme.card)
                            .shadow(theme.shadow_sm())
                            .text_color(theme.text)
                            .font_weight(FontWeight::SEMIBOLD)
                    } else {
                        d.text_color(theme.text2)
                    }
                })
        };

        div()
            .flex()
            .items_center()
            .gap(px(4.))
            .p(px(4.))
            .rounded(px(16.))
            .bg(theme.card2)
            .child(
                tab("tab-vault".into(), self.active.is_none())
                    .child(icon(
                        IconName::Server,
                        16.,
                        if self.active.is_none() {
                            theme.text
                        } else {
                            theme.text2
                        },
                    ))
                    .child("Vault")
                    .on_click(cx.listener(|this, _, window, cx| this.activate(None, window, cx))),
            )
            .children(self.sessions.iter().enumerate().map(|(index, session)| {
                let terminal = session.read(cx);
                let glyph = match terminal.phase {
                    Phase::Live => status_dot(theme).into_any_element(),
                    Phase::Ended(_) => div()
                        .size(px(7.))
                        .rounded_full()
                        .border_1()
                        .border_color(theme.text3)
                        .into_any_element(),
                    _ => icon(IconName::LoaderCircle, 12., theme.info_glyph).into_any_element(),
                };
                tab(
                    SharedString::from(format!("tab-{}", session.entity_id())),
                    self.active == Some(index),
                )
                .pr(px(4.))
                .child(glyph)
                .child(div().max_w(px(180.)).truncate().child(terminal.title()))
                .child(
                    div()
                        .id(SharedString::from(format!("close-{}", session.entity_id())))
                        .size(px(24.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(8.))
                        .text_color(theme.text3)
                        .child(icon(IconName::Close, 12., theme.text3))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_session(index, window, cx);
                        })),
                )
                .on_click(
                    cx.listener(move |this, _, window, cx| this.activate(Some(index), window, cx)),
                )
            }))
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        bench::frame_start();
        let time = if cx.reduce_motion() {
            0.
        } else {
            self.started.elapsed().as_secs_f32()
        };
        let theme = theme::current(cx);
        let ui = Ui {
            theme: &theme,
            slots: &self.header_slots,
        };
        let cached = StyleRefinement::default().size_full();
        let body = match self.active {
            None if bench::uncached() => self.vault.clone().into_any_element(),
            None => self.vault.clone().cached(cached).into_any_element(),
            Some(index) => div()
                .size_full()
                .p(px(16.))
                .child(self.sessions[index].clone().cached(cached))
                .into_any_element(),
        };
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme.win)
            .text_color(theme.text)
            .text_size(px(14.))
            .child(self.header(&ui, cx))
            .child(div().flex_1().min_h_0().child(body))
            .child(glass_layer(&[&self.header_slots, &self.screen_slots], time))
    }
}
