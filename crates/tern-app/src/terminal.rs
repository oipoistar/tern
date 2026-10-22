//! A terminal pane: an SSH session from tern-core, emulated by
//! alacritty_terminal and drawn cell by cell with GPUI.
//!
//! Before the shell is live the pane shows what the connection needs: trust
//! for an unknown host key, a password, or a way to reconnect.
//!
//! Dragging selects text (double-click a word, triple-click a line,
//! Shift+click extends), painted inverted; Ctrl+Shift+C (Cmd+C on macOS)
//! copies it.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Instant;

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode, viewport_to_point};
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape, CursorStyle as TermCursorStyle, NamedColor, Processor, Rgb,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::forward::{Forward, ForwardState};
use tern_core::history::{self, Visit};
use tern_core::settings::{Bell, CursorStyle, TerminalSettings};
use tern_core::sftp::Sftp;
use tern_core::ssh::{ConnectError, Connection, Event, HostKey, Session, SessionInput, TermSize};

use crate::settings;
use crate::theme::{self, MONO, TermPalette, Theme};

/// Line height as a multiple of the font size.
const LINE_SPACING: f32 = 1.35;
/// How long the visual bell lights the pane.
const BELL_FLASH: std::time::Duration = std::time::Duration::from_millis(120);
/// Lines a session's preview keeps: those ending at the cursor.
const PREVIEW_LINES: i32 = 9;

/// The last lines of a session, shared with the vault's side panel.
pub type Preview = Rc<RefCell<Vec<String>>>;

/// Where the connection stands.
pub enum Phase {
    Connecting,
    /// A server's key is unknown (the target's, or the jump host's);
    /// waiting for the user to trust it.
    Trust {
        host: SharedString,
        key: HostKey,
        fingerprint: SharedString,
    },
    /// Key-based login failed; collecting a password.
    Password {
        typed: String,
        rejected: bool,
    },
    Live,
    Ended(SharedString),
}

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
    /// The user typed or pasted this into the shell, for broadcasting to
    /// other panes.
    Typed(Vec<u8>),
}

impl EventEmitter<TerminalEvent> for TerminalView {}

pub struct TerminalView {
    connection: Connection,
    password: Option<String>,
    trust: Option<HostKey>,
    pub phase: Phase,
    input: Option<SessionInput>,
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
    preview: Preview,
    /// The scrollback and cursor the terminal was last configured with.
    configured: (usize, CursorStyle),
    /// The visual bell lights the pane until then.
    flash_until: Option<Instant>,
    /// Everything sent to the session, for tests to look at.
    #[cfg(test)]
    pub sent: RefCell<Vec<u8>>,
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
        view.connect(cx);
        view
    }

    /// The password saved for this connection's host, if it signs in with
    /// one; it's tried before asking.
    fn saved_password(&self, cx: &App) -> Option<String> {
        let id = self.connection.host_id.as_deref()?;
        (self.connection.auth == tern_core::Auth::Password)
            .then(|| crate::sync::secrets(cx).load(&tern_core::secrets::host_password(id)))
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
    fn idle(connection: Connection, cx: &mut Context<Self>) -> Self {
        let size = TermSize {
            cols: 100,
            rows: 30,
        };
        let term_events = Rc::new(RefCell::new(Vec::new()));
        let terminal = settings::current(cx).terminal;
        let config = term_config(&terminal);
        Self {
            connection,
            password: None,
            trust: None,
            phase: Phase::Connecting,
            input: None,
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
            preview: Preview::default(),
            configured: (terminal.scrollback, terminal.cursor),
            flash_until: None,
            #[cfg(test)]
            sent: RefCell::default(),
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus.focus(window, cx);
    }

    /// Tab label: the remote title if the shell set one, else `user@host`.
    pub fn title(&self) -> SharedString {
        self.title.clone().unwrap_or_else(|| {
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
        cx.emit(TerminalEvent::Finished(self.visit(&message)));
        self.phase = Phase::Ended(message);
        self.live_since = None;
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        // Answering a trust or password prompt continues the same attempt.
        if !matches!(self.phase, Phase::Trust { .. } | Phase::Password { .. }) {
            self.started = history::now();
        }
        let request =
            self.connection
                .request(self.size, self.password.as_deref(), self.trust.clone());
        let (input, mut events) = Session::open(request).into_parts();
        self.input = Some(input);
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

    fn on_session_event(&mut self, event: Event, cx: &mut Context<Self>) {
        match event {
            Event::Connected => {
                self.phase = Phase::Live;
                self.live_since = Some(Instant::now());
                self.term = Term::new(
                    term_config(&settings::current(cx).terminal),
                    &GridSize(self.size),
                    Listener(self.term_events.clone()),
                );
                // Queued now, it reaches the shell as soon as it starts.
                if let Some(command) = &self.connection.on_login {
                    self.send(format!("{command}\r").into_bytes());
                }
            }
            Event::Output(bytes) => {
                self.parser.advance(&mut self.term, &bytes);
                self.answer_terminal_events(cx);
                self.update_preview();
            }
            Event::Forwarding { forward, state } => {
                if let Some(host) = self.connection.host.clone() {
                    cx.emit(TerminalEvent::Forwarding {
                        host,
                        forward,
                        state,
                    });
                }
            }
            Event::Closed { exit_status } => {
                self.input = None;
                let message = match exit_status {
                    Some(status) => format!("Exited with status {status}").into(),
                    None => "Connection closed".into(),
                };
                self.end(message, cx);
            }
            Event::Failed(error) => {
                self.input = None;
                match error {
                    ConnectError::UnknownHostKey { host, port, key, fingerprint } => self.phase = Phase::Trust {
                        host: if port == 22 { host.into() } else { format!("{host}:{port}").into() },
                        key: *key,
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
                    ConnectError::Other(message) => self.end(message.into(), cx),
                }
            }
        }
        cx.notify();
    }

    /// Replies to queries the remote program made of the terminal (cursor
    /// position, colours) and picks up title changes.
    fn answer_terminal_events(&mut self, cx: &mut Context<Self>) {
        let events: Vec<TermEvent> = self.term_events.borrow_mut().drain(..).collect();
        for event in events {
            match event {
                TermEvent::PtyWrite(text) => self.send(text.into_bytes()),
                TermEvent::Title(title) => self.title = Some(title.into()),
                TermEvent::ResetTitle => self.title = None,
                TermEvent::ColorRequest(index, reply) => {
                    let palette = TermPalette::for_theme(&theme::current(cx));
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
                TermEvent::Bell if settings::current(cx).terminal.bell == Bell::Visual => {
                    self.flash(cx)
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
        if let Phase::Trust { key, .. } = &self.phase {
            self.trust = Some(key.clone());
            self.connect(cx);
        }
    }

    fn key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
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
            // Typed characters arrive through `replace_text_in_range`.
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
            Phase::Trust { .. } => match keystroke.key.as_str() {
                "enter" => self.trust_and_connect(cx),
                "escape" => self.end("Not trusted".into(), cx),
                _ => return,
            },
            Phase::Ended(_) if keystroke.key == "enter" => self.connect(cx),
            _ => return,
        }
        cx.stop_propagation();
        cx.notify();
    }

    /// Text from the keyboard or an input method.
    fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        match &mut self.phase {
            Phase::Live => self.send_typed(text.as_bytes().to_vec(), cx),
            Phase::Password { typed, .. } => typed.push_str(text),
            _ => return,
        }
        cx.notify();
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        // Terminals expect CR for newlines; bracketed paste lets the shell
        // tell pasted text from typed text.
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bytes = if self.term.mode().contains(TermMode::BRACKETED_PASTE) {
            format!("\x1b[200~{text}\x1b[201~")
        } else {
            text
        };
        self.send_typed(bytes.into_bytes(), cx);
    }

    /// The cell under `position` in the window, and which half of it.
    fn cell_at(&self, position: Point<Pixels>) -> (GridPoint, Side) {
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
        let offset = self.term.grid().display_offset();
        (
            viewport_to_point(offset, GridPoint::new(line, Column(column))),
            side,
        )
    }

    /// Starts a selection: by character, by word on a double-click, by line
    /// on a triple-click. Shift+click extends the current one instead.
    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus(window, cx);
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

    /// A drag or click ends; with copy on select, the selection goes to
    /// the clipboard.
    fn end_selection(&mut self, cx: &mut Context<Self>) {
        let dragged = std::mem::take(&mut self.selecting);
        if dragged && settings::current(cx).terminal.copy_on_select {
            self.copy(cx);
        }
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

    fn scroll(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let line_height = px(settings::current(cx).terminal.font_size * LINE_SPACING);
        let lines = (event.delta.pixel_delta(line_height).y / line_height).round() as i32;
        if lines != 0 {
            self.term.scroll_display(Scroll::Delta(lines));
            cx.notify();
        }
    }

    /// Copies the visible grid into plain data the paint pass can own.
    fn frame(&self, palette: &TermPalette, focused: bool) -> Frame {
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
            // Selected text is ink on paper, whatever its own colours.
            if selection.is_some_and(|range| range.contains(indexed.point)) {
                fg = palette.background;
                bg = palette.foreground;
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
                underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
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

        Some(match &self.phase {
            Phase::Live => return None,
            Phase::Connecting => card(target.into(), None),
            Phase::Trust {
                host, fingerprint, ..
            } => card(
                format!("Trust {host}?").into(),
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
            Phase::Ended(message) => card(
                message.clone(),
                Some(
                    div()
                        .flex()
                        .child(
                            button("reconnect", "Reconnect", true)
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
        let palette = TermPalette::for_theme(&theme);
        let focused = self.focus.is_focused(window);
        let terminal = settings::current(cx).terminal;
        // Settings changed since: scrollback and the default cursor apply
        // to the running terminal too.
        if self.configured != (terminal.scrollback, terminal.cursor) {
            self.term.set_options(term_config(&terminal));
            self.configured = (terminal.scrollback, terminal.cursor);
        }
        let flashing = self.flash_until.is_some_and(|until| Instant::now() < until);
        let frame = Rc::new(self.frame(&palette, focused));
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
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_selection(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_selection(cx)),
            )
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
            .children(self.overlay(&theme, cx))
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

/// Encodes a key press as the bytes an xterm-compatible terminal would send.
fn encode_key(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let m = &keystroke.modifiers;
    let key = keystroke.key.as_str();
    // xterm's modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    let modifier = 1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.control);
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let csi = |code: &str, final_byte: char| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[{code};{modifier}{final_byte}").into_bytes()
        } else {
            format!("\x1b[{code}{final_byte}").into_bytes()
        }
    };
    let arrow = |letter: char| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{letter}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    let bytes = match key {
        "enter" => b"\r".to_vec(),
        "tab" if m.shift => b"\x1b[Z".to_vec(),
        "tab" => b"\t".to_vec(),
        "backspace" if m.alt => b"\x1b\x7f".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "escape" => b"\x1b".to_vec(),
        "up" => arrow('A'),
        "down" => arrow('B'),
        "right" => arrow('C'),
        "left" => arrow('D'),
        "home" => arrow('H'),
        "end" => arrow('F'),
        "insert" => csi("2", '~'),
        "delete" => csi("3", '~'),
        "pageup" => csi("5", '~'),
        "pagedown" => csi("6", '~'),
        "f1" => b"\x1bOP".to_vec(),
        "f2" => b"\x1bOQ".to_vec(),
        "f3" => b"\x1bOR".to_vec(),
        "f4" => b"\x1bOS".to_vec(),
        "f5" => csi("15", '~'),
        "f6" => csi("17", '~'),
        "f7" => csi("18", '~'),
        "f8" => csi("19", '~'),
        "f9" => csi("20", '~'),
        "f10" => csi("21", '~'),
        "f11" => csi("23", '~'),
        "f12" => csi("24", '~'),
        "space" if m.control => vec![0],
        _ if m.control && !m.alt && key.len() == 1 => {
            let c = key.as_bytes()[0].to_ascii_lowercase();
            match c {
                b'a'..=b'z' => vec![c - b'a' + 1],
                b'[' => vec![0x1b],
                b'\\' => vec![0x1c],
                b']' => vec![0x1d],
                b'2' | b'@' => vec![0],
                b'6' | b'^' => vec![0x1e],
                b'-' | b'_' => vec![0x1f],
                _ => return None,
            }
        }
        // Alt+character is ESC then the character (meta key).
        _ if m.alt && !m.control && !m.platform => {
            let text = keystroke.key_char.as_deref().unwrap_or(key);
            [b"\x1b".as_slice(), text.as_bytes()].concat()
        }
        // Plain text arrives through the input handler instead, which also
        // covers IME and characters injected without a key code.
        _ => return None,
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    // Explicit imports: `gpui_kit::*` brings GPUI's own `test` macro, which
    // would shadow Rust's `#[test]`.
    use alacritty_terminal::term::TermMode;
    use gpui::{
        MouseButton, MouseDownEvent, MouseUpEvent, Pixels, Point, TestAppContext,
        VisualTestContext, point,
    };
    use gpui_kit::{Entity, Keystroke, Modifiers};
    use tern_core::ssh::{Connection, Event, Target};

    use super::{TerminalView, encode_key};
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
            let frame = view.frame(&palette, false);
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

    fn key(key: &str, key_char: Option<&str>, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(Into::into),
        }
    }

    fn ctrl() -> Modifiers {
        Modifiers {
            control: true,
            ..Default::default()
        }
    }

    #[test]
    fn encodes_common_keys() {
        let none = TermMode::empty();
        // Plain text is left to the input handler.
        assert_eq!(
            encode_key(&key("a", Some("a"), Modifiers::default()), none),
            None
        );
        assert_eq!(
            encode_key(&key("enter", None, Modifiers::default()), none),
            Some(b"\r".to_vec())
        );
        assert_eq!(encode_key(&key("c", None, ctrl()), none), Some(vec![3]));
        assert_eq!(
            encode_key(&key("up", None, Modifiers::default()), none),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode_key(&key("up", None, Modifiers::default()), TermMode::APP_CURSOR),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode_key(&key("right", None, ctrl()), none),
            Some(b"\x1b[1;5C".to_vec())
        );
        assert_eq!(
            encode_key(&key("delete", None, Modifiers::default()), none),
            Some(b"\x1b[3~".to_vec())
        );
    }

    #[test]
    fn alt_prefixes_escape() {
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(
            encode_key(&key("b", Some("b"), alt), TermMode::empty()),
            Some(b"\x1bb".to_vec())
        );
    }
}
