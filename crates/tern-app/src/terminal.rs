//! A terminal pane: an SSH session from tern-core, emulated by
//! alacritty_terminal and drawn cell by cell with GPUI.
//!
//! Before the shell is live the pane shows what the connection needs: trust
//! for an unknown host key, a password, or a way to reconnect.

use std::cell::RefCell;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, CursorShape, NamedColor, Processor, Rgb};
use gpui_kit::*;
use tern_core::ssh::{
    ConnectError, ConnectRequest, Credential, Event, HostKey, Session, SessionInput, Target,
    TermSize, default_credentials,
};

use crate::theme::{self, MONO, TermPalette, Theme};

const FONT_SIZE: f32 = 13.5;
/// Line height as a multiple of the font size.
const LINE_SPACING: f32 = 1.35;
const SCROLLBACK: usize = 10_000;

/// Where the connection stands.
pub enum Phase {
    Connecting,
    /// The server's key is unknown; waiting for the user to trust it.
    Trust {
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

pub struct TerminalView {
    target: Target,
    identity: Option<PathBuf>,
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
    pub fn new(target: Target, identity: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let size = TermSize {
            cols: 100,
            rows: 30,
        };
        let term_events = Rc::new(RefCell::new(Vec::new()));
        let config = TermConfig {
            scrolling_history: SCROLLBACK,
            ..Default::default()
        };
        let mut view = Self {
            target,
            identity,
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
        };
        view.connect(cx);
        view
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus.focus(window, cx);
    }

    /// Tab label: the remote title if the shell set one, else `user@host`.
    pub fn title(&self) -> SharedString {
        self.title
            .clone()
            .unwrap_or_else(|| format!("{}@{}", self.target.user, self.target.host).into())
    }

    fn connect(&mut self, cx: &mut Context<Self>) {
        let mut credentials = Vec::new();
        if let Some(path) = &self.identity {
            credentials.push(Credential::KeyFile {
                path: path.clone(),
                passphrase: None,
            });
        }
        match &self.password {
            Some(password) => credentials.push(Credential::Password(password.clone())),
            None => credentials.extend(default_credentials()),
        }
        let request = ConnectRequest {
            target: self.target.clone(),
            credentials,
            size: self.size,
            trust: self.trust.clone(),
        };
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
                self.term = Term::new(
                    TermConfig {
                        scrolling_history: SCROLLBACK,
                        ..Default::default()
                    },
                    &GridSize(self.size),
                    Listener(self.term_events.clone()),
                );
            }
            Event::Output(bytes) => {
                self.parser.advance(&mut self.term, &bytes);
                self.answer_terminal_events(cx);
            }
            Event::Closed { exit_status } => {
                self.input = None;
                self.phase = Phase::Ended(match exit_status {
                    Some(status) => format!("Exited with status {status}").into(),
                    None => "Connection closed".into(),
                });
            }
            Event::Failed(error) => {
                self.input = None;
                self.phase = match error {
                    ConnectError::UnknownHostKey { key, fingerprint } => Phase::Trust { key: *key, fingerprint: fingerprint.into() },
                    ConnectError::AuthFailed { .. } => Phase::Password { typed: String::new(), rejected: self.password.is_some() },
                    ConnectError::HostKeyChanged { fingerprint, file, line } => Phase::Ended(
                        // The one place the extra words earn their keep: this
                        // may be an attack, and the user needs the line to fix it.
                        format!(
                            "Host key changed to {fingerprint}, possible interception. Known key: {}:{line}",
                            file.display()
                        )
                        .into(),
                    ),
                    ConnectError::Other(message) => Phase::Ended(message.into()),
                };
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
                TermEvent::ClipboardStore(_, text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text))
                }
                _ => {}
            }
        }
    }

    fn send(&self, bytes: Vec<u8>) {
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
        match &mut self.phase {
            Phase::Live => {
                if is_paste(keystroke) {
                    self.paste(cx);
                } else if let Some(bytes) = encode_key(keystroke, *self.term.mode()) {
                    self.term.scroll_display(Scroll::Bottom);
                    self.send(bytes);
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
                "escape" => self.phase = Phase::Ended("Cancelled".into()),
                _ => return,
            },
            Phase::Trust { .. } => match keystroke.key.as_str() {
                "enter" => self.trust_and_connect(cx),
                "escape" => self.phase = Phase::Ended("Not trusted".into()),
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
            Phase::Live => {
                self.term.scroll_display(Scroll::Bottom);
                self.send(text.as_bytes().to_vec());
            }
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
        self.term.scroll_display(Scroll::Bottom);
        self.send(bytes.into_bytes());
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let line_height = px(FONT_SIZE * LINE_SPACING);
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
            self.target.user, self.target.host, self.target.port
        );

        Some(match &self.phase {
            Phase::Live => return None,
            Phase::Connecting => card(target.into(), None),
            Phase::Trust { fingerprint, .. } => card(
                format!("Trust {target}?").into(),
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
                                        this.phase = Phase::Ended("Not trusted".into());
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
        let frame = Rc::new(self.frame(&palette, focused));
        let entity = cx.entity();
        let this = entity.downgrade();
        let focus = self.focus.clone();
        let current = self.size;

        div()
            .id("terminal")
            .track_focus(&self.focus)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::key_down))
            .on_scroll_wheel(cx.listener(Self::scroll))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.focus(window, cx)),
            )
            .relative()
            .size_full()
            .p(px(14.))
            .rounded(px(18.))
            .bg(palette.background)
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let metrics = Metrics::measure(window);
                        let size = metrics.grid_size(bounds.size);
                        if size != current {
                            cx.defer(move |cx| {
                                let _ = this.update(cx, |view, cx| view.resize(size, cx));
                            });
                        }
                        metrics
                    },
                    move |bounds, metrics, window, cx| {
                        window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx);
                        paint_frame(&frame, &metrics, bounds.origin, window, cx);
                    },
                )
                .size_full(),
            )
            .children(self.overlay(&theme, cx))
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
    fn measure(window: &mut Window) -> Self {
        let font = font(MONO);
        let font_size = px(FONT_SIZE);
        let text = window.text_system();
        let cell_width = text
            .advance(text.resolve_font(&font), font_size, 'm')
            .map(|size| size.width)
            .unwrap_or(px(FONT_SIZE * 0.6));
        Self {
            font,
            font_size,
            cell_width,
            line_height: px((FONT_SIZE * LINE_SPACING).round()),
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

fn is_paste(keystroke: &Keystroke) -> bool {
    let m = &keystroke.modifiers;
    keystroke.key == "v" && ((m.control && m.shift) || m.platform)
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
    use gpui_kit::{Keystroke, Modifiers};

    use super::encode_key;

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
