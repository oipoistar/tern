//! A terminal pane: an SSH session from tern-core, emulated by
//! alacritty_terminal and drawn cell by cell with GPUI.
//!
//! Before the shell is live the pane shows what the connection needs: trust
//! for an unknown host key, a password, or a way to reconnect. A dropped
//! connection reconnects by itself, keeping what was on screen.
//!
//! Dragging selects text (double-click a word, triple-click a line,
//! Shift+click extends), painted inverted; Ctrl+Shift+C (Cmd+C on macOS)
//! copies it. Ctrl+Shift+F (Cmd+F) finds text in the scrollback,
//! Ctrl+click (Cmd+click) opens a link, and Ctrl+= / Ctrl+- / Ctrl+0 zoom
//! the font. Programs that ask for the mouse (vim, htop) get it, except
//! while Shift is held, which keeps it for selecting.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::{Range, RangeInclusive};
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode, viewport_to_point};
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape, CursorStyle as TermCursorStyle, NamedColor, Processor, Rgb,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::forward::{Forward, ForwardKind, ForwardState};
use tern_core::history::{self, Visit};
use tern_core::probe::{self, Listening, Vitals};
use tern_core::reconnect;
use tern_core::settings::{Bell, CursorStyle, TerminalSettings};
use tern_core::sftp::Sftp;
use tern_core::shells;
use tern_core::ssh::{
    ConnectError, Connection, Event, Prompts, Session, SessionInput, TermSize, Trusted,
};

use crate::settings;
use crate::term_input::{MouseReport, encode_key, encode_mouse, hold_paste, wheel_keys};
use crate::theme::{self, MONO, TermPalette, Theme};
use crate::ui::icon;

/// Line height as a multiple of the font size.
const LINE_SPACING: f32 = 1.35;
/// How long the visual bell lights the pane.
const BELL_FLASH: std::time::Duration = std::time::Duration::from_millis(120);
/// Lines a session's preview keeps: those ending at the cursor.
const PREVIEW_LINES: i32 = 9;
/// What a reconnected shell starts on: the modes the last program may have
/// left on switched off (mouse reporting, application keys, bracketed
/// paste, a hidden cursor, colours, a scroll region), and a clear screen,
/// whose old lines move into the scrollback.
const FRESH_MODES: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\
\x1b[?1l\x1b>\x1b[?2004l\x1b[?25h\x1b[0m\x1b[r\x1b[2J\x1b[H";

/// How often a live session reads the host's `/proc`.
const PROBE_EVERY: Duration = Duration::from_secs(5);

/// The last lines of a session, shared with the vault's side panel.
pub type Preview = Rc<RefCell<Vec<String>>>;

/// Where the connection stands.
pub enum Phase {
    Connecting,
    /// A server's key is unknown (the target's, or the jump host's);
    /// waiting for the user to trust it.
    Trust {
        /// The host as the card names it.
        shown: SharedString,
        /// The host and its key, to trust for it alone.
        host: Trusted,
        fingerprint: SharedString,
    },
    /// Key-based login failed; collecting a password.
    Password {
        typed: String,
        rejected: bool,
    },
    /// The server asks questions while signing in, like a one-time code;
    /// collecting the answers one at a time.
    Questions {
        prompts: Prompts,
        answers: Vec<String>,
        typed: String,
    },
    Live,
    Ended(SharedString),
}

/// Input waiting for a yes before it reaches the shell.
enum Held {
    /// A paste whose lines the shell would run as they arrive, or any paste
    /// into a guarded host.
    Paste(String),
    /// A snippet, or a command run from elsewhere, for a guarded host.
    Run(Vec<u8>),
}

/// The rule forwarding a port the host opened to the same port here.
fn port_forward(open: &Listening) -> Option<Forward> {
    let to = format!("{}:{}", open.host, open.port);
    Forward::typed(ForwardKind::Local, &open.port.to_string(), &to).ok()
}

/// The slots of kept shells that open panes hold, by host.
#[derive(Default)]
struct KeptShells(HashMap<String, BTreeSet<u32>>);

impl Global for KeptShells {}

pub enum TerminalEvent {
    /// A connection attempt ended (closed, failed or given up), for
    /// History.
    Finished(Visit),
    /// A port forwarding rule of the saved host `host` started, stopped or
    /// failed.
    Forwarding {
        host: String,
        forward: Forward,
        state: ForwardState,
    },
    /// The user typed this into the shell, for broadcasting to other panes.
    Typed(Vec<u8>),
    /// The user pasted this text (newlines as returns), for broadcasting:
    /// each pane brackets it as its own shell asked.
    Pasted(String),
}

impl EventEmitter<TerminalEvent> for TerminalView {}

pub struct TerminalView {
    connection: Connection,
    /// The connection's one-off command, taken out of it so that neither a
    /// reconnect nor a pane split from this one types it again.
    once: Option<String>,
    /// What's typed here goes to the tab's other panes too.
    pub broadcasting: bool,
    password: Option<String>,
    trust: Option<Trusted>,
    pub phase: Phase,
    input: Option<SessionInput>,
    /// Sessions started here, reconnects included: the latest's number.
    sessions: u64,
    /// The name of the shell kept running on the host for this pane, when
    /// the host keeps them.
    shell: Option<String>,
    /// The ports listening on the host when the pane's first session began,
    /// the ones opened since, and those forwarded here from the toolbar,
    /// each with how it's doing (`None` while starting).
    ports_before: Option<Vec<Listening>>,
    ports: Vec<Listening>,
    forwarded: BTreeMap<u16, (Forward, Option<ForwardState>)>,
    /// The host's recent load, memory and disk use.
    vitals: Vitals,
    _probing: Option<Task<()>>,
    /// Port forwarding asked of the session, for tests.
    #[cfg(test)]
    pub forwards_asked: Vec<(Forward, bool)>,
    _events: Option<Task<()>>,
    term: Term<Listener>,
    parser: Processor,
    term_events: Rc<RefCell<Vec<TermEvent>>>,
    size: TermSize,
    title: Option<SharedString>,
    focus: FocusHandle,
    /// Where the grid was last painted, for finding the cell under the
    /// pointer.
    grid: Rc<Cell<GridGeometry>>,
    /// A drag is extending the selection.
    selecting: bool,
    /// When the current attempt started, in seconds since the Unix epoch.
    started: u64,
    /// Since when the shell has been up.
    live_since: Option<Instant>,
    /// A shell has run in this pane, so a new one continues below it.
    had_shell: bool,
    /// Reconnecting after the connection dropped: the attempt being made
    /// or waited for, counting from 1.
    reconnect: Option<u32>,
    _retry: Option<Task<()>>,
    preview: Preview,
    /// The scrollback and cursor the terminal was last configured with.
    configured: (usize, CursorStyle),
    /// The visual bell lights the pane until then.
    flash_until: Option<Instant>,
    /// The find bar, while it's open.
    find: Option<Find>,
    /// The link under the pointer while Ctrl (Cmd on macOS) is held.
    link: Option<Link>,
    /// A button the program was told went down, until it's told it came up.
    pressed: Option<MouseButton>,
    /// The cell the program last heard the pointer was in.
    reported_cell: Option<(usize, usize)>,
    /// Input waiting for the user to say so.
    held: Option<Held>,
    /// A guarded pane takes Broadcast too: the user let it in.
    pub let_in: bool,
    /// The right-click menu, open where it was asked for.
    menu: Option<Point<Pixels>>,
    /// Output arrived since the pane was last on screen.
    pub unseen: bool,
    /// The bell rang since then.
    pub rang: bool,
    /// Everything sent to the session, for tests to look at.
    #[cfg(test)]
    pub sent: RefCell<Vec<u8>>,
    /// The last answers given to the server's questions, likewise.
    #[cfg(test)]
    pub answered: Vec<String>,
}

/// Find in scrollback: the field, and the search for what's typed in it.
/// The match it's on is the terminal's selection, which moves with the
/// text as output scrolls it.
struct Find {
    input: Entity<InputState>,
    /// What was last searched for.
    query: String,
    /// `None` while the field is empty.
    regex: Option<RegexSearch>,
    /// Which match is selected, counting from the oldest, and how many
    /// there are, as of the last search or step.
    count: (usize, usize),
    _subscription: Subscription,
}

/// A link on screen, from its first cell to its last.
#[derive(Clone, Debug, PartialEq)]
struct Link {
    cells: RangeInclusive<GridPoint>,
    url: String,
}

/// Where the grid sits in the window, as of the last paint.
#[derive(Clone, Copy, Default)]
struct GridGeometry {
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
}

/// Collects events alacritty_terminal raises while parsing output.
#[derive(Clone)]
struct Listener(Rc<RefCell<Vec<TermEvent>>>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        self.0.borrow_mut().push(event);
    }
}

struct GridSize(TermSize);

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.0.rows.into()
    }
    fn screen_lines(&self) -> usize {
        self.0.rows.into()
    }
    fn columns(&self) -> usize {
        self.0.cols.into()
    }
}

impl TerminalView {
    pub fn new(connection: Connection, cx: &mut Context<Self>) -> Self {
        let mut view = Self::idle(connection, cx);
        view.password = view.saved_password(cx);
        view.shell = view.claim_shell(cx);
        view.connect(cx);
        view
    }

    /// The name of this pane's kept shell, when its host keeps them: the
    /// host's lowest slot no open pane holds, given back when the pane goes.
    /// The same slot after a restart finds the same shell.
    fn claim_shell(&self, cx: &mut Context<Self>) -> Option<String> {
        if !self.connection.keep_shell || self.connection.local {
            return None;
        }
        let key = self.connection.host_id.clone().unwrap_or_else(|| {
            let target = &self.connection.target;
            format!("{}@{}:{}", target.user, target.host, target.port)
        });
        let held = cx
            .default_global::<KeptShells>()
            .0
            .entry(key.clone())
            .or_default();
        let slot = (1..)
            .find(|slot| !held.contains(slot))
            .expect("a free slot");
        held.insert(slot);
        let name = shells::name(&key, slot);
        cx.on_release(move |_, cx| {
            if let Some(held) = cx.default_global::<KeptShells>().0.get_mut(&key) {
                held.remove(&slot);
            }
        })
        .detach();
        Some(name)
    }

    /// Reads the host's `/proc` over SFTP every few seconds while the
    /// session is live: the ports it listens on and its vitals. A host
    /// without SFTP or `/proc` (not Linux) shows neither.
    fn probe(&mut self, cx: &mut Context<Self>) {
        let Some(input) = &self.input else {
            return;
        };
        let opening = input.open_sftp();
        self._probing = Some(cx.spawn(async move |this, cx| {
            let Ok(sftp) = opening.await else {
                return;
            };
            let Ok(cpuinfo) = sftp.read_text("/proc/cpuinfo").await else {
                return;
            };
            let cpus = probe::cpus(&cpuinfo).max(1);
            loop {
                let mut tables = sftp.read_text("/proc/net/tcp").await.unwrap_or_default();
                // tcp6's header line isn't a listening port either.
                tables += &sftp.read_text("/proc/net/tcp6").await.unwrap_or_default();
                let read = |text: Result<String, String>, parse: fn(&str) -> Option<f32>| {
                    text.ok().and_then(|text| parse(&text))
                };
                let load = read(sftp.read_text("/proc/loadavg").await, probe::load);
                let memory = read(sftp.read_text("/proc/meminfo").await, probe::memory);
                let disk = sftp.disk_use("/").await.ok().flatten();
                let listening = probe::listening(&tables);
                if this
                    .update(cx, |this, cx| {
                        this.probed(cpus, listening, load, memory, disk, cx)
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(PROBE_EVERY).await;
            }
        }));
    }

    /// Takes in one reading of the host's `/proc`.
    pub(crate) fn probed(
        &mut self,
        cpus: usize,
        listening: Vec<Listening>,
        load: Option<f32>,
        memory: Option<f32>,
        disk: Option<f32>,
        cx: &mut Context<Self>,
    ) {
        self.vitals.cpus = cpus;
        self.vitals.push(load, memory, disk);
        let before = self.ports_before.get_or_insert_with(|| listening.clone());
        self.ports = probe::opened(before, &listening);
        // A port that closed takes its forward with it.
        let closed: Vec<u16> = self
            .forwarded
            .keys()
            .copied()
            .filter(|port| !self.ports.iter().any(|open| open.port == *port))
            .collect();
        for port in closed {
            if let Some((forward, _)) = self.forwarded.remove(&port) {
                self.ask_forward(forward, false);
            }
        }
        cx.notify();
    }

    /// Stops reading the host's `/proc`; the session ended, and its
    /// forwards with it.
    fn end_probe(&mut self) {
        self._probing = None;
        self.forwarded.clear();
    }

    /// The ports the host opened since the pane's first session, each with
    /// how it's forwarded here: not at all (`None`), starting
    /// (`Some(None)`), or its state.
    pub fn opened_ports(&self) -> Vec<(u16, Option<Option<ForwardState>>)> {
        self.ports
            .iter()
            .map(|open| {
                let forwarded = self.forwarded.get(&open.port);
                (open.port, forwarded.map(|(_, state)| state.clone()))
            })
            .collect()
    }

    /// Forwards a port the host opened to the same port here, or stops
    /// forwarding it; one that failed is tried again.
    pub fn toggle_port(&mut self, port: u16, cx: &mut Context<Self>) {
        let Some(forward) = self
            .ports
            .iter()
            .find(|open| open.port == port)
            .and_then(port_forward)
        else {
            return;
        };
        match self.forwarded.remove(&port) {
            Some((asked, None | Some(ForwardState::Running))) => self.ask_forward(asked, false),
            _ => {
                self.ask_forward(forward.clone(), true);
                self.forwarded.insert(port, (forward, None));
            }
        }
        cx.notify();
    }

    /// Opens a port forwarded here in the browser.
    pub fn open_port(port: u16, cx: &mut App) {
        cx.open_url(&format!("http://localhost:{port}/"));
    }

    fn ask_forward(&mut self, forward: Forward, on: bool) {
        #[cfg(test)]
        self.forwards_asked.push((forward.clone(), on));
        if let Some(input) = &self.input {
            input.forward(forward, on);
        }
    }

    /// Ends the shell kept on the host for good, as the pane is closed by
    /// hand.
    pub fn end_kept_shell(&mut self) {
        if let (Some(name), Some(input)) = (&self.shell, &self.input) {
            input.close_after(shells::end(name));
        }
    }

    /// The password saved for this connection's host, if it signs in with
    /// one; it's tried before asking.
    fn saved_password(&self, cx: &App) -> Option<String> {
        let secret = self.connection.secret.as_deref()?;
        (self.connection.auth == tern_core::Auth::Password)
            .then(|| crate::sync::secrets(cx).load(secret))
            .flatten()
    }

    /// A live pane with no connection behind it, for tests.
    #[cfg(test)]
    pub fn offline(cx: &mut Context<Self>) -> Self {
        let target = tern_core::ssh::Target {
            host: "example".into(),
            port: 22,
            user: "me".into(),
        };
        Self::offline_to(Connection::typed(target, None), cx)
    }

    #[cfg(test)]
    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    /// A pane for `connection` that only reads its saved password, for
    /// tests.
    #[cfg(test)]
    pub fn unconnected(connection: Connection, cx: &mut Context<Self>) -> Self {
        let mut view = Self::idle(connection, cx);
        view.password = view.saved_password(cx);
        view
    }

    /// A live pane for `connection` with nothing behind it, for tests.
    #[cfg(test)]
    pub fn offline_to(connection: Connection, cx: &mut Context<Self>) -> Self {
        let mut view = Self::idle(connection, cx);
        view.on_session_event(Event::Connected, cx);
        view
    }

    /// Stands in for a reconnect, for tests: the next session, live, with
    /// nothing behind it either.
    #[cfg(test)]
    pub fn reconnect_offline(&mut self, cx: &mut Context<Self>) {
        self.sessions += 1;
        self.on_session_event(Event::Connected, cx);
    }

    /// The session `open_sftp` opens on, by number: a reconnect starts the
    /// next, so SFTP opened on an earlier one is known to be gone.
    pub fn session(&self) -> u64 {
        self.sessions
    }

    /// An SFTP session on this pane's connection; asked while connecting,
    /// it opens once the shell does.
    pub fn open_sftp(&self) -> Pin<Box<dyn Future<Output = Result<Sftp, String>>>> {
        match &self.input {
            Some(input) => Box::pin(input.open_sftp()),
            None => Box::pin(async { Err("Not connected".to_string()) }),
        }
    }

    /// The session's last lines, kept current as output arrives.
    pub fn preview(&self) -> Preview {
        self.preview.clone()
    }

    /// Copies the lines up to the cursor into the preview: a few rows of
    /// the grid, cheap enough for every chunk of output.
    fn update_preview(&self) {
        let grid = self.term.grid();
        let bottom = grid.cursor.point.line.0;
        let top = (bottom - PREVIEW_LINES + 1).max(-(grid.history_size() as i32));
        let columns = grid.columns();
        let lines = (top..=bottom)
            .map(|line| {
                let row = &grid[alacritty_terminal::index::Line(line)];
                let text: String = (0..columns).map(|column| row[Column(column)].c).collect();
                text.trim_end().to_string()
            })
            .collect();
        *self.preview.borrow_mut() = lines;
    }

    /// Whether input that doesn't come from this pane's keyboard waits for
    /// a yes, or doesn't reach it.
    pub fn guarded(&self) -> bool {
        self.connection.guarded
    }

    /// Whether Broadcast reaches this pane: a guarded host's only once let
    /// in.
    pub fn takes_broadcast(&self) -> bool {
        !self.guarded() || self.let_in
    }

    /// Types a snippet, or a command run from elsewhere; a guarded host's
    /// pane holds it until the user says so.
    pub fn type_command(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if self.guarded() {
            self.held = Some(Held::Run(bytes));
            cx.notify();
        } else {
            self.type_text(bytes, cx);
        }
    }

    /// Types `bytes` into the shell, as if from the keyboard. Ignored
    /// until the session is live.
    pub fn type_text(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if let Phase::Live = self.phase {
            self.term.scroll_display(Scroll::Bottom);
            self.term.selection = None;
            self.send(bytes);
            cx.notify();
        }
    }

    /// A pane for `connection` that hasn't started connecting.
    fn idle(mut connection: Connection, cx: &mut Context<Self>) -> Self {
        let size = TermSize {
            cols: 100,
            rows: 30,
        };
        let term_events = Rc::new(RefCell::new(Vec::new()));
        let terminal = settings::current(cx).terminal;
        let config = term_config(&terminal);
        Self {
            once: connection.once.take(),
            broadcasting: false,
            connection,
            password: None,
            trust: None,
            phase: Phase::Connecting,
            input: None,
            sessions: 0,
            shell: None,
            ports_before: None,
            ports: Vec::new(),
            forwarded: BTreeMap::new(),
            vitals: Vitals::default(),
            _probing: None,
            #[cfg(test)]
            forwards_asked: Vec::new(),
            _events: None,
            term: Term::new(config, &GridSize(size), Listener(term_events.clone())),
            parser: Processor::new(),
            term_events,
            size,
            title: None,
            focus: cx.focus_handle(),
            grid: Rc::default(),
            selecting: false,
            started: history::now(),
            live_since: None,
            had_shell: false,
            reconnect: None,
            _retry: None,
            preview: Preview::default(),
            configured: (terminal.scrollback, terminal.cursor),
            flash_until: None,
            find: None,
            link: None,
            pressed: None,
            reported_cell: None,
            held: None,
            let_in: false,
            menu: None,
            unseen: false,
            rang: false,
            #[cfg(test)]
            sent: RefCell::default(),
            #[cfg(test)]
            answered: Vec::new(),
        }
    }

    /// The pane is on screen: what happened while it wasn't has been seen.
    pub fn seen(&mut self) {
        self.unseen = false;
        self.rang = false;
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus.focus(window, cx);
    }

    /// Tab label: the remote title if the shell set one, else `user@host`.
    pub fn title(&self) -> SharedString {
        self.title.clone().unwrap_or_else(|| {
            if self.connection.local {
                return shell_name().into();
            }
            let target = &self.connection.target;
            format!("{}@{}", target.user, target.host).into()
        })
    }

    /// Whether the shell is up.
    pub fn is_live(&self) -> bool {
        matches!(self.phase, Phase::Live)
    }

    /// The saved host this session is, if it is one.
    pub fn host(&self) -> Option<&str> {
        self.connection.host.as_deref()
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// The saved host's name, or `user@host` for a typed destination.
    pub fn label(&self) -> SharedString {
        match &self.connection.host {
            Some(host) => host.clone().into(),
            None if self.connection.local => "This computer".into(),
            None => {
                let target = &self.connection.target;
                format!("{}@{}", target.user, target.host).into()
            }
        }
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    /// Sends input the user gave this pane, and reports it for broadcast.
    fn send_typed(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        self.term.scroll_display(Scroll::Bottom);
        self.term.selection = None;
        self.send(bytes.clone());
        cx.emit(TerminalEvent::Typed(bytes));
    }

    /// Starts or stops a port forwarding rule in the open session.
    pub fn set_forward(&self, forward: Forward, on: bool) {
        if let Some(input) = &self.input {
            input.forward(forward, on);
        }
    }

    /// The current attempt as a History entry that `ended` as given.
    pub fn visit(&self, ended: &str) -> Visit {
        let target = &self.connection.target;
        Visit {
            host: self.connection.host.clone(),
            user: target.user.clone(),
            address: target.host.clone(),
            port: target.port,
            auth: self.connection.auth.clone(),
            started: self.started,
            lasted: self.live_since.map(|since| since.elapsed().as_secs()),
            ended: ended.into(),
        }
    }

    /// Ends the attempt with `message` on screen, and reports it.
    fn end(&mut self, message: SharedString, cx: &mut Context<Self>) {
        // History is for connections; a local shell isn't one.
        if !self.connection.local {
            cx.emit(TerminalEvent::Finished(self.visit(&message)));
        }
        self.phase = Phase::Ended(message);
        self.live_since = None;
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        // Answering a trust or password prompt continues the same attempt.
        if !matches!(self.phase, Phase::Trust { .. } | Phase::Password { .. }) {
            self.started = history::now();
        }
        let session = if self.connection.local {
            Session::local(self.size)
        } else {
            // Jump hosts sign in with their own saved passwords.
            let store = crate::sync::secrets(cx);
            let mut request = self.connection.request(
                self.size,
                self.password.as_deref(),
                self.trust.clone(),
                |jump| jump.secret.as_deref().and_then(|secret| store.load(secret)),
            );
            request.command = self.shell.as_deref().map(shells::attach);
            Session::open(request)
        };
        let (input, mut events) = session.into_parts();
        self.input = Some(input);
        self.sessions += 1;
        self.phase = Phase::Connecting;
        self._events = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                if this
                    .update(cx, |view, cx| view.on_session_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
        cx.notify();
    }

    pub(crate) fn on_session_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Connected => {
                let reattached = self.had_shell && self.shell.is_some();
                self.phase = Phase::Live;
                if !self.connection.local {
                    self.probe(cx);
                }
                self.live_since = Some(Instant::now());
                self.reconnect = None;
                if self.had_shell {
                    // Reconnected: the scrollback stays.
                    if self.term.mode().contains(TermMode::ALT_SCREEN) {
                        self.parser.advance(&mut self.term, b"\x1b[?1049l");
                    }
                    self.parser.advance(&mut self.term, FRESH_MODES);
                    self.title = None;
                } else {
                    self.term = Term::new(
                        term_config(&settings::current(cx).terminal),
                        &GridSize(self.size),
                        Listener(self.term_events.clone()),
                    );
                }
                self.had_shell = true;
                // Queued now, it reaches the shell as soon as it starts; a
                // kept shell found again needs nothing typed into it.
                if let Some(command) = &self.connection.on_login
                    && !reattached
                {
                    self.send(format!("{command}\r").into_bytes());
                }
                if let Some(command) = self.once.take() {
                    self.type_command(format!("{command}\r").into_bytes(), cx);
                }
            }
            Event::Output(bytes) => {
                self.unseen = true;
                self.parser.advance(&mut self.term, &bytes);
                self.answer_terminal_events(cx);
                self.update_preview();
            }
            Event::Prompts(prompts) => {
                self.phase = Phase::Questions {
                    prompts,
                    answers: Vec::new(),
                    typed: String::new(),
                }
            }
            Event::Forwarding { forward, state } => {
                let port = forward.listen_port;
                let ours = self
                    .forwarded
                    .get(&port)
                    .is_some_and(|(asked, _)| asked.id() == forward.id())
                    || self
                        .ports
                        .iter()
                        .filter_map(port_forward)
                        .any(|ours| ours.id() == forward.id());
                if ours {
                    // One of the ports the host opened, forwarded from the
                    // toolbar.
                    if let Some(entry) = self.forwarded.get_mut(&port)
                        && state != ForwardState::Stopped
                    {
                        entry.1 = Some(state);
                    }
                } else if let Some(host) = self.connection.host.clone() {
                    cx.emit(TerminalEvent::Forwarding {
                        host,
                        forward,
                        state,
                    });
                }
            }
            Event::Closed { exit_status } => {
                self.input = None;
                self.end_probe();
                let message = match exit_status {
                    Some(status) => format!("Exited with status {status}").into(),
                    None => "Connection closed".into(),
                };
                self.end(message, cx);
            }
            Event::Lost => {
                self.input = None;
                self.end_probe();
                cx.emit(TerminalEvent::Finished(self.visit("Connection lost")));
                self.live_since = None;
                self.retry(1, cx);
            }
            // While reconnecting, what may pass by itself is tried again
            // later; the attempts in between aren't History's business.
            Event::Failed(error)
                if self.reconnect.is_some_and(|attempt| {
                    reconnect::worth_retrying(&error) && reconnect::delay(attempt + 1).is_some()
                }) =>
            {
                self.input = None;
                let next = self.reconnect.map_or(1, |attempt| attempt + 1);
                self.retry(next, cx);
            }
            Event::Failed(error) => {
                self.input = None;
                self.reconnect = None;
                match error {
                    ConnectError::UnknownHostKey { host, port, key, fingerprint } => self.phase = Phase::Trust {
                        shown: if port == 22 { host.clone().into() } else { format!("{host}:{port}").into() },
                        host: Trusted { host, port, key: *key },
                        fingerprint: fingerprint.into(),
                    },
                    ConnectError::AuthFailed { .. } => self.phase = Phase::Password { typed: String::new(), rejected: self.password.is_some() },
                    ConnectError::HostKeyChanged { fingerprint, file, line } => self.end(
                        // The one place the extra words earn their keep: this
                        // may be an attack, and the user needs the line to fix it.
                        format!(
                            "Host key changed to {fingerprint}, possible interception. Known key: {}:{line}",
                            file.display()
                        )
                        .into(),
                        cx,
                    ),
                    ConnectError::JumpRefused(message) | ConnectError::Other(message) => {
                        self.end(message.into(), cx)
                    }
                }
            }
        }
        cx.notify();
    }

    /// Waits, then makes reconnect attempt `attempt`.
    fn retry(&mut self, attempt: u32, cx: &mut Context<Self>) {
        let Some(delay) = reconnect::delay(attempt) else {
            return;
        };
        self.reconnect = Some(attempt);
        self.phase = Phase::Connecting;
        self._retry = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |view, cx| view.reconnect_now(cx));
        }));
        cx.notify();
    }

    /// Makes the awaited reconnect attempt without waiting any longer.
    fn reconnect_now(&mut self, cx: &mut Context<Self>) {
        if self.reconnect.is_some() && self.input.is_none() {
            self._retry = None;
            self.connect(cx);
        }
    }

    /// Stops reconnecting, closing an attempt in progress.
    fn stop_reconnecting(&mut self, cx: &mut Context<Self>) {
        if self.reconnect.take().is_some() {
            self._retry = None;
            self.input = None;
            self._events = None;
            // History already has the drop.
            self.phase = Phase::Ended("Connection lost".into());
            cx.notify();
        }
    }

    /// Replies to queries the remote program made of the terminal (cursor
    /// position, colours) and picks up title changes.
    fn answer_terminal_events(&mut self, cx: &mut Context<Self>) {
        let events: Vec<TermEvent> = self.term_events.borrow_mut().drain(..).collect();
        for event in events {
            match event {
                TermEvent::PtyWrite(text) => self.send(text.into_bytes()),
                TermEvent::Title(title) => {
                    // A Windows console's title is its program's path: a
                    // local tab reads better as the program, a remote one
                    // as its host.
                    self.title = match tern_core::local::program(&title) {
                        Some(program) if self.connection.local => Some(program.to_string().into()),
                        Some(_) => None,
                        None => Some(title.into()),
                    }
                }
                TermEvent::ResetTitle => self.title = None,
                TermEvent::ColorRequest(index, reply) => {
                    let palette = TermPalette::current(cx);
                    let color = resolve(
                        AnsiColor::Indexed(index.min(255) as u8),
                        self.term.colors(),
                        &palette,
                    );
                    let rgba = color.to_rgb();
                    let to_u8 = |v: f32| (v * 255.).round() as u8;
                    self.send(
                        reply(Rgb {
                            r: to_u8(rgba.r),
                            g: to_u8(rgba.g),
                            b: to_u8(rgba.b),
                        })
                        .into_bytes(),
                    );
                }
                TermEvent::Bell => {
                    self.rang = true;
                    if settings::current(cx).terminal.bell == Bell::Visual {
                        self.flash(cx)
                    }
                }
                TermEvent::ClipboardStore(_, text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text))
                }
                _ => {}
            }
        }
    }

    fn send(&self, bytes: Vec<u8>) {
        #[cfg(test)]
        self.sent.borrow_mut().extend_from_slice(&bytes);
        if let Some(input) = &self.input {
            input.write(bytes);
        }
    }

    fn resize(&mut self, size: TermSize, cx: &mut Context<Self>) {
        if size == self.size {
            return;
        }
        self.size = size;
        self.term.resize(GridSize(size));
        if let Some(input) = &self.input {
            input.resize(size);
        }
        cx.notify();
    }

    fn trust_and_connect(&mut self, cx: &mut Context<Self>) {
        if let Phase::Trust { host, .. } = &self.phase {
            self.trust = Some(host.clone());
            self.connect(cx);
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        // The window's own shortcuts, like switching tabs, go up to it.
        if crate::workspace::shortcut(keystroke).is_some() {
            return;
        }
        // Keys typed in the find bar pass through here too: only closing
        // it is ours, the rest is the field's.
        if !self.focus.is_focused(window) {
            if keystroke.key == "escape" && self.find.is_some() {
                self.close_find(window, cx);
                cx.stop_propagation();
            }
            return;
        }
        if is_find(keystroke) {
            self.open_find(window, cx);
            cx.stop_propagation();
            return;
        }
        if let Some(steps) = zoom_steps(keystroke) {
            settings::update(cx, |settings| settings.terminal.zoom(steps));
            cx.stop_propagation();
            return;
        }
        if is_copy(keystroke) {
            self.copy(cx);
            cx.stop_propagation();
            return;
        }
        if is_select_all(keystroke) {
            self.select_all();
            cx.stop_propagation();
            cx.notify();
            return;
        }
        // Input waiting for a yes: Enter gives it, Escape doesn't.
        if self.held.is_some() {
            match keystroke.key.as_str() {
                "enter" => self.release(cx),
                "escape" => self.held = None,
                _ => {}
            }
            cx.stop_propagation();
            cx.notify();
            return;
        }
        match &mut self.phase {
            Phase::Live => {
                if is_paste(keystroke) {
                    self.paste(cx);
                } else if let Some(bytes) = encode_key(keystroke, *self.term.mode()) {
                    self.send_typed(bytes, cx);
                } else {
                    return;
                }
            }
            // Typed characters arrive through `replace_text_in_range`; a
            // pasted password or code takes its first line.
            Phase::Password { typed, .. } | Phase::Questions { typed, .. }
                if is_paste(keystroke) =>
            {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    typed.push_str(text.lines().next().unwrap_or_default().trim_end());
                }
            }
            Phase::Password { typed, .. } => match keystroke.key.as_str() {
                "enter" => {
                    self.password = Some(std::mem::take(typed));
                    self.connect(cx);
                }
                "backspace" => {
                    typed.pop();
                }
                "escape" => self.end("Cancelled".into(), cx),
                _ => return,
            },
            Phase::Questions {
                prompts,
                answers,
                typed,
            } => match keystroke.key.as_str() {
                "enter" => {
                    answers.push(std::mem::take(typed));
                    if answers.len() >= prompts.questions.len() {
                        let answers = std::mem::take(answers);
                        #[cfg(test)]
                        {
                            self.answered = answers.clone();
                        }
                        if let Some(input) = &self.input {
                            input.answer(answers);
                        }
                        self.phase = Phase::Connecting;
                    }
                }
                "backspace" => {
                    typed.pop();
                }
                "escape" => {
                    // Closing the session ends the sign-in.
                    self.input = None;
                    self._events = None;
                    self.end("Cancelled".into(), cx);
                }
                _ => return,
            },
            Phase::Trust { .. } => match keystroke.key.as_str() {
                "enter" => self.trust_and_connect(cx),
                "escape" => self.end("Not trusted".into(), cx),
                _ => return,
            },
            Phase::Ended(_) if keystroke.key == "enter" => self.connect(cx),
            Phase::Connecting if self.reconnect.is_some() => match keystroke.key.as_str() {
                "enter" => self.reconnect_now(cx),
                "escape" => self.stop_reconnecting(cx),
                _ => return,
            },
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// Text from the keyboard or an input method.
    fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        match &mut self.phase {
            Phase::Live => self.send_typed(text.as_bytes().to_vec(), cx),
            Phase::Password { typed, .. } | Phase::Questions { typed, .. } => typed.push_str(text),
            _ => return,
        }
        cx.notify();
    }

    /// Pastes the clipboard, asking first when it has lines the shell
    /// would run as they arrive.
    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        // Broadcast, it also reaches shells that may not take it bracketed.
        let bracketed = self.term.mode().contains(TermMode::BRACKETED_PASTE) && !self.broadcasting;
        let confirm = settings::current(cx).terminal.confirm_paste;
        if hold_paste(&text, bracketed, self.guarded(), confirm) {
            self.held = Some(Held::Paste(text));
            cx.notify();
            return;
        }
        self.paste_text(text, cx);
    }

    /// Gives the shell what was held for a yes.
    fn release(&mut self, cx: &mut Context<Self>) {
        match self.held.take() {
            Some(Held::Paste(text)) => self.paste_text(text, cx),
            Some(Held::Run(bytes)) => self.type_text(bytes, cx),
            None => {}
        }
    }

    fn paste_text(&mut self, text: String, cx: &mut Context<Self>) {
        // Terminals expect CR for newlines.
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        self.term.scroll_display(Scroll::Bottom);
        self.term.selection = None;
        self.send(self.pasted(&text));
        cx.emit(TerminalEvent::Pasted(text));
    }

    /// A paste as this pane's shell takes it: bracketed when it asked, so
    /// it can tell pasted text from typed.
    fn pasted(&self, text: &str) -> Vec<u8> {
        if self.term.mode().contains(TermMode::BRACKETED_PASTE) {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.as_bytes().to_vec()
        }
    }

    /// Pastes what another pane was given, while broadcasting.
    pub fn type_paste(&mut self, text: &str, cx: &mut Context<Self>) {
        let bytes = self.pasted(text);
        self.type_text(bytes, cx);
    }

    /// The screen cell under `position` in the window as (line, column)
    /// from the top left, and which half of it.
    fn screen_cell(&self, position: Point<Pixels>) -> (usize, usize, Side) {
        let grid = self.grid.get();
        let x = ((position.x - grid.origin.x) / grid.cell_width.max(px(1.))).max(0.);
        let y = ((position.y - grid.origin.y) / grid.line_height.max(px(1.))).max(0.);
        let last_column = usize::from(self.size.cols).saturating_sub(1);
        let column = (x as usize).min(last_column);
        // Past the last column counts as its right half.
        let side = if x as usize > last_column || x.fract() >= 0.5 {
            Side::Right
        } else {
            Side::Left
        };
        let line = (y as usize).min(usize::from(self.size.rows).saturating_sub(1));
        (line, column, side)
    }

    /// The cell under `position` in the window, and which half of it.
    fn cell_at(&self, position: Point<Pixels>) -> (GridPoint, Side) {
        let (line, column, side) = self.screen_cell(position);
        let offset = self.term.grid().display_offset();
        (
            viewport_to_point(offset, GridPoint::new(line, Column(column))),
            side,
        )
    }

    /// Whether `position` is over the grid itself.
    fn over_grid(&self, position: Point<Pixels>) -> bool {
        let grid = self.grid.get();
        let size = size(
            grid.cell_width * f32::from(self.size.cols),
            grid.line_height * f32::from(self.size.rows),
        );
        Bounds::new(grid.origin, size).contains(&position)
    }

    /// The program asked for the mouse, and Shift isn't keeping it for
    /// selecting.
    fn reports_mouse(&self, modifiers: &Modifiers) -> bool {
        self.is_live() && self.term.mode().intersects(TermMode::MOUSE_MODE) && !modifiers.shift
    }

    /// Tells the program about `report` at `position`, if it asked for
    /// that kind.
    fn report_mouse(
        &mut self,
        report: MouseReport,
        position: Point<Pixels>,
        modifiers: &Modifiers,
    ) {
        let (line, column, _) = self.screen_cell(position);
        if let Some(bytes) = encode_mouse(report, column, line, modifiers, *self.term.mode()) {
            self.send(bytes);
        }
        self.reported_cell = Some((line, column));
    }

    /// The link through `point`, if there's one. Links are found in the
    /// whole wrapped line, so one broken across rows still opens.
    fn link_at(&self, point: GridPoint) -> Option<Link> {
        let grid = self.term.grid();
        let last = self.term.last_column();
        let wraps = |line: i32| grid[Line(line)][last].flags.contains(Flags::WRAPLINE);
        let (mut top, mut bottom) = (point.line.0, point.line.0);
        while top > self.term.topmost_line().0 && wraps(top - 1) {
            top -= 1;
        }
        while bottom < self.term.bottommost_line().0 && wraps(bottom) {
            bottom += 1;
        }
        let mut text = String::new();
        // Where each character starts in `text`, and its cell.
        let mut cells = Vec::new();
        for line in top..=bottom {
            for column in 0..=last.0 {
                let cell = &grid[Line(line)][Column(column)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                cells.push((text.len(), GridPoint::new(Line(line), Column(column))));
                text.push(cell.c);
            }
        }
        // A wide character's spacer belongs to the character before it.
        let at = cells[cells.iter().rposition(|(_, cell)| *cell <= point)?].0;
        let range = tern_core::links::find(&text)
            .into_iter()
            .find(|range| range.contains(&at))?;
        let first = cells.iter().find(|(start, _)| *start == range.start)?.1;
        let last = cells.iter().rev().find(|(start, _)| *start < range.end)?.1;
        Some(Link {
            cells: first..=last,
            url: text[range].to_string(),
        })
    }

    /// Underlines the link under the pointer while Ctrl (Cmd) is held.
    fn hover_link(
        &mut self,
        position: Point<Pixels>,
        modifiers: Modifiers,
        cx: &mut Context<Self>,
    ) {
        let link = (modifiers.secondary() && !self.selecting && self.over_grid(position))
            .then(|| self.link_at(self.cell_at(position).0))
            .flatten();
        if link != self.link {
            self.link = link;
            cx.notify();
        }
    }

    /// Opens a link on Ctrl+click (Cmd+click), passes the button to a
    /// program that asked for the mouse, or starts a selection: by
    /// character, by word on a double-click, by line on a triple-click.
    /// Shift+click extends the current one instead.
    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus(window, cx);
        if event.modifiers.secondary()
            && self.over_grid(event.position)
            && let Some(link) = self.link_at(self.cell_at(event.position).0)
        {
            cx.open_url(&link.url);
            return;
        }
        if self.reports_mouse(&event.modifiers) {
            self.pressed = Some(event.button);
            self.report_mouse(
                MouseReport::Press(event.button),
                event.position,
                &event.modifiers,
            );
            return;
        }
        // Right-click opens the menu (with Shift, even in programs that
        // take the mouse).
        if event.button == MouseButton::Right {
            self.menu = Some(event.position);
            cx.notify();
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        let (point, side) = self.cell_at(event.position);
        let kind = match event.click_count {
            0 | 1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        match &mut self.term.selection {
            Some(selection) if event.modifiers.shift && kind == SelectionType::Simple => {
                selection.update(point, side)
            }
            _ => self.term.selection = Some(Selection::new(kind, point, side)),
        }
        self.selecting = true;
        cx.notify();
    }

    fn drag_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        let (point, side) = self.cell_at(position);
        if let Some(selection) = &mut self.term.selection {
            selection.update(point, side);
            cx.notify();
        }
    }

    /// A button comes up: the program hears it if it heard it go down;
    /// otherwise a selection ends.
    fn mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.pressed == Some(event.button) {
            self.pressed = None;
            self.report_mouse(
                MouseReport::Release(event.button),
                event.position,
                &event.modifiers,
            );
        } else if event.button == MouseButton::Left {
            self.end_selection(cx);
        }
    }

    /// Motion: a program that asked hears drags (or all motion) once per
    /// cell; with Ctrl (Cmd) held, a link under the pointer is underlined.
    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.reports_mouse(&event.modifiers) {
            let report = match event.pressed_button {
                Some(button) if self.pressed == Some(button) => Some(MouseReport::Drag(button)),
                Some(_) => None,
                None => Some(MouseReport::Move),
            };
            let (line, column, _) = self.screen_cell(event.position);
            if let Some(report) = report
                && self.reported_cell != Some((line, column))
            {
                self.report_mouse(report, event.position, &event.modifiers);
            }
        }
        self.hover_link(event.position, event.modifiers, cx);
    }

    /// A drag or click ends; with copy on select, the selection goes to
    /// the clipboard.
    fn end_selection(&mut self, cx: &mut Context<Self>) {
        let dragged = std::mem::take(&mut self.selecting);
        if dragged && settings::current(cx).terminal.copy_on_select {
            self.copy(cx);
        }
    }

    /// The right-click menu: Copy (with a selection), Paste, Select all,
    /// Find, Clear scrollback, Save output.
    fn context_menu(&self, at: Point<Pixels>, t: &Theme, cx: &Context<Self>) -> AnyElement {
        let has_selection = self
            .term
            .selection_to_string()
            .is_some_and(|text| !text.is_empty());
        let item = |id: &'static str, label: &'static str, enabled: bool| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .h(px(32.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(px(9.))
                .text_color(if enabled { t.text } else { t.text3 })
                .when(enabled, |d| d.cursor_pointer().hover(|d| d.bg(t.card2)))
                .child(label)
        };
        let close = |this: &mut Self, cx: &mut Context<Self>| {
            this.menu = None;
            cx.notify();
        };
        deferred(
            anchored()
                .position(at)
                .snap_to_window_with_margin(px(8.))
                .child(
                    div()
                        .id("terminal-menu")
                        .occlude()
                        .w(px(200.))
                        .p(px(6.))
                        .flex()
                        .flex_col()
                        .rounded(px(14.))
                        .bg(t.card)
                        .border_1()
                        .border_color(t.line)
                        .shadow(t.shadow())
                        .text_size(px(13.))
                        .on_mouse_down_out(cx.listener(move |this, _, _, cx| close(this, cx)))
                        .child(
                            item("menu-copy", "Copy", has_selection).when(has_selection, |d| {
                                d.on_click(cx.listener(move |this, _, _, cx| {
                                    this.copy(cx);
                                    close(this, cx);
                                }))
                            }),
                        )
                        .child(
                            item("menu-paste", "Paste", self.is_live()).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    close(this, cx);
                                    if this.is_live() {
                                        this.paste(cx);
                                    }
                                },
                            )),
                        )
                        .child(
                            item("menu-select-all", "Select all", true).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.select_all();
                                    close(this, cx);
                                },
                            )),
                        )
                        .child(item("menu-find", "Find", true).on_click(cx.listener(
                            move |this, _, window, cx| {
                                close(this, cx);
                                this.open_find(window, cx);
                            },
                        )))
                        .child(div().my(px(4.)).h(px(1.)).bg(t.line))
                        .child(
                            item("menu-clear", "Clear scrollback", true).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    // CSI 3 J: erase the lines above the screen.
                                    this.parser.advance(&mut this.term, b"\x1b[3J");
                                    this.term.scroll_display(Scroll::Bottom);
                                    close(this, cx);
                                },
                            )),
                        )
                        .child(item("menu-save", "Save output", true).on_click(cx.listener(
                            move |this, _, _, cx| {
                                close(this, cx);
                                this.save_output(cx);
                            },
                        ))),
                ),
        )
        .with_priority(1)
        .into_any_element()
    }

    /// Everything in the scrollback and on screen, as text, without the
    /// blank lines below the last output.
    pub(crate) fn output_text(&self) -> String {
        let top = GridPoint::new(self.term.topmost_line(), Column(0));
        let bottom = GridPoint::new(self.term.bottommost_line(), self.term.last_column());
        let text = self.term.bounds_to_string(top, bottom);
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        let end = lines
            .iter()
            .rposition(|line| !line.is_empty())
            .map_or(0, |last| last + 1);
        let mut saved = lines[..end].join("\n");
        saved.push('\n');
        saved
    }

    /// Asks where, then writes the output there as text.
    fn save_output(&mut self, cx: &mut Context<Self>) {
        let text = self.output_text();
        let name: String = self
            .label()
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '.' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let dir = std::env::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
        let asked = cx.prompt_for_new_path(&dir, Some(&format!("{name}.txt")));
        cx.spawn(async move |_, _| {
            if let Ok(Ok(Some(path))) = asked.await {
                let _ = std::fs::write(path, text);
            }
        })
        .detach();
    }

    /// Lights the pane briefly, for the bell.
    fn flash(&mut self, cx: &mut Context<Self>) {
        self.flash_until = Some(Instant::now() + BELL_FLASH);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(BELL_FLASH).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    fn copy(&self, cx: &mut Context<Self>) {
        if let Some(text) = self
            .term
            .selection_to_string()
            .filter(|text| !text.is_empty())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Selects the whole scrollback and screen.
    fn select_all(&mut self) {
        let top = GridPoint::new(self.term.topmost_line(), Column(0));
        let bottom = GridPoint::new(self.term.bottommost_line(), self.term.last_column());
        let mut all = Selection::new(SelectionType::Lines, top, Side::Left);
        all.update(bottom, Side::Right);
        self.term.selection = Some(all);
    }

    /// The wheel goes to a program that asked for the mouse, turns into
    /// arrow keys for a full-screen one that didn't, and otherwise scrolls
    /// back.
    fn scroll(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let line_height = px(settings::current(cx).terminal.font_size * LINE_SPACING);
        let lines = (event.delta.pixel_delta(line_height).y / line_height).round() as i32;
        if lines == 0 {
            return;
        }
        let mode = *self.term.mode();
        if self.reports_mouse(&event.modifiers) {
            let report = if lines > 0 {
                MouseReport::WheelUp
            } else {
                MouseReport::WheelDown
            };
            for _ in 0..lines.unsigned_abs() {
                self.report_mouse(report, event.position, &event.modifiers);
            }
        } else if self.is_live()
            && mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
            && !event.modifiers.shift
        {
            self.send(wheel_keys(lines, mode));
        } else {
            self.term.scroll_display(Scroll::Delta(lines));
            cx.notify();
        }
    }

    /// Opens the find bar, or puts the cursor back in it.
    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx));
            let subscription = cx.subscribe_in(
                &input,
                window,
                |this, input, event: &InputEvent, _, cx| match event {
                    // Also raised by keys that leave the text as it was.
                    InputEvent::Change => {
                        let query = input.read(cx).value().to_string();
                        if this.find.as_ref().is_some_and(|find| find.query != query) {
                            this.search(query, cx);
                        }
                    }
                    // Enter goes back in time, Shift+Enter forward.
                    InputEvent::PressEnter { shift, .. } => this.step(
                        if *shift {
                            Direction::Right
                        } else {
                            Direction::Left
                        },
                        cx,
                    ),
                    _ => {}
                },
            );
            self.find = Some(Find {
                input,
                query: String::new(),
                regex: None,
                count: (0, 0),
                _subscription: subscription,
            });
        }
        if let Some(find) = &self.find {
            find.input.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    /// Closes the find bar, leaving the match it was on selected.
    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find = None;
        self.focus(window, cx);
        cx.notify();
    }

    /// Searches for `query` (plain text; case matters only if it has
    /// capitals) and goes to the newest match.
    fn search(&mut self, query: String, cx: &mut Context<Self>) {
        let Some(find) = &mut self.find else {
            return;
        };
        find.regex = (!query.is_empty())
            .then(|| RegexSearch::new(&literal(&query)).ok())
            .flatten();
        find.query = query;
        self.term.selection = None;
        self.step(Direction::Left, cx);
    }

    /// Goes to the next match back in time (`Left`) or forward (`Right`)
    /// from the one selected, wrapping around; from the bottom when none
    /// is.
    fn step(&mut self, direction: Direction, cx: &mut Context<Self>) {
        let Some(find) = &mut self.find else {
            return;
        };
        let current = self
            .term
            .selection
            .as_ref()
            .and_then(|selection| selection.to_range(&self.term));
        let origin = match current {
            Some(range) if direction == Direction::Left => {
                range.start.sub(&self.term, Boundary::None, 1)
            }
            Some(range) => range.end.add(&self.term, Boundary::None, 1),
            None => GridPoint::new(self.term.bottommost_line(), self.term.last_column()),
        };
        let found = find.regex.as_mut().and_then(|regex| {
            self.term
                .search_next(regex, origin, direction, Side::Left, None)
        });
        self.term.selection = found.as_ref().map(|found| {
            let mut selection = Selection::new(SelectionType::Simple, *found.start(), Side::Left);
            selection.update(*found.end(), Side::Right);
            selection
        });
        if let Some(found) = &found {
            self.term.scroll_to_point(*found.start());
        }
        // Counted afresh each time: output may have added matches since.
        find.count = match (&mut find.regex, &found) {
            (Some(regex), Some(found)) => {
                let top = GridPoint::new(self.term.topmost_line(), Column(0));
                let bottom = GridPoint::new(self.term.bottommost_line(), self.term.last_column());
                let all: Vec<Match> =
                    RegexIter::new(top, bottom, Direction::Right, &self.term, regex).collect();
                let at = all
                    .iter()
                    .position(|each| each == found)
                    .map_or(0, |at| at + 1);
                (at, all.len())
            }
            _ => (0, 0),
        };
        cx.notify();
    }

    /// The matches on screen, for painting.
    fn visible_matches(&mut self) -> Vec<Match> {
        let Some(regex) = self.find.as_mut().and_then(|find| find.regex.as_mut()) else {
            return Vec::new();
        };
        let offset = self.term.grid().display_offset();
        let rows = usize::from(self.size.rows);
        let top = viewport_to_point(offset, GridPoint::new(0, Column(0)));
        let bottom = viewport_to_point(
            offset,
            GridPoint::new(rows.saturating_sub(1), self.term.last_column()),
        );
        RegexIter::new(top, bottom, Direction::Right, &self.term, regex).collect()
    }

    fn find_bar(&self, find: &Find, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let (at, total) = find.count;
        let typed = find.regex.is_some();
        let control = |id: &'static str, name: IconName| {
            div()
                .id(id)
                .size(px(30.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(10.))
                .cursor_pointer()
                .hover(|d| d.bg(theme.card2))
                .child(icon(name, 15., theme.text2))
        };
        div()
            .absolute()
            .top(px(10.))
            .right(px(10.))
            // Clicks here are for the bar, not a selection.
            .occlude()
            .w(px(340.))
            .h(px(44.))
            .pl(px(12.))
            .pr(px(6.))
            .flex()
            .items_center()
            .gap(px(2.))
            .rounded(px(14.))
            .bg(theme.card)
            .border_1()
            .border_color(if typed && total == 0 {
                theme.danger
            } else {
                theme.line
            })
            .shadow(theme.shadow_sm())
            .text_color(theme.text)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&find.input).id("find-input").appearance(false)),
            )
            .when(total > 0, |d| {
                d.child(
                    div()
                        .px(px(6.))
                        .flex_none()
                        .font_family(MONO)
                        .text_size(px(12.))
                        .text_color(theme.text2)
                        .child(format!("{at}/{total}")),
                )
            })
            .child(
                control("find-older", IconName::ChevronUp)
                    .on_click(cx.listener(|this, _, _, cx| this.step(Direction::Left, cx))),
            )
            .child(
                control("find-newer", IconName::ChevronDown)
                    .on_click(cx.listener(|this, _, _, cx| this.step(Direction::Right, cx))),
            )
            .child(
                control("find-close", IconName::Close)
                    .on_click(cx.listener(|this, _, window, cx| this.close_find(window, cx))),
            )
            .into_any_element()
    }

    /// Copies the visible grid into plain data the paint pass can own,
    /// with `matches` of a search lit.
    fn frame(&self, palette: &TermPalette, focused: bool, matches: &[Match]) -> Frame {
        let content = self.term.renderable_content();
        let offset = content.display_offset as i32;
        let rows = usize::from(self.size.rows);
        let mut frame = Frame {
            rows: (0..rows).map(|_| RowFrame::default()).collect(),
            cursor: None,
        };

        let cursor = content.cursor;
        let show_cursor = matches!(self.phase, Phase::Live)
            && content.mode.contains(TermMode::SHOW_CURSOR)
            && cursor.shape != CursorShape::Hidden;
        let cursor_row = cursor.point.line.0 + offset;
        let cursor_at = (show_cursor && (0..rows as i32).contains(&cursor_row))
            .then_some((cursor_row as usize, cursor.point.column.0));
        let block_cursor = focused && cursor.shape == CursorShape::Block;
        let selection = content.selection;

        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if !(0..rows as i32).contains(&row) {
                continue;
            }
            let (row, col) = (row as usize, indexed.point.column.0);
            let cell = indexed.cell;
            let mut fg = resolve(cell.fg, content.colors, palette);
            let mut bg = resolve(cell.bg, content.colors, palette);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if cell.flags.contains(Flags::DIM) {
                fg = fg.opacity(0.66);
            }
            if cell.flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            // Selected text is ink on paper, whatever its own colours; the
            // other matches of a search are lit faintly.
            if selection.is_some_and(|range| range.contains(indexed.point)) {
                fg = palette.background;
                bg = palette.foreground;
            } else if matches.iter().any(|lit| lit.contains(&indexed.point)) {
                bg = palette.foreground.opacity(0.25);
            }
            if block_cursor && cursor_at == Some((row, col)) {
                fg = palette.background;
                bg = palette.cursor;
            }

            let line = &mut frame.rows[row];
            if bg != palette.background {
                match line.backgrounds.last_mut() {
                    Some(span) if span.end == col && span.color == bg => span.end += 1,
                    _ => line.backgrounds.push(BgSpan {
                        start: col,
                        end: col + 1,
                        color: bg,
                    }),
                }
            }
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let style = Style {
                color: fg,
                bold: cell.flags.contains(Flags::BOLD),
                italic: cell.flags.contains(Flags::ITALIC),
                underline: cell.flags.intersects(Flags::ALL_UNDERLINES)
                    || self
                        .link
                        .as_ref()
                        .is_some_and(|link| link.cells.contains(&indexed.point)),
                strike: cell.flags.contains(Flags::STRIKEOUT),
            };
            let wide = cell.flags.contains(Flags::WIDE_CHAR);
            // Start a new segment on a style change, and around wide glyphs so
            // a fallback font's width can't push later text off the grid.
            match line.segments.last_mut() {
                Some(segment)
                    if segment.style == style
                        && !segment.wide
                        && !wide
                        && segment.end_col == col =>
                {
                    segment.text.push(cell.c);
                    segment.end_col += 1;
                }
                _ => line.segments.push(Segment {
                    col,
                    end_col: col + if wide { 2 } else { 1 },
                    text: cell.c.to_string(),
                    style,
                    wide,
                }),
            }
        }

        if let Some((row, col)) = cursor_at
            && !block_cursor
        {
            frame.cursor = Some(CursorFrame {
                row,
                col,
                shape: cursor.shape,
                focused,
                color: palette.cursor,
            });
        }
        // Blank runs are spaces; drop the trailing ones so we shape less text.
        for row in &mut frame.rows {
            row.segments
                .retain(|segment| !segment.text.chars().all(|c| c == ' ' || c == '\0'));
        }
        frame
    }

    fn overlay(&self, theme: &Theme, cx: &Context<Self>) -> Option<AnyElement> {
        let card = |title: SharedString, body: Option<AnyElement>| {
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        // Clicks here are for the card, not a selection.
                        .occlude()
                        .max_w(px(520.))
                        .p(px(22.))
                        .rounded(px(22.))
                        .bg(theme.card)
                        .text_color(theme.text)
                        .shadow(theme.shadow())
                        .flex()
                        .flex_col()
                        .gap(px(14.))
                        .child(
                            div()
                                .text_size(px(16.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(title),
                        )
                        .children(body),
                )
                .into_any_element()
        };
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .h(px(38.))
                .px(px(16.))
                .flex()
                .items_center()
                .rounded(px(12.))
                .border_1()
                .border_color(theme.line)
                .bg(if primary { theme.text } else { theme.card })
                .text_color(if primary { theme.card } else { theme.text })
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .child(label)
        };
        let target = format!(
            "{}@{}:{}",
            self.connection.target.user, self.connection.target.host, self.connection.target.port
        );

        if let Some(held) = &self.held {
            let into: SharedString = if self.connection.local {
                "this computer".into()
            } else {
                self.label()
            };
            let (text, action) = match held {
                Held::Paste(text) => (text.clone(), "Paste"),
                Held::Run(bytes) => (String::from_utf8_lossy(bytes).into_owned(), "Run"),
            };
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            let lines: Vec<&str> = text.trim_end_matches('\n').lines().collect();
            let shown = lines.len().min(6);
            let title = match held {
                Held::Paste(_) if lines.len() == 1 => format!("Paste 1 line into {into}?"),
                Held::Paste(_) => format!("Paste {} lines into {into}?", lines.len()),
                Held::Run(_) => format!("Run on {into}?"),
            };
            return Some(card(
                title.into(),
                Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(14.))
                        .child(
                            div()
                                .id("paste-preview")
                                .debug_selector(|| "paste-preview".into())
                                .max_w(px(476.))
                                .p(px(10.))
                                .rounded(px(10.))
                                .bg(theme.card2)
                                .font_family(MONO)
                                .text_size(px(12.))
                                .children(
                                    lines[..shown]
                                        .iter()
                                        .map(|line| div().truncate().child(line.to_string())),
                                )
                                .when(lines.len() > shown, |d| d.child("…")),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(button("paste-confirm", action, true).on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.release(cx);
                                        cx.notify();
                                    },
                                )))
                                .child(button("paste-cancel", "Cancel", false).on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.held = None;
                                        cx.notify();
                                    }),
                                )),
                        )
                        .into_any_element(),
                ),
            ));
        }
        Some(match &self.phase {
            Phase::Live => return None,
            Phase::Connecting if self.reconnect.is_some() => card(
                format!("Reconnecting to {target}").into(),
                Some(
                    div()
                        .flex()
                        .gap(px(8.))
                        // Now, unless an attempt is already on its way.
                        .when(self.input.is_none(), |d| {
                            d.child(
                                button("reconnect-now", "Reconnect now", true)
                                    .on_click(cx.listener(|this, _, _, cx| this.reconnect_now(cx))),
                            )
                        })
                        .child(
                            button("stop-reconnecting", "Cancel", false)
                                .on_click(cx.listener(|this, _, _, cx| this.stop_reconnecting(cx))),
                        )
                        .into_any_element(),
                ),
            ),
            Phase::Connecting if self.connection.local => card(self.label(), None),
            Phase::Connecting => card(target.into(), None),
            Phase::Trust {
                shown, fingerprint, ..
            } => card(
                format!("Trust {shown}?").into(),
                Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(14.))
                        .child(
                            div()
                                .p(px(10.))
                                .rounded(px(10.))
                                .bg(theme.card2)
                                .font_family(MONO)
                                .text_size(px(12.))
                                .child(fingerprint.clone()),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(button("trust", "Trust", true).on_click(
                                    cx.listener(|this, _, _, cx| this.trust_and_connect(cx)),
                                ))
                                .child(button("cancel", "Cancel", false).on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.end("Not trusted".into(), cx);
                                        cx.notify();
                                    },
                                ))),
                        )
                        .into_any_element(),
                ),
            ),
            Phase::Password { typed, rejected } => card(
                format!("Password for {target}").into(),
                Some(
                    div()
                        .w(px(360.))
                        .h(px(40.))
                        .px(px(12.))
                        .flex()
                        .items_center()
                        .rounded(px(10.))
                        .bg(theme.card2)
                        .border_1()
                        .border_color(if *rejected { theme.danger } else { theme.card2 })
                        .font_family(MONO)
                        .child("•".repeat(typed.chars().count()))
                        .into_any_element(),
                ),
            ),
            Phase::Questions {
                prompts,
                answers,
                typed,
            } => {
                let (question, echo) = prompts
                    .questions
                    .get(answers.len())
                    .cloned()
                    .unwrap_or_default();
                let question = question.trim_end_matches(':').trim().to_string();
                card(
                    format!("{question} for {}", prompts.from).into(),
                    Some(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(10.))
                            // The server's own words, when it has any.
                            .when(!prompts.instructions.is_empty(), |d| {
                                d.child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(theme.text2)
                                        .child(prompts.instructions.clone()),
                                )
                            })
                            .child(
                                div()
                                    .id("question")
                                    .debug_selector(|| "question".into())
                                    .w(px(360.))
                                    .h(px(40.))
                                    .px(px(12.))
                                    .flex()
                                    .items_center()
                                    .rounded(px(10.))
                                    .bg(theme.card2)
                                    .font_family(MONO)
                                    .child(if echo {
                                        typed.clone()
                                    } else {
                                        "•".repeat(typed.chars().count())
                                    }),
                            )
                            .into_any_element(),
                    ),
                )
            }
            Phase::Ended(message) => card(
                message.clone(),
                Some(
                    div()
                        .flex()
                        .child(
                            button(
                                "reconnect",
                                if self.connection.local {
                                    "Restart"
                                } else {
                                    "Reconnect"
                                },
                                true,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.connect(cx))),
                        )
                        .into_any_element(),
                ),
            ),
        })
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let palette = TermPalette::current(cx);
        let focused = self.focus.is_focused(window);
        let terminal = settings::current(cx).terminal;
        // Settings changed since: scrollback and the default cursor apply
        // to the running terminal too.
        if self.configured != (terminal.scrollback, terminal.cursor) {
            self.term.set_options(term_config(&terminal));
            self.configured = (terminal.scrollback, terminal.cursor);
        }
        let flashing = self.flash_until.is_some_and(|until| Instant::now() < until);
        let matches = self.visible_matches();
        let frame = Rc::new(self.frame(&palette, focused, &matches));
        let entity = cx.entity();
        let this = entity.downgrade();
        let dragging = this.clone();
        let focus = self.focus.clone();
        let current = self.size;
        let grid = self.grid.clone();

        div()
            .id("terminal")
            .track_focus(&self.focus)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::key_down))
            .on_scroll_wheel(cx.listener(Self::scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Right, cx.listener(Self::mouse_up))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_modifiers_changed(
                cx.listener(|this, event: &ModifiersChangedEvent, window, cx| {
                    this.hover_link(window.mouse_position(), event.modifiers, cx)
                }),
            )
            .when(self.link.is_some(), |d| d.cursor_pointer())
            .relative()
            .size_full()
            .p(px(14.))
            .rounded(px(18.))
            .bg(palette.background)
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let metrics = Metrics::measure(window, &terminal);
                        let size = metrics.grid_size(bounds.size);
                        if size != current {
                            cx.defer(move |cx| {
                                let _ = this.update(cx, |view, cx| view.resize(size, cx));
                            });
                        }
                        grid.set(GridGeometry {
                            origin: bounds.origin,
                            cell_width: metrics.cell_width,
                            line_height: metrics.line_height,
                        });
                        metrics
                    },
                    move |bounds, metrics, window, cx| {
                        window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx);
                        paint_frame(&frame, &metrics, bounds.origin, window, cx);
                        // Follows a selection drag even outside the pane.
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                            if phase == DispatchPhase::Bubble && event.dragging() {
                                let _ = dragging
                                    .update(cx, |view, cx| view.drag_to(event.position, cx));
                            }
                        });
                    },
                )
                .size_full(),
            )
            .when(flashing, |d| {
                d.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .rounded(px(18.))
                        .bg(palette.foreground.opacity(0.14)),
                )
            })
            .children(
                self.find
                    .as_ref()
                    .map(|find| self.find_bar(find, &theme, cx)),
            )
            .children(self.overlay(&theme, cx))
            .children(self.menu.map(|at| self.context_menu(at, &theme, cx)))
    }
}

/// alacritty_terminal's configuration from Tern's terminal settings.
fn term_config(terminal: &TerminalSettings) -> TermConfig {
    let shape = match terminal.cursor {
        CursorStyle::Block => CursorShape::Block,
        CursorStyle::Beam => CursorShape::Beam,
        CursorStyle::Underline => CursorShape::Underline,
    };
    TermConfig {
        scrolling_history: terminal.scrollback,
        default_cursor_style: TermCursorStyle {
            shape,
            blinking: false,
        },
        ..Default::default()
    }
}

/// Font metrics for the grid.
struct Metrics {
    font: Font,
    font_size: Pixels,
    cell_width: Pixels,
    line_height: Pixels,
}

impl Metrics {
    fn measure(window: &mut Window, terminal: &TerminalSettings) -> Self {
        let family = if terminal.font_family.is_empty() {
            MONO.to_string()
        } else {
            terminal.font_family.clone()
        };
        let font = font(family);
        let size = terminal.font_size;
        let font_size = px(size);
        let text = window.text_system();
        let cell_width = text
            .advance(text.resolve_font(&font), font_size, 'm')
            .map(|size| size.width)
            .unwrap_or(px(size * 0.6));
        Self {
            font,
            font_size,
            cell_width,
            line_height: px((size * LINE_SPACING).round()),
        }
    }

    fn grid_size(&self, area: Size<Pixels>) -> TermSize {
        let cols = (area.width / self.cell_width).floor().max(2.) as u16;
        let rows = (area.height / self.line_height).floor().max(1.) as u16;
        TermSize { cols, rows }
    }
}

#[derive(Default)]
struct Frame {
    rows: Vec<RowFrame>,
    cursor: Option<CursorFrame>,
}

#[derive(Default)]
struct RowFrame {
    backgrounds: Vec<BgSpan>,
    segments: Vec<Segment>,
}

struct BgSpan {
    start: usize,
    end: usize,
    color: Hsla,
}

struct Segment {
    col: usize,
    end_col: usize,
    text: String,
    style: Style,
    wide: bool,
}

#[derive(Clone, Copy, PartialEq)]
struct Style {
    color: Hsla,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

struct CursorFrame {
    row: usize,
    col: usize,
    shape: CursorShape,
    focused: bool,
    color: Hsla,
}

fn paint_frame(
    frame: &Frame,
    metrics: &Metrics,
    origin: Point<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let Metrics {
        font,
        font_size,
        cell_width,
        line_height,
    } = metrics;
    let at = |row: usize, col: usize| {
        point(
            origin.x + *cell_width * col as f32,
            origin.y + *line_height * row as f32,
        )
    };

    for (row, line) in frame.rows.iter().enumerate() {
        for span in &line.backgrounds {
            let size = size(*cell_width * (span.end - span.start) as f32, *line_height);
            window.paint_quad(fill(Bounds::new(at(row, span.start), size), span.color));
        }
        for segment in &line.segments {
            let mut font = font.clone();
            if segment.style.bold {
                font.weight = FontWeight::BOLD;
            }
            if segment.style.italic {
                font.style = FontStyle::Italic;
            }
            let color = segment.style.color;
            let run = TextRun {
                len: segment.text.len(),
                font,
                color,
                background_color: None,
                underline: segment.style.underline.then(|| UnderlineStyle {
                    color: Some(color),
                    thickness: px(1.),
                    wavy: false,
                }),
                strikethrough: segment.style.strike.then(|| StrikethroughStyle {
                    color: Some(color),
                    thickness: px(1.),
                }),
            };
            let shaped = window.text_system().shape_line(
                segment.text.clone().into(),
                *font_size,
                &[run],
                None,
            );
            let _ = shaped.paint(
                at(row, segment.col),
                *line_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }

    if let Some(cursor) = &frame.cursor {
        let origin = at(cursor.row, cursor.col);
        let bounds = match (cursor.focused, cursor.shape) {
            (true, CursorShape::Beam) => Bounds::new(origin, size(px(2.), *line_height)),
            (true, CursorShape::Underline) => Bounds::new(
                point(origin.x, origin.y + *line_height - px(2.)),
                size(*cell_width, px(2.)),
            ),
            _ => {
                window.paint_quad(outline(
                    Bounds::new(origin, size(*cell_width, *line_height)),
                    cursor.color,
                    BorderStyle::Solid,
                ));
                return;
            }
        };
        window.paint_quad(fill(bounds, cursor.color));
    }
}

/// Receives typed text (including from input methods). A terminal has no
/// editable text of its own, so the selection queries report an empty buffer.
impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.insert_text(text, cx);
    }

    /// Composition in progress; the committed text arrives through
    /// `replace_text_in_range`.
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        _: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        None
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// Turns a terminal colour into a concrete one using the palette, honouring
/// colours the remote program redefined (OSC 4).
fn resolve(color: AnsiColor, colors: &Colors, palette: &TermPalette) -> Hsla {
    let from_rgb = |rgb: Rgb| {
        Hsla::from(gpui_kit::rgb(
            (u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b),
        ))
    };
    match color {
        AnsiColor::Spec(rgb) => from_rgb(rgb),
        AnsiColor::Named(name) => {
            if let Some(rgb) = colors[name as usize] {
                return from_rgb(rgb);
            }
            match name {
                NamedColor::Foreground | NamedColor::BrightForeground => palette.foreground,
                NamedColor::Background => palette.background,
                NamedColor::Cursor => palette.cursor,
                NamedColor::DimForeground => palette.foreground.opacity(0.66),
                name if (name as usize) < 16 => palette.ansi[name as usize],
                // DimBlack..DimWhite follow Cursor in the enum.
                name => {
                    palette.ansi[(name as usize - NamedColor::DimBlack as usize) % 8].opacity(0.66)
                }
            }
        }
        AnsiColor::Indexed(index) => {
            if let Some(rgb) = colors[index as usize] {
                return from_rgb(rgb);
            }
            match index {
                0..=15 => palette.ansi[index as usize],
                16..=231 => {
                    let i = index - 16;
                    let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
                    from_rgb(Rgb {
                        r: level(i / 36),
                        g: level((i / 6) % 6),
                        b: level(i % 6),
                    })
                }
                _ => {
                    let gray = 8 + (index - 232) * 10;
                    from_rgb(Rgb {
                        r: gray,
                        g: gray,
                        b: gray,
                    })
                }
            }
        }
    }
}

/// The local shell's name, like `pwsh` or `zsh`, for a local tab's title.
fn shell_name() -> String {
    let shell = tern_core::local::default_shell();
    std::path::Path::new(&shell)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or(shell)
}

/// Ctrl+Shift+`key`, or Cmd+`key` on macOS: the terminal's own shortcuts,
/// leaving plain Ctrl+`key` to the shell.
fn is_shortcut(keystroke: &Keystroke, key: &str) -> bool {
    let m = &keystroke.modifiers;
    keystroke.key == key && ((m.control && m.shift) || m.platform)
}

fn is_paste(keystroke: &Keystroke) -> bool {
    is_shortcut(keystroke, "v")
}

fn is_copy(keystroke: &Keystroke) -> bool {
    is_shortcut(keystroke, "c")
}

fn is_select_all(keystroke: &Keystroke) -> bool {
    is_shortcut(keystroke, "a")
}

fn is_find(keystroke: &Keystroke) -> bool {
    is_shortcut(keystroke, "f")
}

/// Font zoom: Ctrl+= or Ctrl++ a point larger, Ctrl+- smaller, Ctrl+0
/// back to the default (`None`); Cmd instead of Ctrl on macOS.
fn zoom_steps(keystroke: &Keystroke) -> Option<Option<i8>> {
    let m = &keystroke.modifiers;
    if !m.secondary() || m.alt {
        return None;
    }
    match keystroke.key.as_str() {
        "=" | "+" => Some(Some(1)),
        "-" if !m.shift => Some(Some(-1)),
        "0" if !m.shift => Some(None),
        _ => None,
    }
}

/// `text` as a pattern that matches only itself.
fn literal(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len());
    for c in text.chars() {
        if r"\.+*?()|[]{}^$#&-~".contains(c) {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern
}

#[cfg(test)]
mod tests {
    // Explicit imports: `gpui_kit::*` brings GPUI's own `test` macro, which
    // would shadow Rust's `#[test]`.
    use gpui::{
        MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point, TestAppContext,
        VisualTestContext, point,
    };
    use gpui_kit::{Entity, Modifiers};
    use tern_core::ssh::{Connection, Event, Target};

    use super::TerminalView;
    use crate::theme::{self, TermPalette};

    /// A live pane showing `output` as a shell would print it, with no
    /// connection behind it.
    fn pane_with<'a>(
        cx: &'a mut TestAppContext,
        output: &str,
    ) -> (Entity<TerminalView>, &'a mut VisualTestContext) {
        cx.update(gpui_kit::init);
        let output = output.as_bytes().to_vec();
        let (view, cx) = cx.add_window_view(move |_, cx| {
            let target = Target {
                host: "example".into(),
                port: 22,
                user: "me".into(),
            };
            let mut view = TerminalView::idle(Connection::typed(target, None), cx);
            view.on_session_event(Event::Connected, cx);
            view.on_session_event(Event::Output(output), cx);
            view
        });
        cx.run_until_parked();
        (view, cx)
    }

    #[gpui::test]
    fn panes_to_one_host_keep_shells_of_their_own(cx: &mut TestAppContext) {
        use gpui::AppContext as _;
        use tern_core::shells;

        cx.update(gpui_kit::init);
        let connection = Connection {
            keep_shell: true,
            host_id: Some("9f39b2acf25b2d3b".into()),
            ..Connection::typed(
                Target {
                    host: "example".into(),
                    port: 22,
                    user: "me".into(),
                },
                None,
            )
        };
        let pane = |cx: &mut TestAppContext| {
            let connection = connection.clone();
            cx.new(|cx| {
                let mut view = TerminalView::idle(connection, cx);
                view.shell = view.claim_shell(cx);
                view
            })
        };
        let shell = |pane: &Entity<TerminalView>, cx: &mut TestAppContext| {
            pane.read_with(cx, |pane, _| pane.shell.clone())
        };
        let kept = |slot| Some(shells::name("9f39b2acf25b2d3b", slot));
        let (first, second) = (pane(cx), pane(cx));
        assert_eq!(shell(&first, cx), kept(1));
        assert_eq!(shell(&second, cx), kept(2));
        // A closed pane's slot goes to the next pane, which finds its shell.
        drop(first);
        // Released as the next update ends.
        cx.update(|_| {});
        assert_eq!(shell(&pane(cx), cx), kept(1));
        // A host that doesn't keep shells gets none.
        let plain = cx.new(|cx| {
            let mut view = TerminalView::idle(
                Connection::typed(
                    Target {
                        host: "example".into(),
                        port: 22,
                        user: "me".into(),
                    },
                    None,
                ),
                cx,
            );
            view.shell = view.claim_shell(cx);
            view
        });
        assert_eq!(shell(&plain, cx), None);
    }

    /// The middle of a cell's left or right half, in the window.
    fn cell(
        view: &Entity<TerminalView>,
        cx: &mut VisualTestContext,
        line: usize,
        column: usize,
        right_half: bool,
    ) -> Point<Pixels> {
        let grid = view.read_with(cx, |view, _| view.grid.get());
        let x = column as f32 + if right_half { 0.75 } else { 0.25 };
        point(
            grid.origin.x + grid.cell_width * x,
            grid.origin.y + grid.line_height * (line as f32 + 0.5),
        )
    }

    /// Presses and releases the left button, as the `clicks`-th click.
    fn click(
        cx: &mut VisualTestContext,
        position: Point<Pixels>,
        clicks: usize,
        modifiers: Modifiers,
    ) {
        cx.simulate_event(MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers,
            click_count: clicks,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers,
            click_count: clicks,
        });
    }

    fn selected(view: &Entity<TerminalView>, cx: &mut VisualTestContext) -> Option<String> {
        view.read_with(cx, |view, _| view.term.selection_to_string())
            .filter(|text| !text.is_empty())
    }

    #[gpui::test]
    fn reports_how_a_session_ended(cx: &mut TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        use tern_core::history::Visit;

        use super::TerminalEvent;

        let (view, cx) = pane_with(cx, "bye");
        let finished: Rc<RefCell<Vec<Visit>>> = Rc::default();
        let recorded = finished.clone();
        cx.update(|_, cx| {
            cx.subscribe(&view, move |_, event: &TerminalEvent, _| {
                if let TerminalEvent::Finished(visit) = event {
                    recorded.borrow_mut().push(visit.clone());
                }
            })
            .detach()
        });
        view.read_with(cx, |view, _| assert!(view.is_live()));
        view.update(cx, |view, cx| {
            view.on_session_event(
                Event::Closed {
                    exit_status: Some(0),
                },
                cx,
            )
        });
        let visit = finished
            .borrow()
            .last()
            .cloned()
            .expect("the end is reported");
        assert_eq!(visit.ended, "Exited with status 0");
        assert_eq!(visit.target(), "me@example");
        assert_eq!(visit.lasted, Some(0));
        view.read_with(cx, |view, _| assert!(!view.is_live()));
    }

    #[gpui::test]
    fn selects_and_copies_text(cx: &mut TestAppContext) {
        let (view, cx) = pane_with(cx, "hello world\r\nsecond line");

        // Dragging selects by character, painted ink on paper.
        let (from, to) = (cell(&view, cx, 0, 0, false), cell(&view, cx, 0, 4, true));
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        assert_eq!(selected(&view, cx).as_deref(), Some("hello"));
        view.read_with(cx, |view, cx| {
            let palette = TermPalette::for_theme(&theme::current(cx));
            let frame = view.frame(&palette, false, &[]);
            let span = &frame.rows[0].backgrounds[0];
            assert_eq!((span.start, span.end), (0, 5));
            assert_eq!(span.color, palette.foreground);
        });

        // The terminal's copy shortcut; plain Ctrl+C stays the shell's.
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-c"
        } else {
            "ctrl-shift-c"
        });
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some("hello"));

        // Shift+click extends; a double-click takes a word, a triple-click
        // the line; a plain click clears.
        let end_of_world = cell(&view, cx, 0, 10, true);
        click(cx, end_of_world, 1, Modifiers::shift());
        assert_eq!(selected(&view, cx).as_deref(), Some("hello world"));
        let in_line = cell(&view, cx, 1, 8, false);
        click(cx, in_line, 2, Modifiers::none());
        assert_eq!(selected(&view, cx).as_deref(), Some("line"));
        click(cx, in_line, 3, Modifiers::none());
        assert_eq!(
            selected(&view, cx).as_deref().map(str::trim_end),
            Some("second line")
        );
        click(cx, in_line, 1, Modifiers::none());
        assert_eq!(selected(&view, cx), None);
    }

    /// Puts the keyboard in the pane.
    fn focus(view: &Entity<TerminalView>, cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            let focus = view.read(cx).focus.clone();
            focus.focus(window, cx);
        });
        cx.run_until_parked();
    }

    /// The terminal's own shortcut for `key`: Ctrl+Shift, or Cmd on macOS.
    fn shortcut(key: &str) -> String {
        if cfg!(target_os = "macos") {
            format!("cmd-{key}")
        } else {
            format!("ctrl-shift-{key}")
        }
    }

    #[gpui::test]
    fn finds_text_in_the_scrollback(cx: &mut TestAppContext) {
        let (view, cx) = pane_with(cx, "alpha one\r\nbeta\r\nALPHA two\r\n");
        let count = |cx: &mut VisualTestContext| {
            view.read_with(cx, |view, _| view.find.as_ref().map(|find| find.count))
        };
        focus(&view, cx);
        cx.simulate_keystrokes(&shortcut("f"));
        cx.run_until_parked();
        cx.simulate_input("alpha");

        // Without capitals either case matches; the newest comes first, and
        // the others on screen are lit.
        assert_eq!(selected(&view, cx).as_deref(), Some("ALPHA"));
        assert_eq!(count(cx), Some((2, 2)));
        view.update(cx, |view, _| assert_eq!(view.visible_matches().len(), 2));

        // Enter goes back, wrapping around; Shift+Enter forward.
        cx.simulate_keystrokes("enter");
        assert_eq!(selected(&view, cx).as_deref(), Some("alpha"));
        assert_eq!(count(cx), Some((1, 2)));
        cx.simulate_keystrokes("enter");
        assert_eq!(count(cx), Some((2, 2)));
        cx.simulate_keystrokes("shift-enter");
        assert_eq!(count(cx), Some((1, 2)));

        // Nothing typed in the bar reaches the shell.
        view.read_with(cx, |view, _| assert!(view.sent.borrow().is_empty()));

        cx.simulate_input("x");
        assert_eq!(selected(&view, cx), None);
        assert_eq!(count(cx), Some((0, 0)));

        // Escape closes the bar and gives the keyboard back to the shell.
        cx.simulate_keystrokes("escape");
        assert_eq!(count(cx), None);
        cx.update(|window, cx| assert!(view.read(cx).is_focused(window)));
    }

    #[gpui::test]
    fn opens_links_on_ctrl_click(cx: &mut TestAppContext) {
        let (view, cx) = pane_with(cx, "see https://example.com/a. ok");
        let on_link = cell(&view, cx, 0, 12, false);
        click(cx, on_link, 1, Modifiers::none());
        assert_eq!(cx.opened_url(), None);

        // With Ctrl (Cmd) held the link under the pointer is underlined,
        // and a click opens it.
        cx.simulate_mouse_move(on_link, None, Modifiers::secondary_key());
        let url = view.read_with(cx, |view, _| view.link.clone().map(|link| link.url));
        assert_eq!(url.as_deref(), Some("https://example.com/a"));
        click(cx, on_link, 1, Modifiers::secondary_key());
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com/a"));

        // Off the link, nothing is.
        let off = cell(&view, cx, 0, 1, false);
        cx.simulate_mouse_move(off, None, Modifiers::secondary_key());
        view.read_with(cx, |view, _| assert_eq!(view.link, None));
    }

    #[gpui::test]
    fn passes_the_mouse_to_programs_that_ask(cx: &mut TestAppContext) {
        // 1000 asks for clicks, 1006 in SGR form.
        let (view, cx) = pane_with(cx, "\x1b[?1000h\x1b[?1006hhello");
        let sent = |cx: &mut VisualTestContext| {
            view.read_with(cx, |view, _| {
                String::from_utf8(view.sent.take()).expect("reports are text")
            })
        };
        let at = cell(&view, cx, 0, 2, false);
        click(cx, at, 1, Modifiers::none());
        assert_eq!(sent(cx), "\x1b[<0;3;1M\x1b[<0;3;1m");
        assert_eq!(selected(&view, cx), None);

        // Shift keeps the mouse for selecting.
        let (from, to) = (cell(&view, cx, 0, 0, false), cell(&view, cx, 0, 4, true));
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::shift());
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::shift());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::shift());
        assert_eq!(selected(&view, cx).as_deref(), Some("hello"));
        assert_eq!(sent(cx), "");

        // Once the program lets go of the mouse, clicks select again.
        view.update(cx, |view, cx| {
            view.on_session_event(Event::Output(b"\x1b[?1000l".to_vec()), cx)
        });
        click(cx, at, 2, Modifiers::none());
        assert_eq!(selected(&view, cx).as_deref(), Some("hello"));
        assert_eq!(sent(cx), "");
    }

    #[gpui::test]
    fn reconnects_after_a_dropped_connection(cx: &mut TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        use alacritty_terminal::grid::Dimensions as _;
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::TermMode;
        use tern_core::ssh::ConnectError;

        use super::{Phase, TerminalEvent};

        // A program had switched mouse reporting on when the line dropped.
        let (view, cx) = pane_with(cx, "before\r\n\x1b[?1000h");
        let ended: Rc<RefCell<Vec<String>>> = Rc::default();
        let recorded = ended.clone();
        cx.update(|_, cx| {
            cx.subscribe(&view, move |_, event: &TerminalEvent, _| {
                if let TerminalEvent::Finished(visit) = event {
                    recorded.borrow_mut().push(visit.ended.clone());
                }
            })
            .detach()
        });
        let event = |event: Event, cx: &mut VisualTestContext| {
            view.update(cx, |view, cx| view.on_session_event(event, cx))
        };
        let attempt = |cx: &mut VisualTestContext| view.read_with(cx, |view, _| view.reconnect);

        event(Event::Lost, cx);
        assert_eq!(attempt(cx), Some(1));
        assert_eq!(*ended.borrow(), ["Connection lost"]);
        // What may pass by itself is tried again, and History hears
        // nothing of it.
        event(
            Event::Failed(ConnectError::Other("connection refused".into())),
            cx,
        );
        assert_eq!(attempt(cx), Some(2));
        assert_eq!(ended.borrow().len(), 1);

        // Back on a clear screen: what was on it is in the scrollback, and
        // the program's modes are gone.
        event(Event::Connected, cx);
        view.read_with(cx, |view, _| {
            assert!(view.is_live() && view.reconnect.is_none());
            assert!(!view.term.mode().intersects(TermMode::MOUSE_MODE));
            let row = |line: Line| -> String {
                (0..6)
                    .map(|c| view.term.grid()[line][Column(c)].c)
                    .collect()
            };
            assert_eq!(row(view.term.topmost_line()), "before");
            assert_eq!(row(Line(0)).trim(), "");
        });

        // A refused sign-in needs the user.
        event(Event::Lost, cx);
        event(
            Event::Failed(ConnectError::AuthFailed { tried: Vec::new() }),
            cx,
        );
        view.read_with(cx, |view, _| {
            assert!(view.reconnect.is_none());
            assert!(matches!(view.phase, Phase::Password { .. }));
        });

        // Escape stops waiting.
        event(Event::Connected, cx);
        event(Event::Lost, cx);
        focus(&view, cx);
        cx.simulate_keystrokes("escape");
        view.read_with(cx, |view, _| {
            assert!(view.reconnect.is_none());
            assert!(matches!(&view.phase, Phase::Ended(message) if message == "Connection lost"));
        });
    }

    #[gpui::test]
    fn asks_the_servers_questions_one_at_a_time(cx: &mut TestAppContext) {
        use gpui_kit::ClipboardItem;
        use tern_core::ssh::Prompts;

        use super::Phase;

        let (view, cx) = pane_with(cx, "");
        view.update(cx, |view, cx| {
            view.phase = super::Phase::Connecting;
            view.on_session_event(
                Event::Prompts(Prompts {
                    from: "me@example".into(),
                    instructions: String::new(),
                    questions: vec![("Verification code:".into(), true), ("PIN:".into(), false)],
                }),
                cx,
            )
        });
        focus(&view, cx);
        // A pasted code takes its first line.
        cx.write_to_clipboard(ClipboardItem::new_string("123456\nmore".into()));
        cx.simulate_keystrokes(&shortcut("v"));
        cx.simulate_keystrokes("enter");
        view.read_with(cx, |view, _| {
            assert!(matches!(&view.phase, Phase::Questions { answers, .. } if answers.len() == 1))
        });
        cx.simulate_input("42");
        cx.simulate_keystrokes("enter");
        view.read_with(cx, |view, _| {
            assert_eq!(view.answered, ["123456", "42"]);
            assert!(matches!(view.phase, Phase::Connecting));
            // Nothing of it went to a shell.
            assert!(view.sent.borrow().is_empty());
        });
    }

    #[gpui::test]
    fn asks_before_pasting_lines_the_shell_would_run(cx: &mut TestAppContext) {
        use gpui_kit::ClipboardItem;

        let (view, cx) = pane_with(cx, "");
        let sent = |cx: &mut VisualTestContext| {
            view.read_with(cx, |view, _| String::from_utf8(view.sent.take()).unwrap())
        };
        focus(&view, cx);
        cx.write_to_clipboard(ClipboardItem::new_string("make\nmake install\n".into()));

        // Escape drops it; Enter pastes it.
        cx.simulate_keystrokes(&shortcut("v"));
        assert_eq!(sent(cx), "");
        assert!(cx.debug_bounds("paste-preview").is_some());
        cx.simulate_keystrokes("escape");
        assert_eq!(sent(cx), "");
        cx.simulate_keystrokes(&shortcut("v"));
        cx.simulate_keystrokes("enter");
        assert_eq!(sent(cx), "make\rmake install\r");

        // A shell with bracketed paste gets it at once, marked as pasted.
        view.update(cx, |view, cx| {
            view.on_session_event(Event::Output(b"\x1b[?2004h".to_vec()), cx)
        });
        cx.simulate_keystrokes(&shortcut("v"));
        assert_eq!(sent(cx), "\x1b[200~make\rmake install\r\x1b[201~");
    }

    #[gpui::test]
    fn the_right_click_menu_copies_clears_and_saves(cx: &mut TestAppContext) {
        use alacritty_terminal::grid::Dimensions as _;

        let output: String = (0..200).map(|n| format!("line {n}\r\n")).collect();
        let (view, cx) = pane_with(cx, &output);
        let menu = |cx: &mut VisualTestContext, item: &'static str| {
            let at = cell(&view, cx, 2, 2, false);
            cx.simulate_event(MouseDownEvent {
                button: MouseButton::Right,
                position: at,
                modifiers: Modifiers::none(),
                click_count: 1,
                first_mouse: false,
            });
            cx.run_until_parked();
            let bounds = cx
                .debug_bounds(item)
                .unwrap_or_else(|| panic!("the menu has no {item}"));
            cx.simulate_click(bounds.center(), Modifiers::none());
            cx.run_until_parked();
        };

        menu(cx, "menu-select-all");
        menu(cx, "menu-copy");
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap();
        assert!(copied.contains("line 0") && copied.contains("line 199"));

        // Saved as text, scrollback and screen, without the blank end.
        let dir = std::env::temp_dir().join(format!("tern-ui-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("output.txt");
        menu(cx, "menu-save");
        let chosen = path.clone();
        cx.simulate_new_path_selection(move |_| Some(chosen));
        cx.run_until_parked();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.starts_with("line 0\n"), "{saved}");
        assert!(saved.ends_with("line 199\n"), "{saved}");
        std::fs::remove_dir_all(&dir).unwrap();

        // Clear scrollback leaves only the screen.
        assert!(view.read_with(cx, |view, _| view.term.grid().history_size()) > 0);
        menu(cx, "menu-clear");
        assert_eq!(
            view.read_with(cx, |view, _| view.term.grid().history_size()),
            0
        );
    }

    #[gpui::test]
    fn zooms_the_font(cx: &mut TestAppContext) {
        let (view, cx) = pane_with(cx, "");
        let size = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| crate::settings::current(cx).terminal.font_size)
        };
        let ctrl = if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        };
        focus(&view, cx);
        cx.simulate_keystrokes(&format!("{ctrl}-="));
        assert_eq!(size(cx), 14.5);
        cx.simulate_keystrokes(&format!("{ctrl}-- {ctrl}--"));
        assert_eq!(size(cx), 12.5);
        cx.simulate_keystrokes(&format!("{ctrl}-0"));
        assert_eq!(size(cx), 13.5);
        // The shell never sees them.
        view.read_with(cx, |view, _| assert!(view.sent.borrow().is_empty()));
    }
}
