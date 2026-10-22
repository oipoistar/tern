//! The window's root: a header with tabs, the active screen (vault or a
//! terminal session, with its snippet bar), and the live layer painted over
//! it.
//!
//! Screens are cached views; only this thin root re-renders each animation
//! frame, so the latency traces can move without redrawing the screens.

use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::ssh::{Connection, parse_command};

use crate::bench;
use crate::live::{LiveSlots, live_layer};
use crate::terminal::{Phase, TerminalEvent, TerminalView};
use crate::theme::{self, Theme};
use crate::ui::{button, icon, status_dot, tern_mark};
use crate::vault::{VaultContent, VaultEvent};

/// Latency traces scroll slowly, so 20 fps is plenty; nothing else animates.
const FRAME: Duration = Duration::from_millis(50);

pub struct Workspace {
    vault: Entity<VaultContent>,
    sessions: Vec<Entity<TerminalView>>,
    /// `None` shows the vault; `Some(i)` the i-th session.
    active: Option<usize>,
    /// Moving marks on the vault screen, re-registered whenever it renders.
    live: LiveSlots,
    started: Instant,
    _ticker: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let live = LiveSlots::default();
        let vault = cx.new(|cx| VaultContent::open(live.clone(), window, cx));
        let mut workspace = Self::with(vault, live, window, cx);
        // `tern --connect "ssh -p 2222 user@host"` opens a session at startup.
        let mut args = std::env::args()
            .skip_while(|arg| arg != "--connect")
            .skip(1);
        if let Some(command) = args.next() {
            match parse_command(&command) {
                Ok((target, identity)) => {
                    workspace.open_session(Connection::typed(target, identity), window, cx)
                }
                Err(error) => eprintln!("--connect: {error}"),
            }
        }
        // `tern --edit [host]` opens the host editor (a new host without a
        // name), for screenshots and checks.
        let mut args = std::env::args().skip_while(|arg| arg != "--edit");
        if args.next().is_some() {
            let host = args.next().filter(|arg| !arg.starts_with("--"));
            workspace
                .vault
                .update(cx, |vault, cx| vault.open_editor(host, window, cx));
        }
        workspace
    }

    /// A window around `vault`, whose live marks register in `live`.
    fn with(
        vault: Entity<VaultContent>,
        live: LiveSlots,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let connect =
            cx.subscribe_in(
                &vault,
                window,
                |this, _, event: &VaultEvent, window, cx| match event {
                    VaultEvent::Connect(connection) => {
                        this.open_session(connection.clone(), window, cx)
                    }
                    VaultEvent::Forward { host, forward, on } => {
                        for session in &this.sessions {
                            let terminal = session.read(cx);
                            if terminal.is_live() && terminal.host() == Some(host.as_str()) {
                                terminal.set_forward(forward.clone(), *on);
                            }
                        }
                    }
                },
            );
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(FRAME).await;
                // Redraw only while something moves: not on a terminal
                // tab, and frozen while another app has focus or motion is
                // reduced. The benchmark draws regardless.
                let alive = this.update_in(cx, |this, window, cx| {
                    let moving = window.is_window_active() && !cx.reduce_motion();
                    if (moving || bench::enabled()) && !this.live.is_empty() {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        Self {
            vault,
            sessions: Vec::new(),
            active: None,
            live,
            started: Instant::now(),
            _ticker: ticker,
            _subscriptions: vec![connect],
        }
    }

    fn open_session(
        &mut self,
        connection: Connection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let terminal = cx.new(|cx| TerminalView::new(connection, cx));
        self.add_session(terminal, window, cx);
    }

    /// Adds a tab for `terminal` and shows it.
    fn add_session(
        &mut self,
        terminal: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Tabs show each session's state and title.
        self._subscriptions
            .push(cx.observe(&terminal, |_, _, cx| cx.notify()));
        self._subscriptions.push(
            cx.subscribe(
                &terminal,
                |this, _, event: &TerminalEvent, cx| match event {
                    TerminalEvent::Finished(visit) => {
                        let visit = visit.clone();
                        this.vault.update(cx, |vault, cx| {
                            if let Some(host) = &visit.host {
                                vault.clear_forward_states(host, cx);
                            }
                            vault.record_visit(visit, cx);
                        });
                    }
                    TerminalEvent::Forwarding {
                        host,
                        forward,
                        state,
                    } => {
                        let (host, forward, state) = (host.clone(), forward.clone(), state.clone());
                        this.vault.update(cx, |vault, cx| {
                            vault.set_forward_state(host, &forward, state, cx)
                        });
                    }
                },
            ),
        );
        self.sessions.push(terminal);
        self.activate(Some(self.sessions.len() - 1), window, cx);
    }

    fn activate(&mut self, tab: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.active = tab;
        // Marks from the previous screen must not linger over the new one.
        self.live.clear();
        self.vault
            .update(cx, |vault, _| vault.set_showing(tab.is_none()));
        match tab {
            // The cached vault re-renders so its marks register again, with
            // art for any key trusted in a session meanwhile.
            None => self.vault.update(cx, |vault, cx| {
                vault.refresh_keys();
                cx.notify();
            }),
            Some(index) => {
                self.sessions[index].update(cx, |terminal, cx| terminal.focus(window, cx))
            }
        }
        cx.notify();
    }

    /// Types a saved snippet into the session on screen.
    fn run_snippet(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(active), Some(snippet)) = (
            self.active,
            self.vault.read(cx).snippets().get(index).cloned(),
        ) else {
            return;
        };
        self.sessions[active].update(cx, |terminal, cx| {
            terminal.type_text(snippet.keystrokes(), cx);
            terminal.focus(window, cx);
        });
    }

    /// Goes to the vault's Snippets screen with a new snippet's form open.
    fn new_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.activate(None, window, cx);
        self.vault
            .update(cx, |vault, cx| vault.new_snippet(window, cx));
    }

    /// Under the terminal: a chip per snippet, then a dashed one to add
    /// another.
    fn snippet_bar(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let names: Vec<String> = self
            .vault
            .read(cx)
            .snippets()
            .iter()
            .map(|snippet| snippet.name.clone())
            .collect();
        div()
            .pt(px(12.))
            .flex()
            .flex_wrap()
            .gap(px(8.))
            .children(names.into_iter().enumerate().map(|(index, name)| {
                let id = format!("snippet-{index}");
                button(SharedString::from(id.clone()), theme)
                    .debug_selector(move || id)
                    .h(px(32.))
                    .rounded(px(10.))
                    .child(name)
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.run_snippet(index, window, cx)),
                    )
            }))
            .child(
                button("new-snippet", theme)
                    .debug_selector(|| "new-snippet".into())
                    .h(px(32.))
                    .rounded(px(10.))
                    .border_dashed()
                    .border_color(theme.text3.opacity(0.5))
                    .bg(transparent_black())
                    .shadow(Vec::new())
                    .text_color(theme.text2)
                    .child(icon(IconName::Plus, 13., theme.text2))
                    .child("New snippet")
                    .on_click(cx.listener(|this, _, window, cx| this.new_snippet(window, cx))),
            )
    }

    fn close_session(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        // A live session closed by hand still goes into History.
        let terminal = self.sessions[index].read(cx);
        if terminal.is_live() {
            let visit = terminal.visit("Closed");
            self.vault.update(cx, |vault, cx| {
                if let Some(host) = &visit.host {
                    vault.clear_forward_states(host, cx);
                }
                vault.record_visit(visit, cx);
            });
        }
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

    fn header(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
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
                    .pl(px(4.))
                    .pr(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(tern_mark(theme.text))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::BOLD)
                            .child("Tern"),
                    ),
            )
            .child(self.tabs(theme, cx))
            .child(div().flex_1())
            // A state, not a control, so it gets no surface of its own.
            .child(
                div()
                    .h(px(34.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .text_size(px(13.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text2)
                    .child(icon(IconName::Check, 14., theme.text))
                    .child("Synced"),
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

    fn tabs(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
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
                    _ => icon(IconName::LoaderCircle, 12., theme.text2).into_any_element(),
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
        let time = if cx.reduce_motion() && !bench::enabled() {
            0.
        } else {
            self.started.elapsed().as_secs_f32()
        };
        let theme = theme::current(cx);
        let cached = StyleRefinement::default().size_full();
        let body = match self.active {
            None if bench::uncached() => self.vault.clone().into_any_element(),
            None => self.vault.clone().cached(cached).into_any_element(),
            Some(index) => div()
                .size_full()
                .p(px(16.))
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .child(self.sessions[index].clone().cached(cached)),
                )
                .child(self.snippet_bar(&theme, cx))
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
            .child(self.header(&theme, cx))
            .child(div().flex_1().min_h_0().child(body))
            .child(live_layer(&self.live, time))
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports: `gpui_kit::*` would shadow Rust's `#[test]`.
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use gpui_kit::AppContext as _;
    use tern_core::Vault;

    use super::Workspace;
    use crate::live::LiveSlots;
    use crate::terminal::TerminalView;
    use crate::vault::VaultContent;

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing on screen is tagged {selector}"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn the_snippet_bar_types_into_the_session(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            let live = LiveSlots::default();
            let vault =
                cx.new(|cx| VaultContent::new(Vault::sample(), None, live.clone(), window, cx));
            let mut workspace = Workspace::with(vault, live, window, cx);
            let terminal = cx.new(TerminalView::offline);
            workspace.add_session(terminal, window, cx);
            workspace
        });

        click(cx, "snippet-0");
        let sent = workspace.read_with(cx, |workspace, cx| {
            workspace.sessions[0].read(cx).sent.borrow().clone()
        });
        assert_eq!(sent, b"df -h\r");

        // New snippet goes to the vault's Snippets screen, form open.
        click(cx, "new-snippet");
        workspace.read_with(cx, |workspace, _| assert_eq!(workspace.active, None));
        assert!(cx.debug_bounds("save-snippet").is_some());
    }
}
