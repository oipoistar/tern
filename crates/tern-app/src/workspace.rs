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
use tern_core::containers::Container;
use tern_core::forward::ForwardState;
use tern_core::probe::Vitals;
use tern_core::restore::{OpenTabs, Opened};
use tern_core::snippet::Snippet;
use tern_core::ssh::{Connection, parse_command};

use crate::bench;
use crate::files::FilesPanel;
use crate::live::{LiveSlots, live_layer};
use crate::marks::{self, Trace};
use crate::settings::{self, Page, SettingsEvent, SettingsView};
use crate::sync::{self, Status, SyncState};
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

/// Opens the vault open in the list: [`VaultContent::open`], or one kept
/// in memory, in tests.
type NewVault = fn(LiveSlots, &mut Window, &mut Context<VaultContent>) -> VaultContent;

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
    /// F11 (Ctrl+Cmd+F on macOS too): the window fills the screen, or stops.
    FullScreen,
    /// Shift+F11: theater mode, a session tab's terminals alone.
    Theater,
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
        "f11" if !m.control && !m.alt && !m.platform => Some(if m.shift {
            Shortcut::Theater
        } else {
            Shortcut::FullScreen
        }),
        "f" if cfg!(target_os = "macos") && m.control && m.platform && !m.shift && !m.alt => {
            Some(Shortcut::FullScreen)
        }
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
    files_open: bool,
}

impl Tab {
    fn focused(&self) -> &Entity<TerminalView> {
        &self.panes[self.focused]
    }
}

/// A host's vitals, each as its icon, a trace of its recent readings and
/// the latest value: the load (against the host's CPUs), and the shares
/// of memory and disk in use. One never read isn't shown.
fn vitals(vitals: &Vitals, theme: &Theme) -> Vec<AnyElement> {
    let cpus = vitals.cpus.max(1) as f32;
    let load = |load: f32| (format!("{load:.2}"), load / cpus);
    // Rounded up, as df does: a disk in use isn't 0% full.
    let share = |share: f32| (format!("{:.0}%", (share * 100.).ceil()), share);
    [
        (
            "vital-load",
            IconName::Cpu,
            &vitals.load,
            &load as &dyn Fn(f32) -> (String, f32),
        ),
        (
            "vital-memory",
            IconName::MemoryStick,
            &vitals.memory,
            &share,
        ),
        ("vital-disk", IconName::HardDrive, &vitals.disk, &share),
    ]
    .into_iter()
    .filter_map(|(id, glyph, readings, shown)| {
        let latest = readings.iter().rev().flatten().next().copied()?;
        let samples: Vec<Option<u32>> = readings
            .iter()
            .map(|reading| reading.map(|reading| (shown(reading).1 * 100.).round() as u32))
            .collect();
        let trace = Trace {
            status: HostStatus::online(0),
            samples: samples.into(),
            sampled_at: None,
        };
        let (ink, faint) = (theme.text, theme.text3);
        Some(
            div()
                .id(id)
                .debug_selector(move || id.into())
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .child(icon(glyph, 13., theme.text2))
                .child(
                    canvas(
                        |_, _, _| (),
                        move |bounds, _, window, _| {
                            marks::paint_trace(bounds, &trace, 0, 0., ink, faint, window)
                        },
                    )
                    .w(px(48.))
                    .h(px(16.)),
                )
                .child(
                    div()
                        .font_family(MONO)
                        .text_size(px(12.))
                        .child(shown(latest).0),
                )
                .into_any_element(),
        )
    })
    .collect()
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
    PlayRecording,
    Settings,
    /// Show a vault screen, by `--show` name.
    Screen(&'static str),
    Theme,
    FullScreen,
    Theater,
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

/// What a tab shows of a pane: live, ended or on its way, its title, and
/// what happened there while it wasn't looked at. The tabs are drawn from
/// these, never from the panes: GPUI redraws a window whenever an entity it
/// read while drawing changes, so reading a pane behind another tab would
/// redraw the window for every bit of its output.
#[derive(Clone, PartialEq)]
struct Badge {
    phase: u8,
    title: SharedString,
    rang: bool,
    finished: bool,
    unseen: bool,
}

impl Badge {
    fn of(terminal: &TerminalView) -> Self {
        Self {
            phase: phase_code(&terminal.phase),
            title: terminal.title(),
            rang: terminal.rang,
            finished: terminal.finished.is_some(),
            unseen: terminal.unseen,
        }
    }
}

/// The list of the focused host's running containers, while open.
struct ContainersMenu {
    pane: Entity<TerminalView>,
    listed: Vec<Container>,
    _listing: Option<Task<()>>,
}

pub struct Workspace {
    /// The vault open, and what it asks for.
    vault: Entity<VaultContent>,
    _vault_events: Option<Subscription>,
    filling: Option<Filling>,
    palette: Option<Palette>,
    containers: Option<ContainersMenu>,
    /// The menu a right-click on a tab opened: that tab, by a pane of it
    /// (indexes shift as tabs close), and where.
    tab_menu: Option<(EntityId, Point<Pixels>)>,
    /// Theater mode: a session tab shows its terminals alone, without the
    /// header, toolbar or bars.
    theater: bool,
    /// In theater mode, the pointer is at the window's top edge, so the way
    /// out shows.
    theater_edge: bool,
    tabs: Vec<Tab>,
    /// `None` shows the vault; `Some(i)` the i-th session tab.
    active: Option<usize>,
    /// Moving marks on the vault screen, re-registered whenever it renders.
    live: LiveSlots,
    started: Instant,
    new_terminal: NewTerminal,
    new_vault: NewVault,
    /// The Settings dialog, while open.
    settings: Option<Entity<SettingsView>>,
    _settings_events: Option<Subscription>,
    /// Where the open tabs are written down for the next start; not for
    /// tests, or a start with flags.
    tabs_file: Option<PathBuf>,
    /// The active tab and each tab's panes when they were last written.
    remembered: (Option<usize>, Vec<Vec<EntityId>>),
    /// Tern is closing, and its open sessions were written to History.
    quitting: bool,
    /// What each pane's tab showed when the window last heard from it.
    badges: HashMap<EntityId, Badge>,
    /// What this window follows of each pane, let go when the pane moves to
    /// another window.
    watching: HashMap<EntityId, Vec<Subscription>>,
    /// In a window holding tabs moved out of the main one, the main
    /// window's workspace: such a window has no vault tab, leaves the
    /// vault's requests to the main window and closes with its last tab.
    main: Option<WeakEntity<Workspace>>,
    /// In the main window, the windows its tabs moved out to, whose tabs
    /// are written down with its own.
    others: Vec<WeakEntity<Workspace>>,
    /// The tab being dragged, while the pointer is outside the window:
    /// let go there, it moves to a window of its own.
    dragged_out: Option<EntityId>,
    /// The window this is the root of, for acting on it from outside, as
    /// agents do.
    window: AnyWindowHandle,
    _ticker: Task<()>,
    _subscriptions: Vec<Subscription>,
}

/// A tab on its way to another window, by a pane of it.
#[derive(Clone)]
struct DraggedTab {
    pane: EntityId,
    title: SharedString,
}

/// What follows the pointer while a tab is dragged: the tab's title.
struct TabPreview(SharedString);

impl Render for TabPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        div()
            .h(px(34.))
            .px(px(14.))
            .flex()
            .items_center()
            .rounded(px(12.))
            .bg(theme.card)
            .border_1()
            .border_color(theme.line)
            .shadow(theme.shadow())
            .text_size(px(13.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.text)
            .child(self.0.clone())
    }
}

/// How Tern's windows open: titled, no smaller than its layouts need.
pub fn window_options(bounds: WindowBounds, focus: bool) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(bounds),
        titlebar: Some(TitlebarOptions {
            title: Some("Tern".into()),
            ..Default::default()
        }),
        window_min_size: Some(size(px(720.), px(480.))),
        focus,
        ..Default::default()
    }
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let live = LiveSlots::default();
        let vault = cx.new(|cx| VaultContent::open(live.clone(), window, cx));
        let mut workspace = Self::with(vault, live, TerminalView::new, window, cx);
        workspace.record_when_closed(window, cx);
        crate::mcp::attach(cx.weak_entity(), cx);
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
        // `tern --connect "ssh -p 2222 user@host"`, or a saved host's name,
        // opens a session at startup.
        let mut args = std::env::args()
            .skip_while(|arg| arg != "--connect")
            .skip(1);
        if let Some(command) = args.next() {
            let saved = workspace
                .vault
                .read(cx)
                .host_entries()
                .iter()
                .any(|(name, _)| *name == command);
            if saved {
                workspace
                    .vault
                    .update(cx, |vault, cx| vault.open_host(&command, window, cx));
            } else {
                match parse_command(&command) {
                    Ok((target, identity)) => {
                        workspace.open_session(Connection::typed(target, identity), window, cx)
                    }
                    Err(error) => eprintln!("--connect: {error}"),
                }
            }
        }
        // `tern --play recording.cast` plays a recording at startup.
        let mut args = std::env::args().skip_while(|arg| arg != "--play").skip(1);
        if let Some(path) = args.next() {
            workspace.play_recording(std::path::Path::new(&path), window, cx);
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
        // `tern --show [keychain|identities|snippets|forwarding|known-hosts|history|vaults]`
        // opens the vault at that screen (or the list of vaults).
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
        let vault_events = Self::follow_vault(&vault, window, cx);
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
            _vault_events: Some(vault_events),
            filling: None,
            palette: None,
            containers: None,
            tab_menu: None,
            theater: false,
            theater_edge: false,
            tabs: Vec::new(),
            active: None,
            live,
            started: Instant::now(),
            new_terminal,
            new_vault: VaultContent::open,
            settings: None,
            _settings_events: None,
            tabs_file: None,
            remembered: (None, Vec::new()),
            quitting: false,
            badges: HashMap::new(),
            watching: HashMap::new(),
            main: None,
            others: Vec::new(),
            dragged_out: None,
            window: window.window_handle(),
            _ticker: ticker,
            _subscriptions: vec![appearance, focus_lost],
        }
    }

    /// A window around `vault` whose panes don't connect, for other
    /// modules' tests.
    #[cfg(test)]
    pub fn offline(vault: tern_core::Vault, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let live = LiveSlots::default();
        let vault = cx.new(|cx| VaultContent::new(vault, None, live.clone(), window, cx));
        Self::with(vault, live, TerminalView::offline_to, window, cx)
    }

    /// A window for `tab`, moved out of `main`'s window: no vault tab of
    /// its own, the vault's requests left to the main window.
    fn holding(
        tab: Tab,
        main: WeakEntity<Workspace>,
        vault: Entity<VaultContent>,
        new_terminal: NewTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut workspace = Self::with(vault, LiveSlots::default(), new_terminal, window, cx);
        workspace._vault_events = None;
        workspace.main = Some(main);
        workspace.record_when_closed(window, cx);
        workspace.adopt_tab(tab, window, cx);
        // Its tabs are written down with the main window's, once this window
        // is done changing: writing them reads this workspace too, which
        // mustn't be in the middle of an update then (GPUI panics, and a
        // panic there ends the app).
        cx.observe_self(|this, cx| {
            let main = this.main.clone();
            cx.defer(move |cx| {
                if let Some(main) = main.and_then(|main| main.upgrade()) {
                    main.update(cx, |main, cx| main.remember_tabs(cx));
                }
            });
        })
        .detach();
        workspace
    }

    /// Sessions still open as a window closes go into History too: as it
    /// closes, or as Tern quits without that (Cmd+Q on macOS). The main
    /// window takes the others with it.
    fn record_when_closed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |_, cx| {
            let _ = this.update(cx, |workspace, cx| {
                if workspace.main.is_some() {
                    // Closing a window of moved tabs closes its tabs.
                    let panes: Vec<_> = workspace
                        .tabs
                        .iter()
                        .flat_map(|tab| tab.panes.clone())
                        .collect();
                    for pane in panes {
                        workspace.record_closed(&pane, cx);
                    }
                } else {
                    workspace.record_open_sessions(cx);
                    if workspace
                        .others
                        .iter()
                        .any(|other| other.upgrade().is_some())
                    {
                        cx.defer(|cx| cx.quit());
                    }
                }
            });
            true
        });
        self._subscriptions.push(cx.on_app_quit(|workspace, cx| {
            workspace.record_open_sessions(cx);
            async {}
        }));
    }

    /// Takes in `tab`, moved here from another window, and shows it.
    fn adopt_tab(&mut self, tab: Tab, window: &mut Window, cx: &mut Context<Self>) {
        for pane in &tab.panes {
            self.watch(pane, cx);
        }
        let files_open = tab.files_open;
        // Its Files panel is made again, for this window.
        self.tabs.push(Tab {
            files: None,
            files_open: false,
            ..tab
        });
        let index = self.tabs.len() - 1;
        self.activate(Some(index), window, cx);
        if files_open {
            self.show_files(index, cx);
        }
        self.share_sessions(cx);
    }

    /// Moves tab `index` to a window of its own, at `at` on the screen or
    /// else just beside this one. Its sessions carry on there.
    fn detach_tab(
        &mut self,
        index: usize,
        at: Option<Point<Pixels>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bounds = window.bounds();
        let tab = self.tabs.remove(index);
        for pane in &tab.panes {
            self.unwatch(pane);
            // A find bar's field belongs to this window.
            pane.update(cx, |pane, cx| pane.drop_find(cx));
        }
        if self
            .filling
            .as_ref()
            .is_some_and(|filling| tab.panes.contains(&filling.pane))
        {
            self.filling = None;
        }
        if self
            .containers
            .as_ref()
            .is_some_and(|menu| tab.panes.contains(&menu.pane))
        {
            self.containers = None;
        }
        let main = self.main.clone().unwrap_or_else(|| cx.entity().downgrade());
        let (vault, new_terminal) = (self.vault.clone(), self.new_terminal);
        let origin = at.unwrap_or(bounds.origin + point(px(48.), px(48.)));
        let options = window_options(
            WindowBounds::Windowed(Bounds::new(origin, bounds.size)),
            true,
        );
        let opened = gpui_kit::open_window(options, cx, move |window, cx| {
            cx.new(|cx| Workspace::holding(tab, main, vault, new_terminal, window, cx))
        });
        if let Ok((_, new)) = opened {
            match self.main.as_ref().and_then(WeakEntity::upgrade) {
                Some(main) => main.update(cx, |main, cx| main.adopt_window(&new, cx)),
                None => self.adopt_window(&new, cx),
            }
        }
        self.after_removing(index, window, cx);
        self.share_sessions(cx);
    }

    /// Keeps track of a window this one's tabs moved out to: its tabs are
    /// written down with these, and once it closes its sessions no longer
    /// count as open.
    fn adopt_window(&mut self, other: &Entity<Workspace>, cx: &mut Context<Self>) {
        self.others.push(other.downgrade());
        let id = other.entity_id();
        cx.observe_release(other, move |this, _, cx| {
            this.others.retain(|other| other.entity_id() != id);
            this.vault.update(cx, |vault, cx| {
                vault.set_open_sessions(id, HashMap::new(), cx)
            });
            cx.notify();
        })
        .detach();
        // Written down once nothing is mid-change: the window a tab left
        // may be another moved-out one, still being updated.
        cx.notify();
    }

    /// Does what `vault` asks: open sessions, start rules, open another
    /// vault.
    fn follow_vault(
        vault: &Entity<VaultContent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            vault,
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
                // Out of the way: a tab at the end, not shown.
                VaultEvent::Run(connection) => {
                    this.push_tab(std::slice::from_ref(connection), cx);
                    cx.notify();
                }
                VaultEvent::Forward { host, forward, on } => {
                    for pane in this.tabs.iter().flat_map(|tab| &tab.panes) {
                        let terminal = pane.read(cx);
                        if terminal.is_live() && terminal.host() == Some(host.as_str()) {
                            terminal.set_forward(forward.clone(), *on);
                        }
                    }
                }
                VaultEvent::Switch => this.switch_vault(window, cx),
            },
        )
    }

    /// Shows the vault now open in place of the one that was, with its
    /// own checks, rules and sync. Open tabs stay as they are.
    fn switch_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        sync::set_state(cx, SyncState::default());
        let new_vault = self.new_vault;
        let vault = cx.new(|cx| new_vault(self.live.clone(), window, cx));
        self._vault_events = Some(Self::follow_vault(&vault, window, cx));
        // The other windows' sessions end in the vault open now.
        for other in self.others.iter().filter_map(WeakEntity::upgrade) {
            other.update(cx, |other, cx| {
                other.vault = vault.clone();
                other.share_sessions(cx);
            });
        }
        self.vault = vault;
        let showing = self.active.is_none();
        self.vault.update(cx, |vault, _| vault.set_showing(showing));
        self.share_sessions(cx);
        if showing {
            self.vault.update(cx, |vault, cx| vault.focus(window, cx));
        }
        cx.notify();
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
            files_open: false,
        });
    }

    /// Opens the tabs written down at the last quit; hosts deleted since
    /// are left out.
    fn reopen(&mut self, open: &OpenTabs, window: &mut Window, cx: &mut Context<Self>) {
        let first = self.tabs.len();
        let (tabs, active) = self.vault.read(cx).reopened(open, cx);
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
        // The tabs moved out to other windows come back in this one.
        let others: Vec<Entity<Workspace>> =
            self.others.iter().filter_map(WeakEntity::upgrade).collect();
        let tabs: Vec<&[Entity<TerminalView>]> = self
            .tabs
            .iter()
            .chain(others.iter().flat_map(|other| &other.read(cx).tabs))
            .map(|tab| tab.panes.as_slice())
            .collect();
        let (active, panes) = &self.remembered;
        let same = *active == self.active
            && panes.len() == tabs.len()
            && tabs.iter().zip(panes).all(|(tab, ids)| {
                tab.len() == ids.len()
                    && tab
                        .iter()
                        .zip(ids)
                        .all(|(pane, id)| pane.entity_id() == *id)
            });
        if same {
            return;
        }
        let open = OpenTabs {
            tabs: tabs
                .iter()
                .map(|tab| {
                    tab.iter()
                        .map(|pane| Opened::of(pane.read(cx).connection()))
                        .collect()
                })
                .collect(),
            active: self.active,
        };
        // Not written down, the tabs only don't come back.
        let _ = open.save(file);
        let ids = tabs
            .iter()
            .map(|tab| tab.iter().map(|pane| pane.entity_id()).collect())
            .collect();
        self.remembered = (self.active, ids);
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
        } else if self.main.is_none() {
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
            action(IconName::Play, "Play a recording", Command::PlayRecording),
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
            action(IconName::Expand, "Full screen", Command::FullScreen),
        ]);
        if self.active.is_some() {
            entries.push(action(
                IconName::RectangleHorizontal,
                if self.theater {
                    "Leave theater mode"
                } else {
                    "Theater mode"
                },
                Command::Theater,
            ));
        }
        // A window of moved tabs leaves hosts and the vault's screens to
        // the main window.
        if self.main.is_some() {
            entries.retain(|entry| {
                !matches!(
                    entry.command,
                    Command::Connect(_) | Command::Tab(None) | Command::Screen(_)
                )
            });
        }
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
            Command::PlayRecording => self.choose_recording(window, cx),
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
            Command::FullScreen => window.toggle_fullscreen(),
            Command::Theater => self.toggle_theater(cx),
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
    /// The vault open.
    pub fn vault(&self) -> &Entity<VaultContent> {
        &self.vault
    }

    pub fn window_handle(&self) -> AnyWindowHandle {
        self.window
    }

    /// Each tab's panes, in order, and the tab showing.
    pub fn tab_panes(&self) -> (Vec<Vec<Entity<TerminalView>>>, Option<usize>) {
        let tabs = self.tabs.iter().map(|tab| tab.panes.clone()).collect();
        (tabs, self.active)
    }

    /// The windows the main window's tabs moved out to.
    pub fn other_windows(&self) -> Vec<Entity<Workspace>> {
        self.others.iter().filter_map(WeakEntity::upgrade).collect()
    }

    /// Opens a tab for `connection`, as opening a host does, and gives its
    /// pane: for an agent.
    pub fn open_for_agent(
        &mut self,
        connection: Connection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalView> {
        self.open_session(connection, window, cx);
        self.tabs.last().expect("a tab was just added").panes[0].clone()
    }

    /// Closes `pane`, in whichever of this window's tabs it is, as its
    /// close button would. Whether it was here.
    pub fn close_session(
        &mut self,
        pane: &Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let found = self.tabs.iter().enumerate().find_map(|(index, tab)| {
            let at = tab.panes.iter().position(|open| open == pane)?;
            Some((index, at))
        });
        let Some((index, at)) = found else {
            return false;
        };
        self.close_pane_of(index, at, window, cx);
        true
    }

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
        // The vault, then each tab, round again; a window of moved tabs has
        // no vault.
        let vault = usize::from(self.main.is_none());
        let count = self.tabs.len() + vault;
        let at = self.active.map_or(0, |index| index + vault);
        let step = |by: usize| match (at + by) % count {
            next if next < vault => None,
            next => Some(next - vault),
        };
        match shortcut {
            Shortcut::Palette => self.open_palette(window, cx),
            Shortcut::NewLocal => self.open_local(window, cx),
            Shortcut::NextTab => self.activate(step(1), window, cx),
            Shortcut::PreviousTab => self.activate(step(count - 1), window, cx),
            Shortcut::FullScreen => window.toggle_fullscreen(),
            Shortcut::Theater => self.toggle_theater(cx),
        }
        cx.stop_propagation();
    }

    /// Theater mode on or off; only a session tab has one.
    fn toggle_theater(&mut self, cx: &mut Context<Self>) {
        if self.active.is_none() {
            return;
        }
        self.theater = !self.theater;
        self.theater_edge = false;
        cx.notify();
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
        let from = cx.entity_id();
        self.vault
            .update(cx, |vault, cx| vault.set_open_sessions(from, sessions, cx));
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
            files_open: false,
        });
        self.activate(Some(self.tabs.len() - 1), window, cx);
    }

    /// Follows a pane: its state and title for the tab, its endings for
    /// History, its rules for Port forwarding, and its typing for
    /// broadcast.
    fn watch(&mut self, terminal: &Entity<TerminalView>, cx: &mut Context<Self>) {
        // A pane behind another tab redraws the window only when what its
        // tab shows changes, not for every bit of output.
        self.badges
            .insert(terminal.entity_id(), Badge::of(terminal.read(cx)));
        let watching = self.watching.entry(terminal.entity_id()).or_default();
        watching.push(cx.observe(terminal, |this, terminal, cx| {
            let badge = Badge::of(terminal.read(cx));
            let changed = this.badges.insert(terminal.entity_id(), badge.clone()) != Some(badge);
            let shown = this
                .active
                .and_then(|index| this.tabs.get(index))
                .is_some_and(|tab| tab.panes.contains(&terminal));
            if changed || shown {
                cx.notify();
            }
        }));
        watching.push(cx.subscribe(
            terminal,
            |this, terminal, event: &TerminalEvent, cx| match event {
                TerminalEvent::Finished(visit) => {
                    let visit = visit.clone();
                    let host_id = terminal.read(cx).connection().host_id.clone();
                    this.vault.update(cx, |vault, cx| {
                        if let Some(host) = &visit.host {
                            vault.clear_forward_states(host, cx);
                        }
                        vault.record_visit(visit, host_id.as_deref(), cx);
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
                // The Files panel on the pane follows its shell.
                TerminalEvent::Folder(folder) => {
                    let files = this
                        .tabs
                        .iter()
                        .filter_map(|tab| tab.files.clone())
                        .find(|files| files.read(cx).is_for(&terminal));
                    if let Some(files) = files {
                        let folder = folder.clone();
                        files.update(cx, |files, cx| files.follow(folder, cx));
                    }
                }
            },
        ));
    }

    /// Lets go of a pane closed or moved to another window: its events
    /// are no longer this window's.
    fn unwatch(&mut self, terminal: &Entity<TerminalView>) {
        self.watching.remove(&terminal.entity_id());
        self.badges.remove(&terminal.entity_id());
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
            if pane.read(cx).takes_broadcast() {
                pane.update(cx, |terminal, cx| give(terminal, cx));
            }
        }
    }

    fn activate(&mut self, tab: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        // What the tab being left and the one shown printed or rang while
        // on screen needs no marker.
        for index in [self.active, tab].into_iter().flatten() {
            if let Some(shown) = self.tabs.get(index) {
                for pane in &shown.panes {
                    pane.update(cx, |pane, _| pane.seen());
                    self.badges
                        .insert(pane.entity_id(), Badge::of(pane.read(cx)));
                }
            }
        }
        self.active = tab;
        // Marks from the previous screen must not linger over the new one.
        self.live.clear();
        // Only the main window shows the vault.
        if self.main.is_some() {
            cx.notify();
            if let Some(index) = tab {
                self.tabs[index]
                    .focused()
                    .update(cx, |terminal, cx| terminal.focus(window, cx));
            }
            return;
        }
        self.vault
            .update(cx, |vault, _| vault.set_showing(tab.is_none()));
        match tab {
            // The cached vault re-renders so its marks register again, with
            // art for any key trusted in a session meanwhile.
            None => {
                self.share_sessions(cx);
                self.vault.update(cx, |vault, cx| {
                    vault.refresh_keys(cx);
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
                pane.update(cx, |pane, _| {
                    pane.broadcasting = on;
                    // Letting a guarded pane in lasts while Broadcast does.
                    pane.let_in &= on;
                });
            }
            cx.notify();
        }
    }

    /// Lets a guarded pane of the active tab take Broadcast, or leaves it
    /// out again.
    fn toggle_let_in(&mut self, pane: usize, cx: &mut Context<Self>) {
        if let Some(index) = self.active {
            self.tabs[index].panes[pane].update(cx, |pane, cx| {
                pane.let_in = !pane.let_in;
                cx.notify();
            });
            cx.notify();
        }
    }

    /// Asks for a recording, then plays it.
    fn choose_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Play".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = chosen.await {
                let _ = this.update_in(cx, |this, window, cx| {
                    for path in paths {
                        this.play_recording(&path, window, cx);
                    }
                });
            }
        })
        .detach();
    }

    /// Plays the recording in `path` in a tab of its own; one that can't
    /// be read says why there.
    fn play_recording(
        &mut self,
        path: &std::path::Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let read = tern_core::cast::load(path);
        let title = read
            .as_ref()
            .ok()
            .and_then(|cast| cast.title.clone())
            .or_else(|| {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        let terminal = cx.new(|cx| TerminalView::replay(read, title, cx));
        self.add_session(terminal, window, cx);
    }

    /// Opens the list of the focused host's running containers, listed
    /// again as it opens.
    fn open_containers(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.active else {
            return;
        };
        let pane = self.tabs[index].focused().clone();
        let listed = pane.read(cx).containers().unwrap_or_default().to_vec();
        let listing = pane.read(cx).list_containers().map(|listing| {
            cx.spawn(async move |this, cx| {
                if let Ok(listed) = listing.await {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(menu) = &mut this.containers {
                            menu.listed = listed;
                            cx.notify();
                        }
                    });
                }
            })
        });
        self.containers = Some(ContainersMenu {
            pane,
            listed,
            _listing: listing,
        });
        cx.notify();
    }

    /// Opens a pane with a shell inside `container` beside the pane its
    /// list was opened from, or in a tab of its own when that tab is full.
    fn open_container(
        &mut self,
        container: Container,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.containers.take() else {
            return;
        };
        let mut connection = menu.pane.read(cx).connection().clone();
        // The host's own rules and one-off command stay with its pane.
        connection.forwards.clear();
        connection.once = None;
        connection.container = Some(container);
        let new_terminal = self.new_terminal;
        let terminal = cx.new(|cx| new_terminal(connection, cx));
        match self
            .tabs
            .iter()
            .position(|tab| tab.panes.contains(&menu.pane))
        {
            Some(index) if self.tabs[index].panes.len() < MAX_PANES => {
                let broadcasting = self.tabs[index].broadcast;
                terminal.update(cx, |terminal, _| terminal.broadcasting = broadcasting);
                self.watch(&terminal, cx);
                let tab = &mut self.tabs[index];
                tab.panes.push(terminal);
                tab.focused = tab.panes.len() - 1;
                tab.maximized = None;
                self.activate(Some(index), window, cx);
            }
            _ => self.add_session(terminal, window, cx),
        }
    }

    /// The Containers button, for a host with Docker or Podman, and its
    /// list while open: each running container by name and image.
    fn containers_button(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let open = self.containers.is_some();
        let listed = self
            .containers
            .as_ref()
            .map(|menu| menu.listed.clone())
            .unwrap_or_default();
        let hover = theme.card2;
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("containers")
                    .debug_selector(|| "containers".into())
                    .h(px(40.))
                    .px(px(14.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(13.))
                    .border_1()
                    .border_color(if open { theme.text } else { theme.line })
                    .bg(if open { theme.text } else { theme.card })
                    .text_color(if open { theme.card } else { theme.text })
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .child(icon(
                        IconName::Container,
                        15.,
                        if open { theme.card } else { theme.text },
                    ))
                    .child("Containers")
                    // Opens only: a click outside the list (this one too)
                    // closes it first.
                    .on_click(cx.listener(|this, _, _, cx| this.open_containers(cx))),
            )
            .when(open, |d| {
                d.child(deferred(
                    anchored().snap_to_window_with_margin(px(8.)).child(
                        div()
                            .id("containers-menu")
                            .occlude()
                            .mt(px(6.))
                            .min_w(px(260.))
                            .p(px(6.))
                            .flex()
                            .flex_col()
                            .rounded(px(14.))
                            .bg(theme.card)
                            .border_1()
                            .border_color(theme.line)
                            .shadow(theme.shadow())
                            .text_color(theme.text)
                            .text_size(px(13.))
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.containers = None;
                                cx.notify();
                            }))
                            .when(listed.is_empty(), |d| {
                                d.child(
                                    div()
                                        .h(px(36.))
                                        .px(px(10.))
                                        .flex()
                                        .items_center()
                                        .text_color(theme.text3)
                                        .child("None running"),
                                )
                            })
                            .children(listed.into_iter().enumerate().map(|(index, container)| {
                                let id = format!("container-{index}");
                                let image = container.image.clone();
                                let name = container.name.clone();
                                div()
                                    .id(SharedString::from(id.clone()))
                                    .debug_selector(move || id)
                                    .h(px(36.))
                                    .px(px(10.))
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .rounded(px(10.))
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(hover))
                                    .child(div().flex_1().truncate().child(name))
                                    .child(
                                        div()
                                            .max_w(px(180.))
                                            .truncate()
                                            .font_family(MONO)
                                            .text_size(px(12.))
                                            .text_color(theme.text3)
                                            .child(image),
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_container(container.clone(), window, cx)
                                    }))
                            })),
                    ),
                ))
            })
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

    /// Opens the Files panel of tab `index` on its focused pane's host:
    /// the one already there if it shows that pane, with SFTP opened again
    /// if the pane reconnected since, or else a new one.
    fn show_files(&mut self, index: usize, cx: &mut Context<Self>) {
        let tab = &mut self.tabs[index];
        let focused = tab.focused().clone();
        match tab
            .files
            .clone()
            .filter(|files| files.read(cx).is_for(&focused))
        {
            Some(files) => {
                files.update(cx, |files, cx| files.reopen_if_reconnected(cx));
            }
            None => {
                tab.files = Some(
                    cx.new(|cx| FilesPanel::new(&focused, FilesPanel::default_local_dir(), cx)),
                )
            }
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

    /// Writes every live session to History as closed, as Tern quits;
    /// without this the sessions still open at the end, the usual way one
    /// ends, would never be there. Kept shells keep running.
    fn record_open_sessions(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.quitting, true) {
            return;
        }
        let panes: Vec<Entity<TerminalView>> =
            self.tabs.iter().flat_map(|tab| tab.panes.clone()).collect();
        for pane in panes {
            let terminal = pane.read(cx);
            if terminal.is_live() && !terminal.connection().local && !terminal.replaying() {
                let visit = terminal.visit("Closed");
                let host_id = terminal.connection().host_id.clone();
                self.vault.update(cx, |vault, cx| {
                    vault.record_visit(visit, host_id.as_deref(), cx)
                });
            }
        }
    }

    /// Records a live pane in History as closed by hand, and ends the
    /// shell it kept on the host.
    fn record_closed(&mut self, terminal: &Entity<TerminalView>, cx: &mut Context<Self>) {
        terminal.update(cx, |terminal, _| terminal.end_kept_shell());
        let terminal = terminal.read(cx);
        if terminal.is_live() && !terminal.connection().local {
            let visit = terminal.visit("Closed");
            let host_id = terminal.connection().host_id.clone();
            self.vault.update(cx, |vault, cx| {
                if let Some(host) = &visit.host {
                    vault.clear_forward_states(host, cx);
                }
                vault.record_visit(visit, host_id.as_deref(), cx);
            });
        }
    }

    /// Closes one pane of the active tab; the last one closes the tab.
    fn close_pane(&mut self, pane: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(index) = self.active {
            self.close_pane_of(index, pane, window, cx);
        }
    }

    /// Closes one pane of tab `index`; the last one closes the tab.
    fn close_pane_of(
        &mut self,
        index: usize,
        pane: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tabs[index].panes.len() == 1 {
            return self.close_tab(index, window, cx);
        }
        let closing = self.tabs[index].panes[pane].clone();
        self.record_closed(&closing, cx);
        self.unwatch(&closing);
        // Dropping the view drops the session's input, which disconnects.
        let tab = &mut self.tabs[index];
        tab.panes.remove(pane);
        tab.maximized = None;
        if tab.focused >= pane && tab.focused > 0 {
            tab.focused -= 1;
        }
        // Its Files panel goes with it; one that was shown opens on the
        // pane that takes the keyboard.
        if tab
            .files
            .as_ref()
            .is_some_and(|files| files.read(cx).is_for(&closing))
        {
            tab.files = None;
            if tab.files_open {
                self.show_files(index, cx);
            }
        }
        // The tab on screen gives the keyboard to the pane that takes it;
        // one behind another stays there.
        if self.active == Some(index) {
            self.activate(Some(index), window, cx);
        } else {
            cx.notify();
        }
    }

    fn close_tab(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        // Live sessions closed by hand still go into History.
        for pane in self.tabs[index].panes.clone() {
            self.record_closed(&pane, cx);
            self.unwatch(&pane);
        }
        if self
            .filling
            .as_ref()
            .is_some_and(|filling| self.tabs[index].panes.contains(&filling.pane))
        {
            self.filling = None;
        }
        self.tabs.remove(index);
        self.after_removing(index, window, cx);
    }

    /// What follows once tab `index` is gone, closed or moved out: another
    /// tab shows, or the vault; a window of moved tabs with none left
    /// closes.
    fn after_removing(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        // With no session left, the next one opens with the chrome back.
        self.theater &= !self.tabs.is_empty();
        if self.main.is_some() && self.tabs.is_empty() {
            window.remove_window();
            return;
        }
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

    /// Closes every session tab but `keep`, which then shows; with none
    /// kept, the vault shows.
    fn close_tabs(&mut self, keep: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        for index in (0..self.tabs.len()).rev() {
            if Some(index) != keep {
                self.close_tab(index, window, cx);
            }
        }
        let kept = keep.and(self.tabs.first()).map(|_| 0);
        self.activate(kept, window, cx);
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
    /// panes that take Broadcast while it broadcasts; a guarded host's pane
    /// asks first.
    fn type_snippet(
        &mut self,
        index: usize,
        snippet: &Snippet,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab = &self.tabs[index];
        let focused = tab.focused();
        let panes: Vec<Entity<TerminalView>> = if tab.broadcast {
            tab.panes
                .iter()
                .filter(|pane| *pane == focused || pane.read(cx).takes_broadcast())
                .cloned()
                .collect()
        } else {
            vec![focused.clone()]
        };
        for pane in panes {
            pane.update(cx, |terminal, cx| {
                terminal.type_command(snippet.keystrokes(), cx)
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

    /// The ports the focused host opened during the session: outlined
    /// until forwarded here, inked while forwarded (opening the browser,
    /// with a stop), a danger outline when forwarding failed. It fills the
    /// toolbar's middle.
    fn opened_ports(&self, tab: &Tab, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let pane = tab.focused().clone();
        let ports = pane.read(cx).opened_ports();
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(6.))
            .overflow_hidden()
            .children(ports.into_iter().map(|(port, forwarded)| {
                let running = matches!(forwarded, Some(Some(ForwardState::Running)));
                let failed = matches!(forwarded, Some(Some(ForwardState::Failed(_))));
                let starting = matches!(forwarded, Some(None));
                let (id, stop) = (format!("port-{port}"), format!("port-stop-{port}"));
                let (to_toggle, to_stop) = (pane.clone(), pane.clone());
                div()
                    .id(SharedString::from(id.clone()))
                    .debug_selector(move || id)
                    .h(px(28.))
                    .pl(px(10.))
                    .pr(px(if running { 4. } else { 10. }))
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(4.))
                    .rounded(px(9.))
                    .border_1()
                    .border_color(if failed {
                        theme.danger
                    } else if running || starting {
                        theme.text
                    } else {
                        theme.line
                    })
                    .when(running, |d| d.bg(theme.text).text_color(theme.card))
                    .font_family(MONO)
                    .text_size(px(12.))
                    .cursor_pointer()
                    .child(format!(":{port}"))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        if running {
                            TerminalView::open_port(port, cx);
                        } else {
                            to_toggle.update(cx, |pane, cx| pane.toggle_port(port, cx));
                        }
                    }))
                    .when(running, |d| {
                        d.child(
                            div()
                                .id(SharedString::from(stop.clone()))
                                .debug_selector(move || stop)
                                .size(px(20.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(6.))
                                .child(icon(IconName::Close, 11., theme.card))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.stop_propagation();
                                    to_stop.update(cx, |pane, cx| pane.toggle_port(port, cx));
                                })),
                        )
                    })
            }))
    }

    /// Over the panes: the focused host with its latency trace, the ports
    /// it opened, the layout and Broadcast.
    fn toolbar(
        &self,
        tab: &Tab,
        full: bool,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
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
        let recorded = tab.focused().clone();
        div()
            .flex()
            .items_center()
            .gap(px(12.))
            // Being recorded: the mark, which stops it.
            .when(focused.recording(), |d| {
                d.child(
                    div()
                        .id("stop-recording")
                        .debug_selector(|| "stop-recording".into())
                        .size(px(28.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(9.))
                        .cursor_pointer()
                        .hover(|d| d.bg(theme.card2))
                        .child(icon(IconName::CircleDot, 16., theme.text))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            recorded.update(cx, |pane, cx| pane.stop_recording(cx))
                        })),
                )
            })
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
                    .child(if focused.replaying() {
                        String::new()
                    } else if local {
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
            .children(vitals(focused.vitals(), theme))
            .child(self.opened_ports(tab, theme, cx))
            // A recording has no panes to lay out or broadcast to.
            .when(!focused.replaying(), |d| {
                d.child(
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
                                        move |this, _, window, cx| {
                                            this.set_panes(count, window, cx)
                                        },
                                    ))
                                })
                        })),
                )
            })
            // How the window shows the tab: full screen, and theater mode.
            .child(
                div()
                    .flex()
                    .gap(px(4.))
                    .p(px(4.))
                    .rounded(px(14.))
                    .bg(theme.card2)
                    .child(
                        view_button(
                            "full-screen",
                            if full {
                                IconName::Shrink
                            } else {
                                IconName::Expand
                            },
                            full,
                            theme,
                        )
                        .on_click(|_, window, _| window.toggle_fullscreen()),
                    )
                    .child(
                        view_button("theater", IconName::RectangleHorizontal, false, theme)
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_theater(cx))),
                    ),
            )
            .when(focused.containers().is_some(), |d| {
                d.child(self.containers_button(theme, cx))
            })
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
            .when(!focused.replaying(), |d| {
                d.child(
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
            })
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

    /// A pane: an ink ring while focused (every pane Broadcast reaches
    /// while broadcasting), a double hairline inside it for a guarded host,
    /// and with others beside it a header with its host, Maximize and
    /// Close, and while broadcasting a guarded host's way in.
    fn pane(&self, tab: &Tab, index: usize, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let terminal = &tab.panes[index];
        let (guarded, let_in) = {
            let pane = terminal.read(cx);
            (pane.guarded(), pane.let_in)
        };
        let reached = tab.broadcast && (!guarded || let_in);
        let ringed = reached || (tab.focused == index && tab.panes.len() > 1);
        let id = format!("pane-{index}");
        let (maximize, close, way_in) = (
            format!("pane-maximize-{index}"),
            format!("pane-close-{index}"),
            format!("pane-let-in-{index}"),
        );
        let hairline = |d: Div, radius: f32| {
            d.flex_1().min_h_0().flex().flex_col().when(guarded, |d| {
                d.rounded(px(radius)).border_1().border_color(theme.text)
            })
        };
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
            .child(
                hairline(div(), 17.)
                    .when(guarded, |d| d.m(px(2.)).p(px(2.)))
                    .child(
                        hairline(div(), 14.)
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
                                        .child(phase_glyph(
                                            phase_code(&terminal.read(cx).phase),
                                            theme,
                                        ))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .child(terminal.read(cx).label()),
                                        )
                                        .when(tab.broadcast && guarded, |d| {
                                            d.child(
                                                div()
                                                    .id(SharedString::from(way_in.clone()))
                                                    .debug_selector(move || way_in)
                                                    .size(px(26.))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded(px(8.))
                                                    .cursor_pointer()
                                                    .when(let_in, |d| d.bg(theme.text))
                                                    .child(icon(
                                                        IconName::RadioTower,
                                                        13.,
                                                        if let_in {
                                                            theme.card
                                                        } else {
                                                            theme.text2
                                                        },
                                                    ))
                                                    .on_click(cx.listener(
                                                        move |this, _, _, cx| {
                                                            this.toggle_let_in(index, cx)
                                                        },
                                                    )),
                                            )
                                        })
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
                                            .on_click(
                                                cx.listener(move |this, _, window, cx| {
                                                    this.toggle_maximized(index, window, cx)
                                                }),
                                            ),
                                        )
                                        .child(
                                            pane_button(close, IconName::Close, theme).on_click(
                                                cx.listener(move |this, _, window, cx| {
                                                    this.close_pane(index, window, cx)
                                                }),
                                            ),
                                        ),
                                )
                            })
                            .child(
                                div().flex_1().min_h_0().child(
                                    terminal
                                        .clone()
                                        .cached(StyleRefinement::default().size_full()),
                                ),
                            ),
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
            // A window of moved tabs has no vault.
            .when(self.main.is_none(), |d| {
                d.child(
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
                        .on_click(
                            cx.listener(|this, _, window, cx| this.activate(None, window, cx)),
                        ),
                )
            })
            .children(self.tabs.iter().enumerate().map(|(index, session)| {
                let focused = session.focused();
                let badge = self
                    .badges
                    .get(&focused.entity_id())
                    .cloned()
                    .unwrap_or_else(|| Badge::of(focused.read(cx)));
                let pane = focused.entity_id();
                tab(
                    SharedString::from(format!("tab-{pane}")),
                    self.active == Some(index),
                )
                .debug_selector(move || format!("tab-{index}"))
                // Dragged out of the window, it goes to a window of its own.
                .on_drag(
                    DraggedTab {
                        pane,
                        title: badge.title.clone(),
                    },
                    |dragged, _, _, cx| cx.new(|_| TabPreview(dragged.title.clone())),
                )
                .pr(px(4.))
                .child(phase_glyph(badge.phase, theme))
                .child(div().max_w(px(180.)).truncate().child(badge.title))
                // A tab not on screen shows a bell if one rang there, a check
                // if a long command finished, or a dot for output it hasn't
                // shown.
                .when(self.active != Some(index), |d| {
                    let panes = session
                        .panes
                        .iter()
                        .filter_map(|pane| self.badges.get(&pane.entity_id()));
                    let (rang, finished, unseen) =
                        panes.fold((false, false, false), |(rang, finished, unseen), pane| {
                            (
                                rang || pane.rang,
                                finished || pane.finished,
                                unseen || pane.unseen,
                            )
                        });
                    let id = format!("activity-{index}");
                    match (rang, finished, unseen) {
                        (true, _, _) => d.child(div().debug_selector(move || id).child(icon(
                            IconName::Bell,
                            12.,
                            theme.text,
                        ))),
                        (false, true, _) => d.child(div().debug_selector(move || id).child(icon(
                            IconName::Check,
                            12.,
                            theme.text,
                        ))),
                        (false, false, true) => d.child(
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
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        this.tab_menu = Some((pane, event.position));
                        cx.notify();
                    }),
                )
                // The middle button closes it, as in a browser.
                .on_mouse_up(
                    MouseButton::Middle,
                    cx.listener(move |this, _: &MouseUpEvent, window, cx| {
                        if let Some(index) = this.tab_holding(pane) {
                            this.close_tab(index, window, cx);
                        }
                    }),
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

    /// The session tab holding the pane `id`.
    fn tab_holding(&self, id: EntityId) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.panes.iter().any(|pane| pane.entity_id() == id))
    }

    /// What a right-click on a session tab offers: closing it, the others,
    /// or all of them.
    fn tab_menu_view(
        &self,
        pane: EntityId,
        at: Point<Pixels>,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let item = |id: &'static str, label: &'static str, enabled: bool| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .h(px(32.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(px(9.))
                .text_color(if enabled { theme.text } else { theme.text3 })
                .when(enabled, |d| d.cursor_pointer().hover(|d| d.bg(theme.card2)))
                .child(label)
        };
        let others = self.tabs.len() > 1;
        deferred(
            anchored()
                .position(at)
                .snap_to_window_with_margin(px(8.))
                .child(
                    div()
                        .id("tab-menu")
                        .occlude()
                        .w(px(200.))
                        .p(px(6.))
                        .flex()
                        .flex_col()
                        .rounded(px(14.))
                        .bg(theme.card)
                        .border_1()
                        .border_color(theme.line)
                        .shadow(theme.shadow())
                        .text_size(px(13.))
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.tab_menu = None;
                            cx.notify();
                        }))
                        .child(
                            item("tab-menu-window", "Move to a new window", true).on_click(
                                cx.listener(move |this, _, window, cx| {
                                    this.tab_menu = None;
                                    if let Some(index) = this.tab_holding(pane) {
                                        this.detach_tab(index, None, window, cx);
                                    }
                                }),
                            ),
                        )
                        .child(div().my(px(4.)).h(px(1.)).bg(theme.line))
                        .child(
                            item("tab-menu-close", "Close tab", true).on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.tab_menu = None;
                                    if let Some(index) = this.tab_holding(pane) {
                                        this.close_tab(index, window, cx);
                                    }
                                },
                            )),
                        )
                        .child(item("tab-menu-others", "Close other tabs", others).when(
                            others,
                            |d| {
                                d.on_click(cx.listener(move |this, _, window, cx| {
                                    this.tab_menu = None;
                                    if let Some(index) = this.tab_holding(pane) {
                                        this.close_tabs(Some(index), window, cx);
                                    }
                                }))
                            },
                        ))
                        .child(
                            item("tab-menu-all", "Close all tabs", true).on_click(cx.listener(
                                |this, _, window, cx| {
                                    this.tab_menu = None;
                                    this.close_tabs(None, window, cx);
                                },
                            )),
                        ),
                ),
        )
        .into_any_element()
    }

    /// In theater mode, the way back: the toggle, shown while the pointer
    /// is at the window's top edge.
    fn theater_exit(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        div()
            .id("theater-edge")
            .debug_selector(|| "theater-edge".into())
            .absolute()
            .top_0()
            .left_0()
            .w_full()
            // Once the toggle shows, deep enough to reach it.
            .h(px(if self.theater_edge { 64. } else { 8. }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.theater_edge = *hovered;
                cx.notify();
            }))
            .when(self.theater_edge, |d| {
                d.child(
                    div()
                        .absolute()
                        .top(px(12.))
                        .right(px(12.))
                        .p(px(4.))
                        .rounded(px(14.))
                        .bg(theme.card2)
                        .shadow(theme.shadow())
                        .child(
                            view_button(
                                "leave-theater",
                                IconName::RectangleHorizontal,
                                true,
                                theme,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_theater(cx))),
                        ),
                )
            })
    }
}

/// A session's state as a glyph: a solid dot while live, a ring once
/// ended, a spinner on the way.
/// A pane's phase as a tab tells it: live, ended or on its way.
fn phase_code(phase: &Phase) -> u8 {
    match phase {
        Phase::Live => 0,
        Phase::Ended(_) => 1,
        _ => 2,
    }
}

/// The glyph for a [`phase_code`].
fn phase_glyph(phase: u8, theme: &Theme) -> AnyElement {
    match phase {
        0 => status_dot(theme).into_any_element(),
        1 => div()
            .size(px(7.))
            .rounded_full()
            .border_1()
            .border_color(theme.text3)
            .into_any_element(),
        _ => icon(IconName::LoaderCircle, 12., theme.text2).into_any_element(),
    }
}

/// A toggle in a toolbar group, raised while on, as the chosen layout is.
fn view_button(id: &'static str, glyph: IconName, on: bool, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .debug_selector(move || id.into())
        .size(px(32.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(10.))
        .cursor_pointer()
        .when(on, |d| d.bg(theme.card).shadow(theme.shadow_sm()))
        .child(icon(glyph, 16., if on { theme.text } else { theme.text2 }))
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
        let theater = self.theater && self.active.is_some();
        let body = match self.active {
            None if self.main.is_some() => div().into_any_element(),
            None if bench::uncached() => self.vault.clone().into_any_element(),
            None => self.vault.clone().cached(cached).into_any_element(),
            // The terminals alone, nearly to the window's edges.
            Some(index) if theater => div()
                .size_full()
                .p(px(8.))
                .flex()
                .child(self.panes(&self.tabs[index], &theme, cx))
                .into_any_element(),
            Some(index) => {
                let tab = &self.tabs[index];
                div()
                    .size_full()
                    .p(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(self.toolbar(tab, window.is_fullscreen(), &theme, cx))
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
            // A tab dragged out of the window and let go there moves to a
            // window of its own, under the pointer.
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DraggedTab>, window, cx| {
                    let inside = Bounds::new(Point::default(), window.viewport_size())
                        .contains(&event.event.position);
                    this.dragged_out = (!inside).then(|| event.drag(cx).pane);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, window, cx| {
                    let Some(pane) = this.dragged_out.take() else {
                        return;
                    };
                    if let Some(index) = this.tab_holding(pane) {
                        let at = window.bounds().origin + event.position - point(px(160.), px(28.));
                        this.detach_tab(index, Some(at), window, cx);
                    }
                }),
            )
            // A recording dropped on the window plays.
            .on_drop(cx.listener(|this, dropped: &ExternalPaths, window, cx| {
                for path in dropped.paths() {
                    if path
                        .extension()
                        .is_some_and(|extension| extension == "cast")
                    {
                        this.play_recording(path, window, cx);
                    }
                }
            }))
            .bg(theme.win)
            .text_color(theme.text)
            .text_size(px(14.))
            .when(!theater, |d| d.child(self.header(&theme, cx)))
            .child(div().flex_1().min_h_0().child(body))
            .child(live_layer(&self.live, time))
            .when(theater, |d| d.child(self.theater_exit(&theme, cx)))
            .when_some(self.palette.as_ref(), |d, palette| {
                d.child(self.palette_view(palette, &theme, cx))
            })
            .when_some(
                self.tab_menu
                    .filter(|(pane, _)| self.tab_holding(*pane).is_some()),
                |d, (pane, at)| d.child(self.tab_menu_view(pane, at, &theme, cx)),
            )
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
    use gpui_kit::{App, AppContext as _, Context, Entity, WeakEntity};
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
    fn tabs_move_out_to_windows_of_their_own(cx: &mut TestAppContext) {
        use gpui::{MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent};
        use tern_core::restore::OpenTabs;

        let dir = std::env::temp_dir().join(format!("tern-tab-windows-{}", std::process::id()));
        let file = dir.join("tabs.json");
        let (workspace, cx) = open_workspace(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.remember_tabs_in(file.clone(), cx)
        });
        click(cx, "new-local");
        let pane = |cx: &mut VisualTestContext, index: usize| {
            workspace.read_with(cx, |w, _| w.tabs[index].focused().entity_id())
        };
        let (first, second) = (pane(cx, 0), pane(cx, 1));
        let windows = |cx: &mut VisualTestContext| cx.update(|_, cx| cx.windows().len());
        let others = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |w, _| {
                w.others
                    .iter()
                    .filter_map(WeakEntity::upgrade)
                    .collect::<Vec<_>>()
            })
        };
        let none = Modifiers::none();

        // From the tab's menu: the tab goes, its session with it.
        let at = cx.debug_bounds("tab-1").unwrap().center();
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Right,
            position: at,
            modifiers: none,
            click_count: 1,
            first_mouse: false,
        });
        click(cx, "tab-menu-window");
        assert_eq!(windows(cx), 2);
        assert_eq!(workspace.read_with(cx, |w, _| w.tabs.len()), 1);
        let moved = others(cx);
        moved[0].read_with(cx, |other, _| {
            assert!(other.main.is_some(), "a window of moved tabs has no vault");
            assert_eq!(other.tabs[0].focused().entity_id(), second);
            assert_eq!(other.active, Some(0));
        });
        // Both still come back at the next start.
        assert_eq!(OpenTabs::load(&file).tabs.len(), 2);
        drop(moved);

        // Dragged past the window's edge and let go, likewise.
        let start = cx.debug_bounds("tab-0").unwrap().center();
        let outside = gpui::point(gpui::px(-80.), gpui::px(12.));
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: start,
            modifiers: none,
            click_count: 1,
            first_mouse: false,
        });
        for position in [start + gpui::point(gpui::px(24.), gpui::px(0.)), outside] {
            cx.simulate_event(MouseMoveEvent {
                position,
                pressed_button: Some(MouseButton::Left),
                modifiers: none,
            });
        }
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: outside,
            modifiers: none,
            click_count: 1,
        });
        cx.run_until_parked();
        assert_eq!(windows(cx), 3);
        assert_eq!(
            workspace.read_with(cx, |w, _| (w.tabs.len(), w.active)),
            (0, None)
        );
        let moved = others(cx);
        assert_eq!(
            moved[1].read_with(cx, |other, _| other.tabs[0].focused().entity_id()),
            first
        );
        assert_eq!(OpenTabs::load(&file).tabs.len(), 2);

        // Its last tab closed, a window of moved tabs closes too.
        let main = cx.update(|window, _| window.window_handle());
        let window = cx.update(|_, cx| {
            cx.windows()
                .into_iter()
                .find(|window| {
                    *window != main && window.update(cx, |_, window, _| window.bounds()).is_ok()
                })
                .unwrap()
        });
        let holding = |other: &Entity<Workspace>, cx: &App| {
            other
                .read(cx)
                .tabs
                .iter()
                .any(|tab| tab.panes.iter().any(|pane| pane.entity_id() == second))
        };
        cx.update(|_, cx| {
            window
                .update(cx, |_, window, cx| {
                    for other in &moved {
                        if holding(other, cx) {
                            other.update(cx, |other, cx| other.close_tab(0, window, cx));
                        }
                    }
                })
                .unwrap()
        });
        // Only the windows hold their workspaces.
        drop(moved);
        cx.run_until_parked();
        assert_eq!(windows(cx), 2);
        assert_eq!(others(cx).len(), 1);
        assert_eq!(OpenTabs::load(&file).tabs.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn a_split_tab_dragged_out_leaves_the_main_window_working(cx: &mut TestAppContext) {
        use gpui::{MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent};
        use tern_core::restore::OpenTabs;

        let dir = std::env::temp_dir().join(format!("tern-split-out-{}", std::process::id()));
        let file = dir.join("tabs.json");
        let (workspace, cx) = open_workspace(cx);
        // Open tabs are written down, as in a plain start.
        workspace.update(cx, |workspace, cx| {
            workspace.remember_tabs_in(file.clone(), cx)
        });
        // Two sessions side by side in one tab, one of them focused.
        click(cx, "layout-2");
        click(cx, "pane-1");
        let none = Modifiers::none();
        let start = cx.debug_bounds("tab-0").unwrap().center();
        let outside = gpui::point(gpui::px(-80.), gpui::px(12.));
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position: start,
            modifiers: none,
            click_count: 1,
            first_mouse: false,
        });
        for position in [start + gpui::point(gpui::px(24.), gpui::px(0.)), outside] {
            cx.simulate_event(MouseMoveEvent {
                position,
                pressed_button: Some(MouseButton::Left),
                modifiers: none,
            });
        }
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position: outside,
            modifiers: none,
            click_count: 1,
        });
        cx.run_until_parked();
        assert_eq!(cx.update(|_, cx| cx.windows().len()), 2);
        assert_eq!(
            workspace.read_with(cx, |w, _| (w.tabs.len(), w.active)),
            (0, None)
        );
        let moved = workspace.read_with(cx, |w, _| w.others[0].upgrade().unwrap());
        assert_eq!(moved.read_with(cx, |other, _| other.tabs[0].panes.len()), 2);
        // The moved window changing (a pane printing, its focus going) has
        // the tabs written down again, and reads that window to do it.
        moved.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(OpenTabs::load(&file).tabs.len(), 1);

        // The main window shows the vault and goes on taking clicks.
        assert!(cx.debug_bounds("new-host").is_some());
        let middle = cx.update(|window, _| window.viewport_size().center());
        cx.simulate_click(middle, none);
        cx.run_until_parked();
        click(cx, "new-host");
        click(cx, "filter");
        assert!(cx.debug_bounds("new-host").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn a_moved_tab_moves_on_again(cx: &mut TestAppContext) {
        use tern_core::restore::OpenTabs;

        let dir = std::env::temp_dir().join(format!("tern-tab-on-{}", std::process::id()));
        let file = dir.join("tabs.json");
        let (workspace, cx) = open_workspace(cx);
        workspace.update(cx, |workspace, cx| {
            workspace.remember_tabs_in(file.clone(), cx)
        });
        click(cx, "new-local");
        let moving = workspace.read_with(cx, |w, _| w.tabs[1].focused().entity_id());
        cx.update(|window, cx| workspace.update(cx, |w, cx| w.detach_tab(1, None, window, cx)));
        cx.run_until_parked();
        let main = cx.update(|window, _| window.window_handle());
        let first = workspace.read_with(cx, |w, _| w.others[0].upgrade().unwrap());
        let window = cx.update(|_, cx| {
            cx.windows()
                .into_iter()
                .find(|window| *window != main)
                .unwrap()
        });

        // Its window has no other tab, so it closes as a new one opens.
        cx.update(|_, cx| {
            window
                .update(cx, |_, window, cx| {
                    first.update(cx, |first, cx| first.detach_tab(0, None, window, cx))
                })
                .unwrap()
        });
        drop(first);
        cx.run_until_parked();
        assert_eq!(cx.update(|_, cx| cx.windows().len()), 2);
        let others: Vec<_> = workspace.read_with(cx, |w, _| {
            w.others.iter().filter_map(WeakEntity::upgrade).collect()
        });
        assert_eq!(others.len(), 1);
        assert_eq!(
            others[0].read_with(cx, |other, _| other.tabs[0].focused().entity_id()),
            moving
        );
        assert_eq!(OpenTabs::load(&file).tabs.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn the_middle_button_closes_a_tab(cx: &mut TestAppContext) {
        use gpui::{MouseButton, MouseDownEvent, MouseUpEvent};

        let (workspace, cx) = open_workspace(cx);
        click(cx, "new-local");
        let panes = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let panes: Vec<_> = workspace
                    .tabs
                    .iter()
                    .map(|tab| tab.focused().entity_id())
                    .collect();
                panes
            })
        };
        let before = panes(cx);
        let at = cx.debug_bounds("tab-0").unwrap().center();
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Middle,
            position: at,
            modifiers: Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Middle,
            position: at,
            modifiers: Modifiers::none(),
            click_count: 1,
        });
        cx.run_until_parked();
        assert_eq!(panes(cx), [before[1]]);
    }

    #[gpui::test]
    fn full_screen_and_theater_mode(cx: &mut TestAppContext) {
        let (workspace, cx) = open_workspace(cx);
        cx.update(|window, cx| {
            let pane = workspace.read(cx).tabs[0].focused().clone();
            pane.update(cx, |pane, cx| pane.focus(window, cx));
        });
        let full = |cx: &mut VisualTestContext| cx.update(|window, _| window.is_fullscreen());

        // F11 fills the screen, and again stops; so does the button.
        cx.simulate_keystrokes("f11");
        assert!(full(cx));
        cx.simulate_keystrokes("f11");
        assert!(!full(cx));
        click(cx, "full-screen");
        assert!(full(cx));
        click(cx, "full-screen");
        assert!(!full(cx));

        // Theater mode: the terminals alone, no header, toolbar or bars.
        click(cx, "theater");
        cx.run_until_parked();
        assert!(cx.debug_bounds("pane-0").is_some());
        for chrome in ["new-local", "broadcast", "theater"] {
            assert!(cx.debug_bounds(chrome).is_none(), "{chrome} still shows");
        }
        // The way back shows at the top edge.
        assert!(cx.debug_bounds("leave-theater").is_none());
        cx.simulate_mouse_move(
            gpui::point(gpui::px(300.), gpui::px(3.)),
            None,
            Modifiers::none(),
        );
        click(cx, "leave-theater");
        assert!(cx.debug_bounds("new-local").is_some());

        // Shift+F11 too, on a session tab only.
        cx.simulate_keystrokes("shift-f11");
        assert!(cx.debug_bounds("new-local").is_none());
        cx.simulate_keystrokes("shift-f11");
        assert!(cx.debug_bounds("new-local").is_some());
        cx.simulate_keystrokes("ctrl-tab");
        assert_eq!(workspace.read_with(cx, |w, _| w.active), None);
        cx.simulate_keystrokes("shift-f11");
        assert!(!workspace.read_with(cx, |w, _| w.theater));
    }

    #[gpui::test]
    fn a_tabs_menu_closes_the_others_or_all(cx: &mut TestAppContext) {
        use gpui::{MouseButton, MouseDownEvent};

        let (workspace, cx) = open_workspace(cx);
        click(cx, "new-local");
        click(cx, "new-local");
        let panes = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let panes: Vec<_> = workspace
                    .tabs
                    .iter()
                    .map(|tab| tab.focused().entity_id())
                    .collect();
                (panes, workspace.active)
            })
        };
        let menu = |cx: &mut VisualTestContext, tab: &'static str, item: &'static str| {
            cx.run_until_parked();
            let at = cx
                .debug_bounds(tab)
                .unwrap_or_else(|| panic!("nothing on screen is tagged {tab}"))
                .center();
            cx.simulate_event(MouseDownEvent {
                button: MouseButton::Right,
                position: at,
                modifiers: Modifiers::none(),
                click_count: 1,
                first_mouse: false,
            });
            click(cx, item);
        };
        let (before, _) = panes(cx);
        assert_eq!(before.len(), 3);

        // The others close; the one asked about stays, and shows.
        menu(cx, "tab-1", "tab-menu-others");
        assert_eq!(panes(cx), (vec![before[1]], Some(0)));
        // With no others, there's nothing for that item to do.
        menu(cx, "tab-0", "tab-menu-others");
        assert_eq!(panes(cx), (vec![before[1]], Some(0)));

        click(cx, "new-local");
        menu(cx, "tab-0", "tab-menu-all");
        assert_eq!(panes(cx), (vec![], None));
        assert!(cx.debug_bounds("tab-menu-all").is_none());
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
    fn guarded_hosts_ask_first_and_broadcast_leaves_them_out(cx: &mut TestAppContext) {
        use gpui_kit::ClipboardItem;
        use tern_core::ssh::Target;

        let (workspace, cx) = open_workspace(cx);
        let target = |host: &str| Target {
            host: host.into(),
            port: 22,
            user: "deploy".into(),
        };
        let guarded = Connection {
            guarded: true,
            once: Some("make deploy".into()),
            ..Connection::typed(target("db-01"), None)
        };
        let plain = Connection::typed(target("web-01"), None);
        workspace.update(cx, |workspace, cx| {
            workspace.vault.update(cx, |_, cx| {
                cx.emit(VaultEvent::OpenTogether(vec![plain, guarded]))
            })
        });
        cx.run_until_parked();
        let panes = workspace.read_with(cx, |workspace, _| workspace.tabs[1].panes.clone());
        let sent = |cx: &mut VisualTestContext| -> Vec<Vec<u8>> {
            panes
                .iter()
                .map(|pane| pane.read_with(cx, |pane, _| pane.sent.take()))
                .collect()
        };
        let nothing = || [Vec::<u8>::new(), Vec::new()];

        // A command run from elsewhere waits for a yes.
        assert_eq!(sent(cx), nothing());
        click(cx, "paste-confirm");
        assert_eq!(sent(cx), [Vec::new(), b"make deploy\r".to_vec()]);

        // Broadcast leaves it out until it's let in.
        cx.update(|window, cx| panes[0].update(cx, |pane, cx| pane.focus(window, cx)));
        click(cx, "broadcast");
        cx.simulate_input("ls");
        assert_eq!(sent(cx), [b"ls".to_vec(), Vec::new()]);
        click(cx, "pane-let-in-1");
        cx.update(|window, cx| panes[0].update(cx, |pane, cx| pane.focus(window, cx)));
        cx.simulate_input("pwd");
        assert_eq!(sent(cx), [b"pwd".to_vec(), b"pwd".to_vec()]);
        // A snippet still asks there; Cancel drops it.
        click(cx, "snippet-0");
        assert_eq!(sent(cx), [b"df -h\r".to_vec(), Vec::new()]);
        click(cx, "paste-cancel");
        assert_eq!(sent(cx), nothing());
        // Letting it in lasts while Broadcast does.
        click(cx, "broadcast");
        panes[1].read_with(cx, |pane, _| assert!(!pane.let_in));

        // Even one pasted line asks.
        cx.update(|window, cx| panes[1].update(cx, |pane, cx| pane.focus(window, cx)));
        cx.write_to_clipboard(ClipboardItem::new_string("uptime".into()));
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-v"
        } else {
            "ctrl-shift-v"
        });
        assert_eq!(sent(cx), nothing());
        cx.simulate_keystrokes("enter");
        assert_eq!(sent(cx), [Vec::new(), b"uptime".to_vec()]);
    }

    #[gpui::test]
    fn the_files_panel_follows_its_shell(cx: &mut TestAppContext) {
        use tern_core::ssh::Event;

        let (workspace, cx) = open_workspace(cx);
        click(cx, "toggle-files");
        let pane = workspace.read_with(cx, |workspace, _| workspace.tabs[0].focused().clone());
        pane.update(cx, |pane, cx| {
            let moved = b"\x1b]7;file://web-01/srv/my%20app\x07$ ".to_vec();
            pane.on_session_event(Event::Output(moved), cx)
        });
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            let files = workspace.tabs[0]
                .files
                .as_ref()
                .expect("Files is open")
                .read(cx);
            // Before SFTP opens, it'll start there.
            assert_eq!(files.following(), Some("/srv/my app"));
            assert_eq!(pane.read(cx).folder(), Some("/srv/my app"));
        });
    }

    #[gpui::test]
    fn switches_between_vaults_keeping_tabs_open(cx: &mut TestAppContext) {
        use gpui::{Context, Window};

        use crate::vaults;

        /// The first vault is the sample, any other starts empty.
        fn in_memory(
            live: LiveSlots,
            window: &mut Window,
            cx: &mut Context<VaultContent>,
        ) -> VaultContent {
            let vault = if vaults::current(cx) == tern_core::vaults::FIRST {
                Vault::sample()
            } else {
                Vault::default()
            };
            VaultContent::new(vault, None, live, window, cx)
        }

        let (workspace, cx) = open_workspace(cx);
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.new_vault = in_memory;
            workspace.activate(None, window, cx);
        });
        let hosts = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace.vault.read(cx).host_entries().len()
            })
        };
        let open =
            |cx: &mut VisualTestContext| cx.update(|_, cx| vaults::list(cx).current().name.clone());
        let sample = hosts(cx);
        assert!(sample > 0);

        // A new vault opens, empty, and the session tab stays.
        click(cx, "vaults");
        click(cx, "vault-new");
        cx.simulate_input("Work");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(open(cx), "Work");
        assert_eq!(hosts(cx), 0);
        workspace.read_with(cx, |workspace, _| assert_eq!(workspace.tabs.len(), 1));

        // Back to the first, renamed.
        click(cx, "vaults");
        click(cx, "vault-Personal");
        assert_eq!((open(cx), hosts(cx)), ("Personal".to_string(), sample));
        click(cx, "vaults");
        click(cx, "vault-rename");
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        });
        cx.simulate_input("Home");
        cx.simulate_keystrokes("enter");
        assert_eq!(open(cx), "Home");

        // The other one deleted, after a second click.
        click(cx, "vault-delete-Work");
        assert_eq!(cx.update(|_, cx| vaults::list(cx).vaults.len()), 2);
        click(cx, "vault-delete-confirm");
        assert_eq!(cx.update(|_, cx| vaults::list(cx).vaults.len()), 1);
        assert!(cx.debug_bounds("vault-Work").is_none());
    }

    #[gpui::test]
    fn output_behind_another_tab_redraws_only_its_mark(cx: &mut TestAppContext) {
        use std::cell::Cell;
        use std::rc::Rc;

        use tern_core::ssh::Event;

        let (workspace, cx) = open_workspace(cx);
        let behind = workspace.read_with(cx, |workspace, _| workspace.tabs[0].focused().clone());
        behind.update(cx, |pane, cx| pane.on_session_event(Event::Connected, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            let terminal = cx.new(TerminalView::offline);
            workspace.add_session(terminal, window, cx);
        });
        cx.run_until_parked();
        let redraws = Rc::new(Cell::new(0));
        let counted = redraws.clone();
        let _counting =
            cx.update(|_, cx| cx.observe(&workspace, move |_, _| counted.set(counted.get() + 1)));

        // The first output marks the tab; the rest changes nothing it shows.
        for line in 0..20 {
            let bytes = format!("line {line}\r\n").into_bytes();
            behind.update(cx, |pane, cx| {
                pane.on_session_event(Event::Output(bytes), cx)
            });
            cx.run_until_parked();
        }
        assert_eq!(redraws.get(), 1);
        assert!(cx.debug_bounds("activity-0").is_some());
    }

    #[gpui::test]
    fn a_host_back_opens_its_tab_out_of_the_way(cx: &mut TestAppContext) {
        let (workspace, cx) = open_workspace(cx);
        let (vault, active) = workspace.read_with(cx, |workspace, _| {
            (workspace.vault.clone(), workspace.active)
        });
        let sample = Vault::sample();
        let mut connection = sample.connection(sample.find("bastion").unwrap()).unwrap();
        connection.once = Some("uptime".into());
        vault.update(cx, |_, cx| cx.emit(VaultEvent::Run(connection)));
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(workspace.tabs.len(), 2);
            assert_eq!(workspace.active, active);
            let pane = workspace.tabs[1].focused().read(cx);
            assert_eq!(pane.connection().host.as_deref(), Some("bastion"));
        });
    }

    #[gpui::test]
    fn sessions_open_as_tern_closes_go_into_history(cx: &mut TestAppContext) {
        use tern_core::ssh::Event;

        let (workspace, cx) = open_workspace(cx);
        // A second tab whose pane never got a shell isn't a visit.
        click(cx, "layout-2");
        let panes = workspace.read_with(cx, |workspace, _| workspace.tabs[0].panes.clone());
        panes[1].update(cx, |pane, cx| {
            pane.on_session_event(
                Event::Failed(tern_core::ssh::ConnectError::Other("refused".into())),
                cx,
            )
        });
        let visits = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .vault
                    .read(cx)
                    .visits()
                    .iter()
                    .map(|visit| visit.ended.clone())
                    .collect::<Vec<_>>()
            })
        };
        let before = visits(cx);
        workspace.update(cx, |workspace, cx| workspace.record_open_sessions(cx));
        let after = visits(cx);
        assert_eq!(after.len(), before.len() + 1);
        assert_eq!(after[0], "Closed");
        // Closing and quitting both ask: it's written once.
        workspace.update(cx, |workspace, cx| workspace.record_open_sessions(cx));
        assert_eq!(visits(cx).len(), after.len());
    }

    #[gpui::test]
    fn records_a_session_and_plays_it_back(cx: &mut TestAppContext) {
        use gpui::{point, px};
        use tern_core::ssh::Event;

        let dir = std::env::temp_dir().join(format!("tern-ui-cast-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("web.cast");
        let (workspace, cx) = open_workspace(cx);
        let pane = workspace.read_with(cx, |workspace, _| workspace.tabs[0].focused().clone());
        pane.update(cx, |pane, cx| {
            pane.record_to(&path, cx);
            pane.on_session_event(Event::Output(b"hello\r\n".to_vec()), cx);
            pane.on_session_event(Event::Output(b"world".to_vec()), cx);
        });
        // The mark stops it.
        click(cx, "stop-recording");
        assert!(!pane.read_with(cx, |pane, _| pane.recording()));
        let cast = tern_core::cast::read(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cast.events.len(), 2);

        // Played back in a tab of its own; the track moves it.
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.play_recording(&path, window, cx)
            })
        });
        let replay = workspace.read_with(cx, |workspace, _| workspace.tabs[1].focused().clone());
        assert!(replay.read_with(cx, |replay, _| replay.replaying()));
        cx.run_until_parked();
        let track = cx.debug_bounds("replay-track").expect("the track");
        let shown =
            |cx: &mut VisualTestContext| replay.read_with(cx, |replay, _| replay.output_text());
        cx.simulate_click(
            point(track.right() - px(1.), track.center().y),
            Modifiers::none(),
        );
        assert!(shown(cx).contains("hello\nworld"), "{}", shown(cx));
        cx.simulate_click(
            point(track.left() + px(1.), track.center().y),
            Modifiers::none(),
        );
        assert!(!shown(cx).contains("hello"));
        // Space plays and pauses.
        cx.update(|window, cx| replay.update(cx, |replay, cx| replay.focus(window, cx)));
        let playing = |cx: &mut VisualTestContext| {
            replay.read_with(cx, |replay, _| {
                replay.replay_at().map(|(_, playing)| playing)
            })
        };
        let before = playing(cx);
        cx.simulate_keystrokes("space");
        assert_ne!(playing(cx), before);

        // One that can't be read says why in its tab.
        std::fs::write(dir.join("notes.cast"), "not a recording").unwrap();
        cx.update(|window, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.play_recording(&dir.join("notes.cast"), window, cx)
            })
        });
        workspace.read_with(cx, |workspace, cx| {
            let pane = workspace.tabs[2].focused().read(cx);
            assert!(matches!(&pane.phase, crate::terminal::Phase::Ended(why) if why == "It isn't a recording"));
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn opens_a_shell_inside_a_container_on_the_host(cx: &mut TestAppContext) {
        use tern_core::containers::Container;

        let (workspace, cx) = open_workspace(cx);
        let pane = workspace.read_with(cx, |workspace, _| workspace.tabs[0].focused().clone());
        // A host without Docker or Podman has no Containers.
        cx.run_until_parked();
        assert!(cx.debug_bounds("containers").is_none());
        let web = Container {
            tool: "docker".into(),
            id: "3f2a9c1b".into(),
            name: "web".into(),
            image: "nginx:1.27".into(),
        };
        pane.update(cx, |pane, cx| {
            pane.set_containers(Some(vec![web.clone()]));
            cx.notify();
        });
        click(cx, "containers");
        click(cx, "container-0");
        workspace.read_with(cx, |workspace, cx| {
            let tab = &workspace.tabs[0];
            assert_eq!((tab.panes.len(), tab.focused), (2, 1));
            let inside = tab.panes[1].read(cx);
            assert_eq!(inside.connection().container.as_ref(), Some(&web));
            assert_eq!(inside.label(), "web");
            assert!(workspace.containers.is_none());
        });
    }

    #[gpui::test]
    fn draws_a_hosts_vitals_once_read(cx: &mut TestAppContext) {
        let (workspace, cx) = open_workspace(cx);
        let pane = workspace.read_with(cx, |workspace, _| workspace.tabs[0].focused().clone());
        let shown = |cx: &mut VisualTestContext| {
            cx.run_until_parked();
            ["vital-load", "vital-memory", "vital-disk"].map(|id| cx.debug_bounds(id).is_some())
        };
        assert_eq!(shown(cx), [false; 3]);
        // A host that doesn't say how full its disk is leaves that out.
        pane.update(cx, |pane, cx| {
            pane.probed(4, Vec::new(), Some(1.5), Some(0.25), None, cx)
        });
        assert_eq!(shown(cx), [true, true, false]);
        pane.update(cx, |pane, cx| {
            pane.probed(4, Vec::new(), Some(0.5), Some(0.3), Some(0.8), cx)
        });
        assert_eq!(shown(cx), [true; 3]);
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.vitals().load, [Some(1.5), Some(0.5)]);
            assert_eq!(pane.vitals().cpus, 4);
        });
    }

    #[gpui::test]
    fn forwards_the_ports_a_host_opens_in_a_click(cx: &mut TestAppContext) {
        use tern_core::forward::ForwardState;
        use tern_core::probe::Listening;
        use tern_core::ssh::Event;

        let (workspace, cx) = open_workspace(cx);
        let pane = workspace.read_with(cx, |workspace, _| workspace.tabs[0].focused().clone());
        let at = |port: u16| Listening {
            port,
            host: "localhost".into(),
        };
        let probed = |cx: &mut VisualTestContext, ports: Vec<Listening>| {
            pane.update(cx, |pane, cx| {
                pane.probed(2, ports, Some(0.5), Some(0.4), Some(0.7), cx)
            });
            cx.run_until_parked();
        };
        let asked = |cx: &mut VisualTestContext| {
            pane.update(cx, |pane, _| std::mem::take(&mut pane.forwards_asked))
        };

        // What listened at first isn't offered; what opens later is.
        probed(cx, vec![at(22)]);
        probed(cx, vec![at(22), at(3000)]);
        assert!(cx.debug_bounds("port-22").is_none());
        click(cx, "port-3000");
        let forward = match asked(cx).as_slice() {
            [(forward, true)] => forward.clone(),
            other => panic!("asked {other:?}"),
        };
        assert_eq!(
            (
                forward.listen_port,
                forward.to_host.as_str(),
                forward.to_port
            ),
            (3000, "localhost", 3000)
        );

        // Forwarded: the chip opens the page, its stop ends it.
        pane.update(cx, |pane, cx| {
            let state = ForwardState::Running;
            pane.on_session_event(Event::Forwarding { forward, state }, cx)
        });
        click(cx, "port-3000");
        assert_eq!(cx.opened_url().as_deref(), Some("http://localhost:3000/"));
        click(cx, "port-stop-3000");
        assert!(matches!(asked(cx).as_slice(), [(_, false)]));

        // A port that closes goes, and its forward with it.
        click(cx, "port-3000");
        assert!(matches!(asked(cx).as_slice(), [(_, true)]));
        probed(cx, vec![at(22)]);
        assert!(cx.debug_bounds("port-3000").is_none());
        assert!(matches!(asked(cx).as_slice(), [(_, false)]));
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
        workspace.read_with(cx, |workspace, cx| {
            let tab = &workspace.tabs[1];
            assert_eq!(tab.focused, 1);
            assert!(tab.files_open);
            assert!(tab.files.as_ref().unwrap().read(cx).is_for(&tab.panes[1]));
        });
    }

    #[gpui::test]
    fn files_opens_again_on_the_tabs_current_connection(cx: &mut TestAppContext) {
        let (workspace, cx) = open_workspace(cx);
        let tab = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, _| {
                let tab = &workspace.tabs[0];
                (tab.panes.clone(), tab.files.clone().unwrap())
            })
        };
        let on = |cx: &mut VisualTestContext| {
            let (panes, files) = tab(cx);
            files.read_with(cx, |files, cx| {
                let pane = panes.iter().position(|pane| files.is_for(pane)).unwrap();
                (pane, files.session(), panes[pane].read(cx).session())
            })
        };
        click(cx, "toggle-files");
        let (panes, files) = tab(cx);
        assert_eq!(on(cx), (0, 0, 0));

        // The pane reconnected while Files was hidden: shown again, the
        // same panel opens SFTP on the new connection.
        click(cx, "toggle-files");
        panes[0].update(cx, |pane, cx| pane.reconnect_offline(cx));
        click(cx, "toggle-files");
        assert_eq!(tab(cx).1, files);
        assert_eq!(on(cx), (0, 1, 1));

        // While it's shown, Refresh does.
        panes[0].update(cx, |pane, cx| pane.reconnect_offline(cx));
        click(cx, "files-refresh");
        assert_eq!(on(cx), (0, 2, 2));

        // Files on a second pane; closing that pane ends its session, and
        // Files opens on the pane left.
        click(cx, "toggle-files");
        click(cx, "layout-2");
        click(cx, "toggle-files");
        assert_eq!(on(cx).0, 1);
        let closing = tab(cx).0[1].downgrade();
        click(cx, "pane-close-1");
        assert!(closing.upgrade().is_none());
        assert_eq!(on(cx).0, 0);
        assert!(workspace.read_with(cx, |workspace, _| workspace.tabs[0].files_open));
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
    fn a_long_settings_page_scrolls_inside_a_short_window(cx: &mut TestAppContext) {
        let (_workspace, cx) = open_workspace(cx);
        let height = gpui::px(600.);
        cx.simulate_resize(gpui::size(gpui::px(900.), height));
        click(cx, "open-settings");
        click(cx, "settings-terminal");
        let bounds = |cx: &mut VisualTestContext, selector: &'static str| {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("nothing on screen is tagged {selector}"))
        };
        let (dialog, page) = (bounds(cx, "settings"), bounds(cx, "settings-page"));
        assert!(dialog.bottom() <= height, "{dialog:?}");
        assert!(
            page.bottom() <= dialog.bottom(),
            "{page:?} runs past {dialog:?}"
        );
        // Taller than the room it has, so it scrolls.
        assert!(bounds(cx, "settings-card").size.height > page.size.height);
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
