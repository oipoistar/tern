//! The window's root: a header with tabs, the active screen (the vault, or a
//! session tab of one to four terminal panes with a toolbar and a snippet
//! bar), and the live layer painted over it.
//!
//! Screens are cached views; only this thin root re-renders each animation
//! frame, so the latency traces can move without redrawing the screens.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState, MoveDown, MoveUp};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::HostStatus;
use tern_core::restore::{OpenTabs, Opened};
use tern_core::snippet::Snippet;
use tern_core::ssh::{Connection, parse_command};

use crate::bench;
use crate::files::FilesPanel;
use crate::live::{LiveSlots, live_layer};
use crate::marks;
use crate::settings::{self, Page, SettingsEvent, SettingsView};
use crate::sync::{self, Status};
use crate::terminal::{Phase, TerminalEvent, TerminalView};
use crate::theme::{self, MONO, Theme};
use crate::ui::{button, icon, ink_button, status_dot, tern_mark};
use crate::vault::{VaultContent, VaultEvent};

/// Latency traces scroll slowly, so 20 fps is plenty; nothing else animates.
const FRAME: Duration = Duration::from_millis(50);
/// Panes a tab holds at most: the grid of four.
const MAX_PANES: usize = 4;

/// Makes a terminal pane for a connection: [`TerminalView::new`], or one
/// that doesn't connect, in tests.
type NewTerminal = fn(Connection, &mut Context<TerminalView>) -> TerminalView;

/// What the window's own keyboard shortcuts do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortcut {
    /// Ctrl+Shift+T (Cmd+T on macOS): a local terminal tab.
    NewLocal,
    /// Ctrl+Tab and Ctrl+Shift+Tab: the next and previous tab, the vault
    /// included.
    NextTab,
    PreviousTab,
    /// Ctrl+Shift+P (Cmd+Shift+P on macOS): the command palette.
    Palette,
}

/// The window shortcut `keystroke` is, if any. Panes leave these alone.
pub fn shortcut(keystroke: &Keystroke) -> Option<Shortcut> {
    let m = &keystroke.modifiers;
    match keystroke.key.as_str() {
        "t" if (m.control && m.shift && !m.alt) || (m.platform && !m.shift) => {
            Some(Shortcut::NewLocal)
        }
        "p" if m.shift && !m.alt && (m.control || m.platform) => Some(Shortcut::Palette),
        "tab" if m.control && !m.alt => Some(if m.shift {
            Shortcut::PreviousTab
        } else {
            Shortcut::NextTab
        }),
        _ => None,
    }
}

/// A session tab: one to four panes, laid out by how many there are
/// (single, side by side, main plus stack, grid of four).
struct Tab {
    panes: Vec<Entity<TerminalView>>,
    /// The pane typing goes to.
    focused: usize,
    /// One pane filling the tab while the others keep running behind it.
    maximized: Option<usize>,
    /// Typing in any pane goes to every pane.
    broadcast: bool,
    /// The Files panel, made when first opened, on the pane focused then.
    files: Option<Entity<FilesPanel>>,
    /// The pane whose host the Files panel shows.
    files_for: Option<Entity<TerminalView>>,
    files_open: bool,
}

impl Tab {
    fn focused(&self) -> &Entity<TerminalView> {
        &self.panes[self.focused]
    }
}

/// A square outlined button in the header, holding an icon.
fn header_button(id: &'static str, glyph: IconName, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .debug_selector(move || id.into())
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
        .child(icon(glyph, 18., theme.text2))
}

/// What a command palette entry does.
#[derive(Clone)]
enum Command {
    /// Connect to the saved host.
    Connect(String),
    /// Show the tab (`None` is the vault).
    Tab(Option<usize>),
    /// Type the snippet into the focused pane.
    Snippet(usize),
    NewLocal,
    Settings,
    /// Show a vault screen, by `--show` name.
    Screen(&'static str),
    Theme,
}

/// A command palette entry.
#[derive(Clone)]
struct Entry {
    glyph: IconName,
    label: SharedString,
    detail: Option<SharedString>,
    command: Command,
}

/// The command palette: a field and the entries that match it.
struct Palette {
    input: Entity<InputState>,
    selected: usize,
    /// What had the keyboard before, given it back on closing.
    restore: Option<FocusHandle>,
    _subscription: Subscription,
}

/// Entries the palette shows at most.
const PALETTE_ROWS: usize = 8;

/// A snippet waiting for its `{{name}}` values before it's typed.
struct Filling {
    snippet: Snippet,
    /// The pane it was asked for in; it's typed into that pane's tab.
    pane: Entity<TerminalView>,
    /// Each variable's name and field.
    fields: Vec<(String, Entity<InputState>)>,
    _subscriptions: Vec<Subscription>,
}

pub struct Workspace {
    vault: Entity<VaultContent>,
    filling: Option<Filling>,
    palette: Option<Palette>,
    tabs: Vec<Tab>,
    /// `None` shows the vault; `Some(i)` the i-th session tab.
    active: Option<usize>,
    /// Moving marks on the vault screen, re-registered whenever it renders.
    live: LiveSlots,
    started: Instant,
    new_terminal: NewTerminal,
    /// The Settings dialog, while open.
    settings: Option<Entity<SettingsView>>,
    _settings_events: Option<Subscription>,
    /// Where the open tabs are written down for the next start; not for
    /// tests, or a start with flags.
    tabs_file: Option<PathBuf>,
    /// The active tab and each tab's panes when they were last written.
    remembered: (Option<usize>, Vec<Vec<EntityId>>),
    _ticker: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let live = LiveSlots::default();
        let vault = cx.new(|cx| VaultContent::open(live.clone(), window, cx));
        let mut workspace = Self::with(vault, live, TerminalView::new, window, cx);
        // A plain start opens the tabs open when Tern last quit, and from
        // then on keeps track of them. Flags are for checks and leave both
        // alone.
        if !std::env::args().skip(1).any(|arg| arg.starts_with("--")) {
            let file = tern_core::paths::tabs();
            if settings::current(cx).reopen_tabs {
                workspace.reopen(&OpenTabs::load(&file), window, cx);
            }
            workspace.remember_tabs_in(file, cx);
        }
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
        // `tern --local` opens a local terminal at startup.
        if std::env::args().any(|arg| arg == "--local") {
            workspace.open_local(window, cx);
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
        // `tern --settings [general|terminal|appearance|sync]` opens
        // Settings at that page, likewise.
        let mut args = std::env::args().skip_while(|arg| arg != "--settings");
        if args.next().is_some() {
            let page = match args.next().as_deref() {
                Some("general") => Page::General,
                Some("appearance") => Page::Appearance,
                Some("sync") => Page::Sync,
                _ => Page::Terminal,
            };
            workspace.open_settings(page, window, cx);
        }
        // `tern --show [keychain|identities|snippets|forwarding|known-hosts|history]`
        // opens the vault at that screen.
        let mut args = std::env::args().skip_while(|arg| arg != "--show");
        if args.next().is_some()
            && let Some(screen) = args.next()
        {
            workspace
                .vault
                .update(cx, |vault, cx| vault.show_named(&screen, window, cx));
        }
        // `tern --palette [query]` opens the command palette, likewise.
        let mut args = std::env::args().skip_while(|arg| arg != "--palette");
        if args.next().is_some() {
            workspace.open_palette(window, cx);
            if let (Some(query), Some(palette)) = (
                args.next().filter(|arg| !arg.starts_with("--")),
                &workspace.palette,
            ) {
                palette
                    .input
                    .update(cx, |input, cx| input.set_value(query, window, cx));
            }
        }
        workspace
    }

    /// A window around `vault`, whose live marks register in `live`, making
    /// panes with `new_terminal`.
    fn with(
        vault: Entity<VaultContent>,
        live: LiveSlots,
        new_terminal: NewTerminal,
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
                    VaultEvent::Open { connection, files } => {
                        this.show_session(connection.clone(), *files, window, cx)
                    }
                    VaultEvent::OpenTogether(connections) => {
                        this.open_together(connections.clone(), window, cx)
                    }
                    VaultEvent::Forward { host, forward, on } => {
                        for pane in this.tabs.iter().flat_map(|tab| &tab.panes) {
                            let terminal = pane.read(cx);
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
        // With the system's appearance chosen, follow it as it changes.
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            settings::apply_appearance(window, cx)
        });
        let focus_lost =
            cx.on_focus_lost(window, |this, window, cx| this.restore_focus(window, cx));
        Self {
            vault,
            filling: None,
            palette: None,
            tabs: Vec::new(),
            active: None,
            live,
            started: Instant::now(),
            new_terminal,
            settings: None,
            _settings_events: None,
            tabs_file: None,
            remembered: (None, Vec::new()),
            _ticker: ticker,
            _subscriptions: vec![connect, appearance, focus_lost],
        }
    }

    /// Opens `connections` side by side, four panes to a tab, and shows
    /// the first of those tabs.
    fn open_together(
        &mut self,
        connections: Vec<Connection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let first = self.tabs.len();
        for group in connections.chunks(4) {
            self.push_tab(group, cx);
        }
        if self.tabs.len() > first {
            self.activate(Some(first), window, cx);
        }
    }

    /// A tab at the end with a pane for each connection.
    fn push_tab(&mut self, connections: &[Connection], cx: &mut Context<Self>) {
        let new_terminal = self.new_terminal;
        let panes: Vec<Entity<TerminalView>> = connections
            .iter()
            .map(|connection| cx.new(|cx| new_terminal(connection.clone(), cx)))
            .collect();
        for pane in &panes {
            self.watch(pane, cx);
        }
        self.tabs.push(Tab {
            panes,
            focused: 0,
            maximized: None,
            broadcast: false,
            files: None,
            files_for: None,
            files_open: false,
        });
    }

    /// Opens the tabs written down at the last quit; hosts deleted since
    /// are left out.
    fn reopen(&mut self, open: &OpenTabs, window: &mut Window, cx: &mut Context<Self>) {
        let first = self.tabs.len();
        let (tabs, active) = self.vault.read(cx).reopened(open);
        for connections in &tabs {
            self.push_tab(connections, cx);
        }
        self.activate(active.map(|active| first + active), window, cx);
    }

    /// Keeps `file` up to date with the open tabs from now on.
    fn remember_tabs_in(&mut self, file: PathBuf, cx: &mut Context<Self>) {
        self.tabs_file = Some(file);
        cx.observe_self(|this, cx| this.remember_tabs(cx)).detach();
    }

    /// Writes the open tabs down when they've changed, for the next start.
    /// Runs whenever the window changes, so it compares without
    /// allocating first.
    fn remember_tabs(&mut self, cx: &mut Context<Self>) {
        let Some(file) = &self.tabs_file else {
            return;
        };
        let (active, panes) = &self.remembered;
        let same = *active == self.active
            && panes.len() == self.tabs.len()
            && self.tabs.iter().zip(panes).all(|(tab, ids)| {
                tab.panes.len() == ids.len()
                    && tab
                        .panes
                        .iter()
                        .zip(ids)
                        .all(|(pane, id)| pane.entity_id() == *id)
            });
        if same {
            return;
        }
        let open = OpenTabs {
            tabs: self
                .tabs
                .iter()
                .map(|tab| {
                    tab.panes
                        .iter()
                        .map(|pane| Opened::of(pane.read(cx).connection()))
                        .collect()
                })
                .collect(),
            active: self.active,
        };
        // Not written down, the tabs only don't come back.
        let _ = open.save(file);
        self.remembered = (
            self.active,
            self.tabs
                .iter()
                .map(|tab| tab.panes.iter().map(|pane| pane.entity_id()).collect())
                .collect(),
        );
    }

    /// Gives the keyboard somewhere to go when what had it is gone (a closed
    /// form or row): the palette's field, Settings, the tab's pane, or the
    /// vault. Keys reaching no view would also miss the window's shortcuts.
    fn restore_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(palette) = &self.palette {
            palette
                .input
                .update(cx, |input, cx| input.focus(window, cx));
        } else if let Some(settings) = &self.settings {
            settings.update(cx, |settings, cx| settings.focus(window, cx));
        } else if let Some(index) = self.active {
            self.tabs[index]
                .focused()
                .update(cx, |pane, cx| pane.focus(window, cx));
        } else {
            self.vault.update(cx, |vault, cx| vault.focus(window, cx));
        }
    }

    /// Opens the command palette, its field focused.
    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.is_some() {
            return;
        }
        let restore = window.focused(cx);
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        input.update(cx, |input, cx| input.focus(window, cx));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    if let Some(palette) = &mut this.palette {
                        palette.selected = 0;
                    }
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.run_selected(window, cx),
                _ => {}
            },
        );
        self.palette = Some(Palette {
            input,
            selected: 0,
            restore,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Closes the palette, the keyboard back where it was.
    fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(restore) = self.palette.take().and_then(|palette| palette.restore) {
            window.focus(&restore, cx);
        }
        cx.notify();
    }

    /// Everything the palette offers: hosts, open tabs, snippets, then
    /// the window's own actions.
    fn entries(&self, cx: &App) -> Vec<Entry> {
        let vault = self.vault.read(cx);
        let mut entries: Vec<Entry> = vault
            .host_entries()
            .into_iter()
            .map(|(name, target)| Entry {
                glyph: IconName::Server,
                label: name.clone().into(),
                detail: Some(target.into()),
                command: Command::Connect(name),
            })
            .collect();
        entries.push(Entry {
            glyph: IconName::LayoutGrid,
            label: "Vault".into(),
            detail: None,
            command: Command::Tab(None),
        });
        for (index, tab) in self.tabs.iter().enumerate() {
            let pane = tab.focused().read(cx);
            entries.push(Entry {
                glyph: IconName::SquareTerminal,
                label: pane.title(),
                detail: Some(pane.label()),
                command: Command::Tab(Some(index)),
            });
        }
        if self.active.is_some() {
            for (index, snippet) in vault.snippets().iter().enumerate() {
                entries.push(Entry {
                    glyph: IconName::CodeXml,
                    label: snippet.name.clone().into(),
                    detail: Some(snippet.command.clone().into()),
                    command: Command::Snippet(index),
                });
            }
        }
        let action = |glyph, label: &'static str, command| Entry {
            glyph,
            label: label.into(),
            detail: None,
            command,
        };
        entries.extend([
            action(
                IconName::SquareTerminal,
                "New local terminal",
                Command::NewLocal,
            ),
            action(IconName::SlidersHorizontal, "Settings", Command::Settings),
            if theme::current(cx).dark {
                action(IconName::Sun, "Light appearance", Command::Theme)
            } else {
                action(IconName::Moon, "Dark appearance", Command::Theme)
            },
            action(IconName::KeyRound, "Keychain", Command::Screen("keychain")),
            action(
                IconName::KeyRound,
                "Identities",
                Command::Screen("identities"),
            ),
            action(IconName::CodeXml, "Snippets", Command::Screen("snippets")),
            action(
                IconName::ArrowLeftRight,
                "Port forwarding",
                Command::Screen("forwarding"),
            ),
            action(
                IconName::ShieldCheck,
                "Known hosts",
                Command::Screen("known-hosts"),
            ),
            action(IconName::Clock, "History", Command::Screen("history")),
        ]);
        entries
    }

    /// The entries matching what's typed, best first.
    fn matching(&self, query: &str, cx: &App) -> Vec<Entry> {
        let mut scored: Vec<(u32, Entry)> = self
            .entries(cx)
            .into_iter()
            .filter_map(|entry| {
                let label = tern_core::fuzzy::score(query, &entry.label);
                let detail = entry
                    .detail
                    .as_ref()
                    .and_then(|detail| tern_core::fuzzy::score(query, detail))
                    .map(|score| score / 2);
                label.max(detail).map(|score| (score, entry))
            })
            .collect();
        // Stable, so equal scores keep their place: hosts, tabs, snippets.
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        scored
            .into_iter()
            .take(PALETTE_ROWS)
            .map(|(_, entry)| entry)
            .collect()
    }

    fn move_selection(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some(palette) = &self.palette else {
            return;
        };
        let count = self.matching(&palette.input.read(cx).value(), cx).len();
        if let Some(palette) = &mut self.palette
            && count > 0
        {
            palette.selected = (palette.selected as isize + by).rem_euclid(count as isize) as usize;
        }
        cx.notify();
    }

    /// Runs the selected entry and closes the palette.
    fn run_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(palette) = &self.palette else {
            return;
        };
        let entries = self.matching(&palette.input.read(cx).value(), cx);
        let Some(entry) = entries.get(palette.selected).cloned() else {
            return;
        };
        self.close_palette(window, cx);
        self.run_command(entry.command, window, cx);
    }

    fn run_command(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            Command::Connect(name) => self
                .vault
                .update(cx, |vault, cx| vault.open_host(&name, window, cx)),
            Command::Tab(index) => self.activate(index, window, cx),
            Command::Snippet(index) => self.run_snippet(index, window, cx),
            Command::NewLocal => self.open_local(window, cx),
            Command::Settings => self.open_settings(Page::Terminal, window, cx),
            Command::Screen(name) => {
                self.activate(None, window, cx);
                self.vault
                    .update(cx, |vault, cx| vault.show_named(name, window, cx));
            }
            Command::Theme => {
                let dark = theme::current(cx).dark;
                settings::update(cx, |settings| {
                    settings.appearance = if dark {
                        tern_core::settings::Appearance::Light
                    } else {
                        tern_core::settings::Appearance::Dark
                    }
                });
                settings::apply_appearance(window, cx);
            }
        }
    }

    /// The palette over the window: a field, then the entries that match,
    /// the selected one in ink.
    fn palette_view(
        &self,
        palette: &Palette,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let entries = self.matching(&palette.input.read(cx).value(), cx);
        div()
            .id("palette-scrim")
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .pt(px(90.))
            .flex()
            .justify_center()
            .items_start()
            .bg(theme.scrim())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.close_palette(window, cx)),
            )
            .child(
                div()
                    .id("palette")
                    .debug_selector(|| "palette".into())
                    .w(px(560.))
                    .max_w_full()
                    .p(px(10.))
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .rounded(px(20.))
                    .bg(theme.card)
                    .border_1()
                    .border_color(theme.line)
                    .shadow(theme.shadow_lg())
                    .text_color(theme.text)
                    // Clicks inside stay inside.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                        this.move_selection(-1, cx);
                        cx.stop_propagation();
                    }))
                    .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                        this.move_selection(1, cx);
                        cx.stop_propagation();
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                        if event.keystroke.key == "escape" {
                            cx.stop_propagation();
                            this.close_palette(window, cx);
                        }
                    }))
                    .child(
                        div()
                            .h(px(44.))
                            .px(px(12.))
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .rounded(px(13.))
                            .bg(theme.card2)
                            .child(icon(IconName::Search, 16., theme.text2))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(14.))
                                    .child(Input::new(&palette.input).appearance(false)),
                            ),
                    )
                    .children(entries.into_iter().enumerate().map(|(index, entry)| {
                        let on = index == palette.selected;
                        let id = format!("palette-{index}");
                        let (ink, faint) = if on {
                            (theme.card, theme.card.opacity(0.7))
                        } else {
                            (theme.text, theme.text2)
                        };
                        let command = entry.command.clone();
                        div()
                            .id(SharedString::from(id.clone()))
                            .debug_selector(move || id)
                            .h(px(40.))
                            .px(px(12.))
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .rounded(px(11.))
                            .when(on, |d| d.bg(theme.text))
                            .text_color(ink)
                            .cursor_pointer()
                            .child(icon(entry.glyph, 15., ink))
                            .child(
                                div()
                                    .flex_none()
                                    .max_w(px(240.))
                                    .truncate()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(entry.label),
                            )
                            .children(entry.detail.map(|detail| {
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_family(MONO)
                                    .text_size(px(12.))
                                    .text_color(faint)
                                    .child(detail)
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_palette(window, cx);
                                this.run_command(command.clone(), window, cx);
                            }))
                    })),
            )
    }

    /// A tab with a shell on this computer.
    fn open_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_session(Connection::local(), window, cx);
    }

    /// The window's shortcuts, which panes pass up.
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Over the palette or Settings they would act behind it.
        if self.palette.is_some() || self.settings.is_some() {
            return;
        }
        let Some(shortcut) = shortcut(&event.keystroke) else {
            return;
        };
        // The vault, then each tab, round again.
        let count = self.tabs.len() + 1;
        let at = self.active.map_or(0, |index| index + 1);
        let step = |by: usize| match (at + by) % count {
            0 => None,
            next => Some(next - 1),
        };
        match shortcut {
            Shortcut::Palette => self.open_palette(window, cx),
            Shortcut::NewLocal => self.open_local(window, cx),
            Shortcut::NextTab => self.activate(step(1), window, cx),
            Shortcut::PreviousTab => self.activate(step(count - 1), window, cx),
        }
        cx.stop_propagation();
    }

    fn open_session(
        &mut self,
        connection: Connection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let new_terminal = self.new_terminal;
        let terminal = cx.new(|cx| new_terminal(connection, cx));
        self.add_session(terminal, window, cx);
    }

    /// Shows the tab with a session to `connection`'s saved host, or opens
    /// one, then its Files panel when `files`.
    fn show_session(
        &mut self,
        connection: Connection,
        files: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open = connection.host.as_deref().and_then(|host| {
            self.tabs.iter().enumerate().find_map(|(index, tab)| {
                tab.panes
                    .iter()
                    .position(|pane| pane.read(cx).is_live() && pane.read(cx).host() == Some(host))
                    .map(|pane| (index, pane))
            })
        });
        match open {
            // That host's pane takes the keyboard, so Files opens on it.
            Some((index, pane)) => {
                self.tabs[index].focused = pane;
                self.activate(Some(index), window, cx);
            }
            None => self.open_session(connection, window, cx),
        }
        if let Some(index) = self.active
            && files
        {
            self.show_files(index, cx);
        }
    }

    /// Tells the vault which saved hosts have a live session, for its side
    /// panel's previews.
    fn share_sessions(&self, cx: &mut Context<Self>) {
        let sessions: HashMap<String, _> = self
            .tabs
            .iter()
            .flat_map(|tab| &tab.panes)
            .filter_map(|pane| {
                let terminal = pane.read(cx);
                let host = terminal.host().filter(|_| terminal.is_live())?;
                Some((host.to_string(), terminal.preview()))
            })
            .collect();
        self.vault
            .update(cx, |vault, cx| vault.set_open_sessions(sessions, cx));
    }

    /// Adds a tab for `terminal` and shows it.
    fn add_session(
        &mut self,
        terminal: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.watch(&terminal, cx);
        self.tabs.push(Tab {
            panes: vec![terminal],
            focused: 0,
            maximized: None,
            broadcast: false,
            files: None,
            files_for: None,
            files_open: false,
        });
        self.activate(Some(self.tabs.len() - 1), window, cx);
    }

    /// Follows a pane: its state and title for the tab, its endings for
    /// History, its rules for Port forwarding, and its typing for
    /// broadcast.
    fn watch(&mut self, terminal: &Entity<TerminalView>, cx: &mut Context<Self>) {
        self._subscriptions
            .push(cx.observe(terminal, |_, _, cx| cx.notify()));
        self._subscriptions.push(cx.subscribe(
            terminal,
            |this, terminal, event: &TerminalEvent, cx| match event {
                TerminalEvent::Finished(visit) => {
                    let visit = visit.clone();
                    this.vault.update(cx, |vault, cx| {
                        if let Some(host) = &visit.host {
                            vault.clear_forward_states(host, cx);
                        }
                        vault.record_visit(visit, cx);
                    });
                    this.share_sessions(cx);
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
                TerminalEvent::Typed(bytes) => {
                    this.broadcast(&terminal, |pane, cx| pane.type_text(bytes.clone(), cx), cx)
                }
                TerminalEvent::Pasted(text) => {
                    this.broadcast(&terminal, |pane, cx| pane.type_paste(text, cx), cx)
                }
            },
        ));
    }

    /// Gives the other panes of `from`'s tab what it was given, while that
    /// tab broadcasts.
    fn broadcast(
        &mut self,
        from: &Entity<TerminalView>,
        give: impl Fn(&mut TerminalView, &mut Context<TerminalView>),
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.panes.contains(from)) else {
            return;
        };
        if !tab.broadcast {
            return;
        }
        for pane in tab.panes.iter().filter(|pane| *pane != from) {
            pane.update(cx, |terminal, cx| give(terminal, cx));
        }
    }

    fn activate(&mut self, tab: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        // What the tab being left and the one shown printed or rang while
        // on screen needs no marker.
        for index in [self.active, tab].into_iter().flatten() {
            if let Some(shown) = self.tabs.get(index) {
                for pane in &shown.panes {
                    pane.update(cx, |pane, _| pane.seen());
                }
            }
        }
        self.active = tab;
        // Marks from the previous screen must not linger over the new one.
        self.live.clear();
        self.vault
            .update(cx, |vault, _| vault.set_showing(tab.is_none()));
        match tab {
            // The cached vault re-renders so its marks register again, with
            // art for any key trusted in a session meanwhile.
            None => {
                self.share_sessions(cx);
                self.vault.update(cx, |vault, cx| {
                    vault.refresh_keys();
                    vault.focus(window, cx);
                    cx.notify();
                });
            }
            Some(index) => self.tabs[index]
                .focused()
                .update(cx, |terminal, cx| terminal.focus(window, cx)),
        }
        cx.notify();
    }

    /// Lays the active tab out for `count` panes, opening new ones to the
    /// focused pane's host. Fewer than are open does nothing: panes close
    /// only by hand.
    fn set_panes(&mut self, count: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.active else {
            return;
        };
        let mut connection = self.tabs[index].focused().read(cx).connection().clone();
        // The first pane already runs the host's rules.
        connection.forwards.clear();
        let new_terminal = self.new_terminal;
        let broadcasting = self.tabs[index].broadcast;
        while self.tabs[index].panes.len() < count.min(MAX_PANES) {
            let terminal = cx.new(|cx| {
                let mut terminal = new_terminal(connection.clone(), cx);
                terminal.broadcasting = broadcasting;
                terminal
            });
            self.watch(&terminal, cx);
            let tab = &mut self.tabs[index];
            tab.panes.push(terminal);
            tab.focused = tab.panes.len() - 1;
        }
        self.tabs[index].maximized = None;
        self.activate(Some(index), window, cx);
    }

    fn toggle_broadcast(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.active {
            let tab = &mut self.tabs[index];
            tab.broadcast = !tab.broadcast;
            let on = tab.broadcast;
            for pane in &tab.panes {
                pane.update(cx, |pane, _| pane.broadcasting = on);
            }
            cx.notify();
        }
    }

    /// Shows or hides the Files panel; shown, it's on the focused pane's
    /// host.
    fn toggle_files(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.active else {
            return;
        };
        if self.tabs[index].files_open {
            self.tabs[index].files_open = false;
            cx.notify();
        } else {
            self.show_files(index, cx);
        }
    }

    /// Opens the Files panel of tab `index` on its focused pane's host,
    /// starting SFTP there unless the panel already shows that pane.
    fn show_files(&mut self, index: usize, cx: &mut Context<Self>) {
        let tab = &mut self.tabs[index];
        let focused = tab.focused().clone();
        if tab.files_for.as_ref() != Some(&focused) {
            let (opening, host) = {
                let pane = focused.read(cx);
                (pane.open_sftp(), pane.label())
            };
            tab.files = Some(
                cx.new(|cx| FilesPanel::new(opening, FilesPanel::default_local_dir(), host, cx)),
            );
            tab.files_for = Some(focused);
        }
        tab.files_open = true;
        cx.notify();
    }

    fn toggle_maximized(&mut self, pane: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.active else {
            return;
        };
        let tab = &mut self.tabs[index];
        tab.maximized = match tab.maximized {
            Some(_) => None,
            None => Some(pane),
        };
        tab.focused = pane;
        self.activate(Some(index), window, cx);
    }

    /// Records a live pane in History as closed by hand.
    fn record_closed(&mut self, terminal: &Entity<TerminalView>, cx: &mut Context<Self>) {
        let terminal = terminal.read(cx);
        if terminal.is_live() && !terminal.connection().local {
            let visit = terminal.visit("Closed");
            self.vault.update(cx, |vault, cx| {
                if let Some(host) = &visit.host {
                    vault.clear_forward_states(host, cx);
                }
                vault.record_visit(visit, cx);
            });
        }
    }

    /// Closes one pane of the active tab; the last one closes the tab.
    fn close_pane(&mut self, pane: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.active else {
            return;
        };
        if self.tabs[index].panes.len() == 1 {
            return self.close_tab(index, window, cx);
        }
        let closing = self.tabs[index].panes[pane].clone();
        self.record_closed(&closing, cx);
        // Dropping the view drops the session's input, which disconnects.
        let tab = &mut self.tabs[index];
        tab.panes.remove(pane);
        tab.maximized = None;
        if tab.focused >= pane && tab.focused > 0 {
            tab.focused -= 1;
        }
        self.activate(Some(index), window, cx);
    }

    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        // Live sessions closed by hand still go into History.
        for pane in self.tabs[index].panes.clone() {
            self.record_closed(&pane, cx);
        }
        if self
            .filling
            .as_ref()
            .is_some_and(|filling| self.tabs[index].panes.contains(&filling.pane))
        {
            self.filling = None;
        }
        self.tabs.remove(index);
        let next = match self.active {
            Some(active) if active == index => {
                (!self.tabs.is_empty()).then(|| index.min(self.tabs.len() - 1))
            }
            Some(active) if active > index => Some(active - 1),
            other => other,
        };
        // The tabs after it moved down, so the old index would mark another
        // tab as seen.
        self.active = next;
        self.activate(next, window, cx);
    }

    /// Types a saved snippet into the focused pane, or every pane while
    /// broadcasting; one with `{{name}}` variables asks for them first.
    fn run_snippet(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(active), Some(snippet)) = (
            self.active,
            self.vault.read(cx).snippets().get(index).cloned(),
        ) else {
            return;
        };
        let names = snippet.variables();
        if names.is_empty() {
            return self.type_snippet(active, &snippet, window, cx);
        }
        let fields: Vec<(String, Entity<InputState>)> = names
            .into_iter()
            .map(|name| {
                let placeholder = name.clone();
                let field = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
                (name, field)
            })
            .collect();
        // Enter moves to the next value, and types the snippet after the
        // last.
        let subscriptions = fields
            .iter()
            .enumerate()
            .map(|(at, (_, field))| {
                cx.subscribe_in(
                    field,
                    window,
                    move |this, _, event: &InputEvent, window, cx| {
                        if let InputEvent::PressEnter { .. } = event {
                            this.next_value(at, window, cx);
                        }
                    },
                )
            })
            .collect();
        if let Some((_, first)) = fields.first() {
            first.update(cx, |field, cx| field.focus(window, cx));
        }
        self.filling = Some(Filling {
            snippet,
            pane: self.tabs[active].focused().clone(),
            fields,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    /// After the value at `at`: the next field, or the snippet typed.
    fn next_value(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(filling) = &self.filling else {
            return;
        };
        if let Some((_, next)) = filling.fields.get(at + 1) {
            next.update(cx, |field, cx| field.focus(window, cx));
            return;
        }
        let values: Vec<(String, String)> = filling
            .fields
            .iter()
            .map(|(name, field)| (name.clone(), field.read(cx).value().to_string()))
            .collect();
        let snippet = filling.snippet.filled(&values);
        let tab = self
            .tabs
            .iter()
            .position(|tab| tab.panes.contains(&filling.pane));
        self.filling = None;
        if let Some(tab) = tab {
            self.type_snippet(tab, &snippet, window, cx);
        }
        cx.notify();
    }

    /// Types `snippet` into tab `index`'s focused pane, or into all its
    /// panes while it broadcasts.
    fn type_snippet(
        &mut self,
        index: usize,
        snippet: &Snippet,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab = &self.tabs[index];
        let panes = if tab.broadcast {
            tab.panes.clone()
        } else {
            vec![tab.focused().clone()]
        };
        for pane in panes {
            pane.update(cx, |terminal, cx| {
                terminal.type_text(snippet.keystrokes(), cx)
            });
        }
        if self.active == Some(index) {
            self.tabs[index]
                .focused()
                .update(cx, |terminal, cx| terminal.focus(window, cx));
        }
    }

    fn open_settings(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.new(|cx| SettingsView::new(page, window, cx));
        view.update(cx, |view, cx| view.focus(window, cx));
        self._settings_events = Some(cx.subscribe(&view, |this, _, event, cx| match event {
            SettingsEvent::Close => {
                this.settings = None;
                this._settings_events = None;
                cx.notify();
            }
            SettingsEvent::SyncNow => this.vault.update(cx, |vault, cx| vault.sync_now(cx)),
        }));
        self.settings = Some(view);
        cx.notify();
    }

    /// Goes to the vault's Snippets screen with a new snippet's form open.
    fn new_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.activate(None, window, cx);
        self.vault
            .update(cx, |vault, cx| vault.new_snippet(window, cx));
    }

    /// Over the panes: the focused host with its latency trace, the layout
    /// and Broadcast.
    fn toolbar(&self, tab: &Tab, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let focused = tab.focused().read(cx);
        let local = focused.connection().local;
        let host = focused.host().map(str::to_string);
        let target = focused.connection().target.clone();
        let trace = host
            .as_deref()
            .and_then(|name| self.vault.read(cx).host_trace(name));
        let latency = match trace.as_ref().map(|trace| trace.status) {
            Some(HostStatus::Online {
                latency_ms: Some(ms),
            }) => format!("{ms} ms"),
            _ => String::new(),
        };
        let layouts = [
            (1, IconName::Square),
            (2, IconName::Columns2),
            (3, IconName::PanelLeft),
            (4, IconName::LayoutGrid),
        ];
        let open = tab.panes.len();
        div()
            .flex()
            .items_center()
            .gap(px(12.))
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(if local {
                        focused.label().to_string()
                    } else {
                        host.clone().unwrap_or_else(|| target.host.clone())
                    }),
            )
            .child(
                div()
                    .font_family(MONO)
                    .text_size(px(12.))
                    .text_color(theme.text2)
                    .child(if local {
                        tern_core::local::default_shell()
                    } else {
                        format!("{}@{}:{}", target.user, target.host, target.port)
                    }),
            )
            .when_some(trace, |d, trace| {
                let (ink, faint, seed) = (theme.text, theme.text3, marks::seed(&target.host));
                d.child(
                    // Drawn once per render: a terminal tab doesn't tick.
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, _| {
                            marks::paint_trace(bounds, &trace, seed, 0., ink, faint, window)
                        },
                    )
                    .w(px(120.))
                    .h(px(18.)),
                )
                .child(div().font_family(MONO).text_size(px(12.)).child(latency))
            })
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .gap(px(4.))
                    .p(px(4.))
                    .rounded(px(14.))
                    .bg(theme.card2)
                    .children(layouts.into_iter().map(|(count, glyph)| {
                        let id = format!("layout-{count}");
                        let (on, usable) = (count == open, count >= open);
                        let fg = if on {
                            theme.text
                        } else if usable {
                            theme.text2
                        } else {
                            theme.text3.opacity(0.4)
                        };
                        div()
                            .id(SharedString::from(id.clone()))
                            .debug_selector(move || id)
                            .size(px(32.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(10.))
                            .when(on, |d| d.bg(theme.card).shadow(theme.shadow_sm()))
                            .child(icon(glyph, 16., fg))
                            .when(usable && !on, |d| {
                                d.cursor_pointer().on_click(cx.listener(
                                    move |this, _, window, cx| this.set_panes(count, window, cx),
                                ))
                            })
                    })),
            )
            .when(!local, |d| {
                d.child(
                    div()
                        .id("toggle-files")
                        .debug_selector(|| "toggle-files".into())
                        .h(px(40.))
                        .px(px(14.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .rounded(px(13.))
                        .border_1()
                        .border_color(if tab.files_open {
                            theme.text
                        } else {
                            theme.line
                        })
                        .bg(if tab.files_open {
                            theme.text
                        } else {
                            theme.card
                        })
                        .text_color(if tab.files_open {
                            theme.card
                        } else {
                            theme.text
                        })
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .cursor_pointer()
                        .child(icon(
                            IconName::FolderOpen,
                            15.,
                            if tab.files_open {
                                theme.card
                            } else {
                                theme.text
                            },
                        ))
                        .child("Files")
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_files(cx))),
                )
            })
            .child(
                div()
                    .id("broadcast")
                    .debug_selector(|| "broadcast".into())
                    .h(px(40.))
                    .px(px(14.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(13.))
                    .border_1()
                    .border_color(if tab.broadcast {
                        theme.text
                    } else {
                        theme.line
                    })
                    .bg(if tab.broadcast {
                        theme.text
                    } else {
                        theme.card
                    })
                    .text_color(if tab.broadcast {
                        theme.card
                    } else {
                        theme.text
                    })
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .child(icon(
                        IconName::RadioTower,
                        15.,
                        if tab.broadcast {
                            theme.card
                        } else {
                            theme.text
                        },
                    ))
                    .child("Broadcast")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_broadcast(cx))),
            )
    }

    /// The panes, laid out by how many there are, or the maximized one.
    fn panes(&self, tab: &Tab, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let pane = |index: usize| self.pane(tab, index, theme, cx);
        let row = || div().flex_1().min_h_0().flex().gap(px(12.));
        let column = || div().flex_1().min_w_0().flex().flex_col().gap(px(12.));
        if let Some(index) = tab.maximized {
            return row().child(pane(index)).into_any_element();
        }
        match tab.panes.len() {
            1 => row().child(pane(0)).into_any_element(),
            2 => row().child(pane(0)).child(pane(1)).into_any_element(),
            3 => row()
                .child(pane(0))
                .child(column().child(pane(1)).child(pane(2)))
                .into_any_element(),
            _ => column()
                .child(row().child(pane(0)).child(pane(1)))
                .child(row().child(pane(2)).child(pane(3)))
                .into_any_element(),
        }
    }

    /// A pane: an ink ring while focused (every pane while broadcasting),
    /// and with others beside it a header with its host, Maximize and
    /// Close.
    fn pane(&self, tab: &Tab, index: usize, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let terminal = &tab.panes[index];
        let ringed = tab.broadcast || (tab.focused == index && tab.panes.len() > 1);
        let id = format!("pane-{index}");
        let (maximize, close) = (
            format!("pane-maximize-{index}"),
            format!("pane-close-{index}"),
        );
        div()
            .id(SharedString::from(id.clone()))
            .debug_selector(move || id)
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .rounded(px(20.))
            .border_2()
            .border_color(if ringed {
                theme.text
            } else {
                transparent_black()
            })
            .when(tab.panes.len() > 1, |d| {
                let maximized = tab.maximized.is_some();
                d.child(
                    div()
                        .h(px(32.))
                        .pl(px(12.))
                        .pr(px(4.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .text_size(px(12.))
                        .child(phase_glyph(&terminal.read(cx).phase, theme))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(terminal.read(cx).label()),
                        )
                        .child(
                            pane_button(
                                maximize,
                                if maximized {
                                    IconName::Minimize2
                                } else {
                                    IconName::Maximize2
                                },
                                theme,
                            )
                            .on_click(cx.listener(
                                move |this, _, window, cx| this.toggle_maximized(index, window, cx),
                            )),
                        )
                        .child(
                            pane_button(close, IconName::Close, theme).on_click(cx.listener(
                                move |this, _, window, cx| this.close_pane(index, window, cx),
                            )),
                        ),
                )
            })
            .child(
                div().flex_1().min_h_0().child(
                    terminal
                        .clone()
                        .cached(StyleRefinement::default().size_full()),
                ),
            )
    }

    /// The values a snippet asks for, in a row above the snippet bar.
    fn filling_row(
        &self,
        filling: &Filling,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let last = filling.fields.len().saturating_sub(1);
        div()
            .id("snippet-values")
            .debug_selector(|| "snippet-values".into())
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            // Escape leaves the snippet untyped, and the keyboard with the
            // pane.
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    if let Some(filling) = this.filling.take() {
                        filling.pane.update(cx, |pane, cx| pane.focus(window, cx));
                    }
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(filling.snippet.name.clone()),
            )
            .children(filling.fields.iter().map(|(_, field)| {
                div()
                    .w(px(180.))
                    .h(px(32.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .rounded(px(10.))
                    .bg(theme.card2)
                    .font_family(MONO)
                    .text_size(px(12.5))
                    .child(Input::new(field).appearance(false))
            }))
            .child(
                ink_button("snippet-run", theme)
                    .debug_selector(|| "snippet-run".into())
                    .h(px(32.))
                    .rounded(px(10.))
                    .child(icon(IconName::Play, 13., theme.card))
                    .child("Run")
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.next_value(last, window, cx)),
                    ),
            )
    }

    /// Under the panes: a chip per snippet, then a dashed one to add
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
            // A state, not a control, so it gets no surface of its own;
            // clicking it goes to the Sync settings. Nothing while sync is
            // off.
            .map(|d| {
                let (glyph, label) = match sync::state(cx).status {
                    Status::Off => return d,
                    Status::Syncing => (IconName::LoaderCircle, "Syncing"),
                    Status::Synced(_) => (IconName::Check, "Synced"),
                    Status::Failed(_) => (IconName::CircleAlert, "Not synced"),
                };
                d.child(
                    div()
                        .id("sync-state")
                        .debug_selector(|| "sync-state".into())
                        .h(px(34.))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .gap(px(7.))
                        .text_size(px(13.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text2)
                        .cursor_pointer()
                        .child(icon(glyph, 14., theme.text))
                        .child(label)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_settings(Page::Sync, window, cx)
                        })),
                )
            })
            .child(
                header_button("open-palette", IconName::Search, theme)
                    .on_click(cx.listener(|this, _, window, cx| this.open_palette(window, cx))),
            )
            .child(
                header_button("open-settings", IconName::SlidersHorizontal, theme).on_click(
                    cx.listener(|this, _, window, cx| {
                        this.open_settings(Page::Terminal, window, cx)
                    }),
                ),
            )
            .child(
                header_button(
                    "theme-toggle",
                    if dark { IconName::Sun } else { IconName::Moon },
                    theme,
                )
                // A choice made here sticks, as if made in Settings.
                .on_click(move |_, window, cx| {
                    settings::update(cx, |settings| {
                        settings.appearance = if dark {
                            tern_core::settings::Appearance::Light
                        } else {
                            tern_core::settings::Appearance::Dark
                        }
                    });
                    settings::apply_appearance(window, cx);
                }),
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
            .children(self.tabs.iter().enumerate().map(|(index, session)| {
                let focused = session.focused();
                let terminal = focused.read(cx);
                tab(
                    SharedString::from(format!("tab-{}", focused.entity_id())),
                    self.active == Some(index),
                )
                .pr(px(4.))
                .child(phase_glyph(&terminal.phase, theme))
                .child(div().max_w(px(180.)).truncate().child(terminal.title()))
                // A tab not on screen shows a bell if one rang there, or a dot
                // for output it hasn't shown.
                .when(self.active != Some(index), |d| {
                    let panes = session.panes.iter().map(|pane| pane.read(cx));
                    let (rang, unseen) = panes.fold((false, false), |(rang, unseen), pane| {
                        (rang || pane.rang, unseen || pane.unseen)
                    });
                    let id = format!("activity-{index}");
                    match (rang, unseen) {
                        (true, _) => d.child(div().debug_selector(move || id).child(icon(
                            IconName::Bell,
                            12.,
                            theme.text,
                        ))),
                        (false, true) => d.child(
                            div()
                                .debug_selector(move || id)
                                .size(px(5.))
                                .rounded_full()
                                .bg(theme.text2),
                        ),
                        _ => d,
                    }
                })
                .child(
                    div()
                        .id(SharedString::from(format!("close-{index}")))
                        .size(px(24.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(8.))
                        .text_color(theme.text3)
                        .child(icon(IconName::Close, 12., theme.text3))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.close_tab(index, window, cx);
                        })),
                )
                .on_click(
                    cx.listener(move |this, _, window, cx| this.activate(Some(index), window, cx)),
                )
            }))
            .child(
                div()
                    .id("new-local")
                    .debug_selector(|| "new-local".into())
                    .size(px(34.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(12.))
                    .cursor_pointer()
                    .hover(|d| d.bg(theme.card))
                    .child(icon(IconName::SquareTerminal, 16., theme.text2))
                    .on_click(cx.listener(|this, _, window, cx| this.open_local(window, cx))),
            )
    }
}

/// A session's state as a glyph: a solid dot while live, a ring once
/// ended, a spinner on the way.
fn phase_glyph(phase: &Phase, theme: &Theme) -> AnyElement {
    match phase {
        Phase::Live => status_dot(theme).into_any_element(),
        Phase::Ended(_) => div()
            .size(px(7.))
            .rounded_full()
            .border_1()
            .border_color(theme.text3)
            .into_any_element(),
        _ => icon(IconName::LoaderCircle, 12., theme.text2).into_any_element(),
    }
}

fn pane_button(id: String, glyph: IconName, theme: &Theme) -> Stateful<Div> {
    div()
        .id(SharedString::from(id.clone()))
        .debug_selector(move || id)
        .size(px(26.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(8.))
        .cursor_pointer()
        .child(icon(glyph, 13., theme.text2))
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        bench::frame_start();
        let time = if cx.reduce_motion() && !bench::enabled() {
            0.
        } else {
            self.started.elapsed().as_secs_f32()
        };
        // The focused pane is the one holding keyboard focus.
        if let Some(index) = self.active {
            let tab = &mut self.tabs[index];
            if let Some(focused) = tab
                .panes
                .iter()
                .position(|pane| pane.read(cx).is_focused(window))
            {
                tab.focused = focused;
            }
        }
        let theme = theme::current(cx);
        let cached = StyleRefinement::default().size_full();
        let body = match self.active {
            None if bench::uncached() => self.vault.clone().into_any_element(),
            None => self.vault.clone().cached(cached).into_any_element(),
            Some(index) => {
                let tab = &self.tabs[index];
                div()
                    .size_full()
                    .p(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(self.toolbar(tab, &theme, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .gap(px(12.))
                            .child(self.panes(tab, &theme, cx))
                            .when_some(tab.files.clone().filter(|_| tab.files_open), |d, files| {
                                d.child(files)
                            }),
                    )
                    .when_some(
                        self.filling
                            .as_ref()
                            .filter(|filling| tab.panes.contains(&filling.pane)),
                        |d, filling| d.child(self.filling_row(filling, &theme, cx)),
                    )
                    .child(self.snippet_bar(&theme, cx))
                    .into_any_element()
            }
        };
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .on_key_down(cx.listener(Self::key_down))
            .bg(theme.win)
            .text_color(theme.text)
            .text_size(px(14.))
            .child(self.header(&theme, cx))
            .child(div().flex_1().min_h_0().child(body))
            .child(live_layer(&self.live, time))
            .when_some(self.palette.as_ref(), |d, palette| {
                d.child(self.palette_view(palette, &theme, cx))
            })
            .when_some(self.settings.clone(), |d, view| {
                d.child(
                    div()
                        .id("settings-scrim")
                        .occlude()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .p(px(12.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(theme.scrim())
                        .child(view),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports: `gpui_kit::*` would shadow Rust's `#[test]`.
    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use gpui_kit::{AppContext as _, Context, Entity};
    use tern_core::Vault;
    use tern_core::ssh::Connection;

    use super::Workspace;
    use crate::live::LiveSlots;
    use crate::terminal::TerminalView;
    use crate::vault::{VaultContent, VaultEvent};

    /// Panes that don't connect.
    fn offline(connection: Connection, cx: &mut Context<TerminalView>) -> TerminalView {
        TerminalView::offline_to(connection, cx)
    }

    /// A window with the sample vault and one session tab.
    fn open_workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        cx.update(gpui_kit::init);
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            let live = LiveSlots::default();
            let vault =
                cx.new(|cx| VaultContent::new(Vault::sample(), None, live.clone(), window, cx));
            let mut workspace = Workspace::with(vault, live, offline, window, cx);
            let terminal = cx.new(TerminalView::offline);
            workspace.add_session(terminal, window, cx);
            workspace
        });
        cx.run_until_parked();
        (workspace, cx)
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing on screen is tagged {selector}"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }

    /// What each pane of the first tab has sent, then forgets it.
    fn sent(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> Vec<Vec<u8>> {
        workspace.read_with(cx, |workspace, cx| {
            workspace.tabs[0]
                .panes
                .iter()
                .map(|pane| pane.read(cx).sent.take())
                .collect()
        })
    }

    #[gpui::test]
    fn opens_local_terminals_and_switches_tabs(cx: &mut TestAppContext) {
        let (workspace, cx) = open_workspace(cx);
        let active = |cx: &mut VisualTestContext| workspace.read_with(cx, |w, _| w.active);
        let local = |cx: &mut VisualTestContext, index: usize| {
            workspace.read_with(cx, |w, cx| {
                w.tabs[index].focused().read(cx).connection().local
            })
        };
        // Focus a pane, as a user typing would have.
        cx.update(|window, cx| {
            let pane = workspace.read(cx).tabs[0].focused().clone();
            pane.update(cx, |pane, cx| pane.focus(window, cx));
        });

        // The shortcut passes through the pane to the window.
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-t"
        } else {
            "ctrl-shift-t"
        });
        assert_eq!(active(cx), Some(1));
        assert!(local(cx, 1));
        assert!(!local(cx, 0));
        // A local tab has no Files.
        assert!(cx.debug_bounds("toggle-files").is_none());
        click(cx, "new-local");
        assert_eq!(active(cx), Some(2));

        // Ctrl+Tab goes round: the vault, then each tab.
        cx.simulate_keystrokes("ctrl-tab");
        assert_eq!(active(cx), None);
        cx.simulate_keystrokes("ctrl-tab");
        assert_eq!(active(cx), Some(0));
        cx.simulate_keystrokes("ctrl-shift-tab");
        assert_eq!(active(cx), None);
        cx.simulate_keystrokes("ctrl-shift-tab");
        assert_eq!(active(cx), Some(2));
    }

    #[gpui::test]
    fn opens_hosts_together_four_to_a_tab(cx: &mut TestAppContext) {
        let (workspace, cx) = open_workspace(cx);
        let connections: Vec<Connection> = (0..5)
            .map(|n| {
                Connection::typed(
                    tern_core::ssh::Target {
                        host: format!("node-{n}"),
                        port: 22,
                        user: "ops".into(),
                    },
                    None,
                )
            })
            .collect();
        workspace.update(cx, |workspace, cx| {
            workspace
                .vault
                .update(cx, |_, cx| cx.emit(VaultEvent::OpenTogether(connections)))
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            // The first tab was already there.
            let panes: Vec<usize> = workspace.tabs.iter().map(|tab| tab.panes.len()).collect();
            assert_eq!(panes, [1, 4, 1]);
            assert_eq!(workspace.active, Some(1));
            let hosts: Vec<String> = workspace.tabs[1]
                .panes
                .iter()
                .map(|pane| pane.read(cx).connection().target.host.clone())
                .collect();
            assert_eq!(hosts, ["node-0", "node-1", "node-2", "node-3"]);
        });
    }

    #[gpui::test]
    fn types_a_one_off_command_at_the_first_login_only(cx: &mut TestAppContext) {
        use tern_core::ssh::{Event, Target};

        let (workspace, cx) = open_workspace(cx);
        let target = Target {
            host: "web-01".into(),
            port: 22,
            user: "deploy".into(),
        };
        let connection = Connection {
            on_login: Some("cd /srv".into()),
            once: Some("make deploy".into()),
            ..Connection::typed(target, None)
        };
        workspace.update(cx, |workspace, cx| {
            workspace.vault.update(cx, |_, cx| {
                cx.emit(VaultEvent::OpenTogether(vec![connection]))
            })
        });
        cx.run_until_parked();
        // Offline panes are signed in as they open.
        let sent = |cx: &mut VisualTestContext, pane: usize| {
            workspace.read_with(cx, |workspace, cx| {
                workspace.tabs[1].panes[pane].read(cx).sent.take()
            })
        };
        assert_eq!(sent(cx, 0), b"cd /srv\rmake deploy\r");
        // A reconnect types only the host's own command again.
        let pane = workspace.read_with(cx, |workspace, _| workspace.tabs[1].panes[0].clone());
        pane.update(cx, |pane, cx| pane.on_session_event(Event::Connected, cx));
        assert_eq!(sent(cx, 0), b"cd /srv\r");
        // So does a pane split from it.
        click(cx, "layout-2");
        assert_eq!(sent(cx, 1), b"cd /srv\r");
    }

    #[gpui::test]
    fn broadcast_pastes_ask_first_and_suit_each_shell(cx: &mut TestAppContext) {
        use gpui_kit::ClipboardItem;
        use tern_core::ssh::Event;

        let (workspace, cx) = open_workspace(cx);
        click(cx, "layout-2");
        click(cx, "broadcast");
        let panes = workspace.read_with(cx, |workspace, _| workspace.tabs[0].panes.clone());
        // The first pane's shell takes pastes bracketed, the second's not.
        panes[0].update(cx, |pane, cx| {
            pane.on_session_event(Event::Output(b"\x1b[?2004h".to_vec()), cx)
        });
        cx.update(|window, cx| panes[0].update(cx, |pane, cx| pane.focus(window, cx)));
        cx.write_to_clipboard(ClipboardItem::new_string("make\nmake install".into()));

        // It asks first, though this pane's shell would take it bracketed.
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-v"
        } else {
            "ctrl-shift-v"
        });
        assert_eq!(sent(&workspace, cx), [Vec::<u8>::new(), Vec::new()]);
        cx.simulate_keystrokes("enter");
        assert_eq!(
            sent(&workspace, cx),
            [
                b"\x1b[200~make\rmake install\x1b[201~".to_vec(),
                b"make\rmake install".to_vec()
            ]
        );
    }

    #[gpui::test]
    fn closing_a_tab_keeps_the_other_tabs_marks(cx: &mut TestAppContext) {
        use tern_core::ssh::Event;

        let (workspace, cx) = open_workspace(cx);
        click(cx, "new-local");
        click(cx, "new-local");
        // On the middle tab, the last one rings.
        cx.simulate_keystrokes("ctrl-shift-tab");
        let last = workspace.read_with(cx, |workspace, _| workspace.tabs[2].focused().clone());
        last.update(cx, |pane, cx| {
            pane.on_session_event(Event::Output(b"\x07".to_vec()), cx)
        });

        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.close_tab(0, window, cx))
        });
        workspace.read_with(cx, |workspace, _| assert_eq!(workspace.active, Some(0)));
        last.read_with(cx, |pane, _| assert!(pane.rang));
    }

    #[gpui::test]
    fn window_shortcuts_wait_while_the_palette_is_open(cx: &mut TestAppContext) {
        let (workspace, cx) = open_workspace(cx);
        let first = workspace.read_with(cx, |workspace, _| workspace.tabs[0].focused().clone());
        cx.update(|window, cx| first.update(cx, |pane, cx| pane.focus(window, cx)));
        let active = |cx: &mut VisualTestContext| workspace.read_with(cx, |w, _| w.active);

        click(cx, "open-palette");
        cx.simulate_keystrokes("ctrl-tab");
        assert!(cx.debug_bounds("palette").is_some());
        assert_eq!(active(cx), Some(0));
        cx.simulate_keystrokes("escape");
        cx.simulate_keystrokes("ctrl-tab");
        assert_eq!(active(cx), None);
    }

    #[gpui::test]
    fn files_opens_on_the_host_asked_for(cx: &mut TestAppContext) {
        let (workspace, cx) = open_workspace(cx);
        let sample = Vault::sample();
        let connection = |name: &str| sample.connection(sample.find(name).unwrap()).unwrap();
        let (web, db) = (connection("prod-web-01"), connection("prod-db-01"));
        let emit = |cx: &mut VisualTestContext, event: VaultEvent| {
            workspace.update(cx, |workspace, cx| {
                workspace.vault.update(cx, |_, cx| cx.emit(event))
            });
            cx.run_until_parked();
        };
        emit(cx, VaultEvent::OpenTogether(vec![web, db.clone()]));
        workspace.read_with(cx, |workspace, _| assert_eq!(workspace.tabs[1].focused, 0));

        // The db's SFTP, not the focused web pane's.
        emit(
            cx,
            VaultEvent::Open {
                connection: db,
                files: true,
            },
        );
        workspace.read_with(cx, |workspace, _| {
            let tab = &workspace.tabs[1];
            assert_eq!(tab.focused, 1);
            assert!(tab.files_open);
            assert_eq!(tab.files_for.as_ref(), Some(&tab.panes[1]));
        });
    }

    #[gpui::test]
    fn marks_tabs_with_unseen_output_and_bells(cx: &mut TestAppContext) {
        use tern_core::ssh::Event;

        let (workspace, cx) = open_workspace(cx);
        let first = workspace.read_with(cx, |workspace, _| workspace.tabs[0].focused().clone());
        let output = |cx: &mut VisualTestContext, bytes: &[u8]| {
            first.update(cx, |pane, cx| {
                pane.on_session_event(Event::Output(bytes.to_vec()), cx)
            });
            cx.run_until_parked();
        };
        // Output while the tab shows needs no marker.
        output(cx, b"hello");
        click(cx, "new-local");
        assert!(cx.debug_bounds("activity-0").is_none());

        // Hidden, its output gets a dot, then a bell beats the dot.
        output(cx, b"more");
        assert!(cx.debug_bounds("activity-0").is_some());
        output(cx, b"\x07");
        first.read_with(cx, |pane, _| assert!(pane.rang));

        // Looking at it clears both.
        cx.simulate_keystrokes("ctrl-shift-tab");
        cx.simulate_keystrokes("ctrl-tab");
        assert!(cx.debug_bounds("activity-0").is_none());
        first.read_with(cx, |pane, _| assert!(!pane.rang && !pane.unseen));
    }

    #[gpui::test]
    fn asks_for_a_snippets_values_before_typing_it(cx: &mut TestAppContext) {
        use tern_core::snippet::Snippet;

        cx.update(gpui_kit::init);
        let (workspace, cx) = cx.add_window_view(|window, cx| {
            let live = LiveSlots::default();
            let mut vault = Vault::default();
            vault
                .snippets
                .push(Snippet::new("Tail", "tail -n {{lines}} {{file}}"));
            let vault = cx.new(|cx| VaultContent::new(vault, None, live.clone(), window, cx));
            let mut workspace = Workspace::with(vault, live, offline, window, cx);
            let terminal = cx.new(TerminalView::offline);
            workspace.add_session(terminal, window, cx);
            workspace
        });
        cx.run_until_parked();

        click(cx, "snippet-0");
        assert_eq!(sent(&workspace, cx), [Vec::<u8>::new()]);
        cx.simulate_input("50");
        cx.simulate_keystrokes("enter");
        cx.simulate_input("/var/log/syslog");
        cx.simulate_keystrokes("enter");
        assert_eq!(
            sent(&workspace, cx),
            [b"tail -n 50 /var/log/syslog\r".to_vec()]
        );
        assert!(cx.debug_bounds("snippet-values").is_none());

        // Escape leaves it untyped, and the keyboard with the pane.
        click(cx, "snippet-0");
        cx.simulate_keystrokes("escape");
        assert!(cx.debug_bounds("snippet-values").is_none());
        assert_eq!(sent(&workspace, cx), [Vec::<u8>::new()]);
        let pane = workspace.read_with(cx, |workspace, _| workspace.tabs[0].focused().clone());
        cx.update(|window, cx| assert!(pane.read(cx).is_focused(window)));

        // The values stay with the tab they were asked for in.
        click(cx, "snippet-0");
        click(cx, "new-local");
        assert!(cx.debug_bounds("snippet-values").is_none());
        cx.simulate_keystrokes("ctrl-shift-tab");
        click(cx, "snippet-run");
        assert_eq!(sent(&workspace, cx), [b"tail -n  \r".to_vec()]);
        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.tabs[1].focused().read(cx).sent.take().is_empty())
        });
    }

    #[gpui::test]
    fn the_snippet_bar_types_into_the_session(cx: &mut TestAppContext) {
        let (workspace, cx) = open_workspace(cx);
        click(cx, "snippet-0");
        assert_eq!(sent(&workspace, cx), [b"df -h\r".to_vec()]);

        // New snippet goes to the vault's Snippets screen, form open.
        click(cx, "new-snippet");
        workspace.read_with(cx, |workspace, _| assert_eq!(workspace.active, None));
        assert!(cx.debug_bounds("save-snippet").is_some());
    }

    #[gpui::test]
    fn writes_down_the_open_tabs(cx: &mut TestAppContext) {
        use tern_core::restore::{OpenTabs, Opened};

        let dir = std::env::temp_dir().join(format!("tern-open-tabs-{}", std::process::id()));
        let file = dir.join("tabs.json");
        let (workspace, cx) = open_workspace(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.remember_tabs_in(file.clone(), cx)
        });

        click(cx, "new-local");
        let open = OpenTabs::load(&file);
        assert_eq!(open.tabs.len(), 2);
        assert_eq!(open.tabs[1], [Opened::Local]);
        assert_eq!(open.active, Some(1));

        // The vault on screen is written down too.
        cx.simulate_keystrokes("ctrl-tab");
        assert_eq!(OpenTabs::load(&file).active, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn reopens_the_tabs_written_down(cx: &mut TestAppContext) {
        use tern_core::restore::{OpenTabs, Opened};

        let (workspace, cx) = open_workspace(cx);
        let host = |name: &str| Opened::Host {
            id: None,
            name: name.into(),
        };
        let open = OpenTabs {
            tabs: vec![
                vec![host("prod-web-01")],
                vec![Opened::Local, host("prod-web-02")],
                vec![host("deleted since")],
            ],
            active: Some(1),
        };
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| workspace.reopen(&open, window, cx))
        });
        workspace.read_with(cx, |workspace, cx| {
            // After the tab that was already there; the deleted host's tab
            // is left out.
            let tabs: Vec<Vec<String>> = workspace.tabs[1..]
                .iter()
                .map(|tab| {
                    tab.panes
                        .iter()
                        .map(|pane| pane.read(cx).label().to_string())
                        .collect()
                })
                .collect();
            assert_eq!(tabs.len(), 2);
            assert_eq!(tabs[1].len(), 2);
            assert_eq!(workspace.active, Some(2));
            let hosts: Vec<Option<String>> = workspace.tabs[2]
                .panes
                .iter()
                .map(|pane| pane.read(cx).connection().host.clone())
                .collect();
            assert_eq!(hosts, [None, Some("prod-web-02".to_string())]);
            assert!(workspace.tabs[2].panes[0].read(cx).connection().local);
        });
    }

    #[gpui::test]
    fn the_palette_runs_what_it_finds(cx: &mut TestAppContext) {
        let (workspace, cx) = open_workspace(cx);
        let palette = if cfg!(target_os = "macos") {
            "cmd-shift-p"
        } else {
            "ctrl-shift-p"
        };
        let first = workspace.read_with(cx, |workspace, _| workspace.tabs[0].focused().clone());
        cx.update(|window, cx| first.update(cx, |pane, cx| pane.focus(window, cx)));

        // Down passes over the first match; Enter connects to the second.
        cx.simulate_keystrokes(palette);
        assert!(cx.debug_bounds("palette").is_some());
        cx.simulate_input("prod");
        cx.simulate_keystrokes("down enter");
        assert!(cx.debug_bounds("palette").is_none());
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.active, Some(1));
            let pane = workspace.tabs[1].focused().read(cx);
            assert_eq!(pane.connection().target.host, "web-01.internal");
        });

        // Escape runs nothing and gives the keyboard back.
        cx.simulate_keystrokes(palette);
        cx.simulate_input("local");
        cx.simulate_keystrokes("escape");
        assert!(cx.debug_bounds("palette").is_none());
        workspace.read_with(cx, |workspace, _| assert_eq!(workspace.tabs.len(), 2));
        let second = workspace.read_with(cx, |workspace, _| workspace.tabs[1].focused().clone());
        cx.update(|window, cx| {
            assert!(second.read(cx).is_focused(window));
        });

        // A click runs a row; the search button opens it too.
        click(cx, "open-palette");
        cx.simulate_input("known");
        click(cx, "palette-0");
        workspace.read_with(cx, |workspace, _| assert_eq!(workspace.active, None));
        assert!(cx.debug_bounds("known-hosts").is_some());

        // The window's own actions.
        let dark = |cx: &mut VisualTestContext| cx.update(|_, cx| crate::theme::current(cx).dark);
        let before = dark(cx);
        click(cx, "open-palette");
        cx.simulate_input("appearance");
        cx.simulate_keystrokes("enter");
        assert_eq!(dark(cx), !before);
    }

    #[gpui::test]
    fn settings_change_the_theme_and_terminals(cx: &mut TestAppContext) {
        use tern_core::settings::{Appearance, CursorStyle};

        let (workspace, cx) = open_workspace(cx);
        let dark = |cx: &mut VisualTestContext| cx.update(|_, cx| crate::theme::current(cx).dark);
        let current = |cx: &mut VisualTestContext| cx.update(|_, cx| crate::settings::current(cx));

        click(cx, "open-settings");
        click(cx, "settings-appearance");
        click(cx, "appearance-dark");
        assert!(dark(cx));
        assert_eq!(current(cx).appearance, Appearance::Dark);
        click(cx, "appearance-light");
        assert!(!dark(cx));

        click(cx, "settings-terminal");
        click(cx, "cursor-beam");
        click(cx, "copy-on-select");
        let terminal = current(cx).terminal;
        assert_eq!(
            (terminal.cursor, terminal.copy_on_select),
            (CursorStyle::Beam, true)
        );

        // A colour scheme repaints terminals; Tern's own follows the look.
        click(cx, "colors-dracula");
        assert_eq!(current(cx).terminal.colors.as_deref(), Some("Dracula"));
        let background = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| crate::theme::TermPalette::current(cx).background)
        };
        let dracula = tern_core::schemes::find("Dracula").unwrap();
        assert_eq!(
            background(cx),
            crate::theme::TermPalette::for_scheme(dracula).background
        );
        click(cx, "colors-tern");
        assert_eq!(current(cx).terminal.colors, None);

        // A typed size applies once it makes sense.
        cx.update(|window, cx| {
            let view = workspace
                .read(cx)
                .settings
                .clone()
                .expect("Settings is open");
            let input = view.read(cx).font_size_input();
            input.update(cx, |state, cx| state.focus(window, cx));
        });
        cx.run_until_parked();
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        });
        cx.simulate_input("16");
        assert_eq!(current(cx).terminal.font_size, 16.);

        assert!(current(cx).reopen_tabs);
        click(cx, "settings-general");
        click(cx, "reopen-tabs");
        assert!(!current(cx).reopen_tabs);

        // The header's toggle sticks too; Escape closes.
        cx.simulate_keystrokes("escape");
        assert!(cx.debug_bounds("settings").is_none());
        click(cx, "theme-toggle");
        assert_eq!(current(cx).appearance, Appearance::Dark);
    }

    #[gpui::test]
    fn the_sync_page_sets_what_syncs(cx: &mut TestAppContext) {
        let (_, cx) = open_workspace(cx);
        let current = |cx: &mut VisualTestContext| cx.update(|_, cx| crate::settings::current(cx));
        // Off: no state in the header.
        assert!(cx.debug_bounds("sync-state").is_none());
        cx.update(|_, cx| {
            crate::settings::update(cx, |settings| {
                settings.sync.folder = Some(std::env::temp_dir());
                settings.sync.ensure_device_id();
            });
            crate::sync::set_state(
                cx,
                crate::sync::SyncState {
                    status: crate::sync::Status::Synced(tern_core::host::now_ms()),
                    devices: vec![("Laptop".into(), tern_core::host::now_ms())],
                },
            );
        });
        click(cx, "sync-state");
        click(cx, "sync-snippets");
        click(cx, "sync-forwards");
        let sync = current(cx).sync;
        assert_eq!(
            (sync.hosts, sync.snippets, sync.forwards),
            (true, false, false)
        );
        click(cx, "sync-stop");
        assert_eq!(current(cx).sync.folder, None);
        assert!(cx.debug_bounds("sync-state").is_none());
    }

    #[gpui::test]
    fn opening_a_host_shows_its_session(cx: &mut TestAppContext) {
        let (workspace, cx) = open_workspace(cx);
        let vault = Vault::sample();
        let to = |name: &str| {
            let host = vault.find(name).unwrap();
            vault.connection(host).unwrap()
        };
        let tabs = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| (workspace.tabs.len(), workspace.active))
        };
        let show = |cx: &mut VisualTestContext, connection: Connection, files: bool| {
            cx.update(|window, cx| {
                workspace.update(cx, |workspace, cx| {
                    workspace.show_session(connection, files, window, cx)
                })
            });
            cx.run_until_parked();
        };

        // No session to bastion yet: one opens, in a tab of its own.
        show(cx, to("bastion"), false);
        assert_eq!(tabs(cx), (2, Some(1)));
        cx.update(|window, cx| workspace.update(cx, |w, cx| w.activate(Some(0), window, cx)));

        // Now it's shown again, with its Files panel.
        show(cx, to("bastion"), true);
        assert_eq!(tabs(cx), (2, Some(1)));
        workspace.read_with(cx, |workspace, _| assert!(workspace.tabs[1].files_open));
        show(cx, to("prod-db-01"), false);
        assert_eq!(tabs(cx), (3, Some(2)));
    }

    #[gpui::test]
    fn splits_into_panes_and_broadcasts(cx: &mut TestAppContext) {
        let (workspace, cx) = open_workspace(cx);
        let panes = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| workspace.tabs[0].panes.len())
        };

        // The grid of four opens three more panes to the same host.
        click(cx, "layout-4");
        assert_eq!(panes(cx), 4);
        for pane in ["pane-0", "pane-1", "pane-2", "pane-3"] {
            assert!(cx.debug_bounds(pane).is_some(), "{pane} shows");
        }
        // Fewer panes than are open isn't offered.
        click(cx, "layout-2");
        assert_eq!(panes(cx), 4);

        // Typing goes to the focused pane only, until broadcasting.
        sent(&workspace, cx);
        click(cx, "pane-1");
        cx.simulate_input("ls");
        assert_eq!(
            sent(&workspace, cx),
            [vec![], b"ls".to_vec(), vec![], vec![]]
        );
        click(cx, "broadcast");
        cx.simulate_input("uptime");
        assert_eq!(sent(&workspace, cx), vec![b"uptime".to_vec(); 4]);
        click(cx, "snippet-0");
        assert_eq!(sent(&workspace, cx), vec![b"df -h\r".to_vec(); 4]);

        // Maximize shows one pane; closing one goes to main plus stack.
        click(cx, "pane-maximize-2");
        assert!(cx.debug_bounds("pane-0").is_none());
        click(cx, "pane-maximize-2");
        click(cx, "pane-close-3");
        assert_eq!(panes(cx), 3);
        assert!(cx.debug_bounds("pane-3").is_none());
    }
}
