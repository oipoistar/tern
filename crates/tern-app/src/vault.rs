//! The hosts vault: Tern's home screen. It asks the workspace to open a
//! session with [`VaultEvent::Connect`], opens the host editor over itself
//! to add or change hosts, and saves every change to the vault file. While
//! it shows in the active window it checks in the background that the hosts
//! answer, and their traces draw what the checks measured.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::randomart::Randomart;
use tern_core::reach::{Monitor, Place};
use tern_core::ssh::{Connection, Target, key_art, known_host_key, probe, quick_destination};
use tern_core::{Auth, Host, HostGroup, HostStatus, Vault, paths, putty};

use crate::bench;
use crate::host_editor::{EditorEvent, HostEditor};
use crate::live::LiveSlots;
use crate::marks::{self, Trace};
use crate::theme::{self, MONO, Theme};
use crate::ui::{Ui, art_tile, button, icon};

/// Where `tern-devserver` keeps the key it accepts.
const DEV_KEY: &str = "dev/test-ssh/id_ed25519";

/// Wide enough for `user@host` and two tags; three fit beside the sidebar
/// in the default window.
const CARD_WIDTH: f32 = 320.;

pub enum VaultEvent {
    Connect(Connection),
}

impl EventEmitter<VaultEvent> for VaultContent {}

/// Everything on the vault screen except the header.
pub struct VaultContent {
    vault: Vault,
    /// Where changes are saved; `None` keeps them in memory (tests, or a
    /// file this Tern mustn't overwrite).
    file: Option<PathBuf>,
    /// One line about the vault file: why it couldn't be read or saved.
    notice: Option<SharedString>,
    selected: String,
    live: LiveSlots,
    /// Art for each host whose key a known_hosts file already trusts, by
    /// (address, port).
    keys: HashMap<(String, u16), Randomart>,
    search: Entity<InputState>,
    query: SharedString,
    /// Set when the search text is itself an SSH destination.
    destination: Option<(Target, Option<PathBuf>)>,
    editor: Option<Entity<HostEditor>>,
    /// Saved PuTTY sessions not in the vault yet; the import button shows
    /// only while there are some.
    putty: Vec<Host>,
    /// What the background checks found.
    monitor: Monitor,
    /// The vault is the tab on screen; checks run only then.
    showing: bool,
    _checks: Option<Task<()>>,
    _subscription: Subscription,
    _editor_events: Option<Subscription>,
}

impl VaultContent {
    /// The vault saved in Tern's data folder, checked in the background. A
    /// first run in a dev checkout starts with the dev server's hosts. While
    /// benchmarking: the same hosts every run, with made-up checks.
    pub fn open(live: LiveSlots, window: &mut Window, cx: &mut Context<Self>) -> Self {
        if bench::enabled() {
            let mut content = Self::new(bench::vault(), None, live, window, cx);
            bench::fill(&mut content.monitor, &content.vault);
            return content;
        }
        let opened = tern_core::vault::open(&paths::vault());
        let mut vault = opened.vault;
        if opened.fresh && Path::new(DEV_KEY).exists() {
            vault.groups.insert(0, dev_server_group());
        }
        let mut content = Self::new(vault, opened.path, live, window, cx);
        content.notice = opened.notice.map(Into::into);
        content.start_checks(window, cx);
        content
    }

    /// Whether the vault is the tab on screen.
    pub fn set_showing(&mut self, showing: bool) {
        self.showing = showing;
    }

    /// Looks every second for checks that are due, and runs them while the
    /// vault shows in the active window.
    fn start_checks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._checks = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let alive = this.update_in(cx, |this, window, cx| {
                    if this.showing && window.is_window_active() {
                        this.run_due_checks(cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        }));
    }

    fn run_due_checks(&mut self, cx: &mut Context<Self>) {
        let mut places: Vec<Place> = Vec::new();
        for host in self.vault.hosts() {
            let place = self.vault.first_hop(host);
            if !places.contains(&place) {
                places.push(place);
            }
        }
        let due = self.monitor.due(&places, Instant::now());
        if due.is_empty() {
            return;
        }
        for place in due {
            let check = probe(&place.0, place.1);
            cx.spawn(async move |this, cx| {
                let latency_ms = check.await.ok().map(|reached| reached.latency_ms);
                let _ = this.update(cx, |this, cx| {
                    this.monitor.record(&place, latency_ms, Instant::now());
                    cx.notify();
                });
            })
            .detach();
        }
        // A first check shows as connecting.
        cx.notify();
    }

    /// The trace for `host`, from the checks of its first hop.
    fn trace(&self, host: &Host) -> Trace {
        let place = self.vault.first_hop(host);
        let (samples, sampled_at) = self.monitor.samples(&place);
        Trace {
            status: self.monitor.status(&place),
            samples: samples.into(),
            sampled_at,
        }
    }

    /// Shows `vault`, saving changes to `file` when there is one.
    pub fn new(
        vault: Vault,
        file: Option<PathBuf>,
        live: LiveSlots,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let subscription = cx.subscribe_in(
            &search,
            window,
            |this, search, event: &InputEvent, window, cx| match event {
                InputEvent::Change => {
                    this.query = search.read(cx).value();
                    this.destination = quick_destination(&this.query);
                    cx.notify();
                }
                // A typed destination wins; otherwise open the first match.
                InputEvent::PressEnter { .. } => {
                    let first = this.matching_hosts().next().map(|host| host.name.clone());
                    if let Some((target, identity)) = this.destination.clone() {
                        this.connect(Connection::typed(target, identity), window, cx);
                    } else if let Some(name) = first {
                        this.connect_saved(&name, window, cx);
                    }
                }
                _ => {}
            },
        );

        let selected = vault
            .hosts()
            .next()
            .map(|host| host.name.clone())
            .unwrap_or_default();
        let mut content = Self {
            vault,
            file,
            notice: None,
            selected,
            live,
            keys: HashMap::new(),
            search,
            query: SharedString::default(),
            destination: None,
            editor: None,
            putty: Vec::new(),
            monitor: Monitor::default(),
            showing: true,
            _checks: None,
            _subscription: subscription,
            _editor_events: None,
        };
        content.refresh_keys();
        content.putty = putty::importable_here(&content.vault);
        content
    }

    /// Adds every PuTTY session not in the vault yet to the PuTTY group and
    /// selects the first.
    fn import_putty(&mut self, cx: &mut Context<Self>) {
        let hosts = std::mem::take(&mut self.putty);
        if let Some(first) = hosts.first() {
            self.selected = first.name.clone();
        }
        for host in hosts {
            self.vault.save(None, host, putty::GROUP);
        }
        self.persist();
        self.refresh_keys();
        cx.notify();
    }

    /// Writes the vault to its file, if it has one. A failure stays on
    /// screen until a later save works.
    fn persist(&mut self) {
        let Some(path) = &self.file else {
            return;
        };
        self.notice = self
            .vault
            .save_to(path)
            .err()
            .map(|error| format!("Can't save {}: {error}", path.display()).into());
    }

    #[cfg(test)]
    fn offer_putty(&mut self, hosts: Vec<Host>) {
        self.putty = hosts;
    }

    /// Re-reads which hosts have a trusted key (known_hosts files only, read
    /// never written), so a key trusted in a session shows up here.
    pub fn refresh_keys(&mut self) {
        self.keys = self
            .vault
            .hosts()
            .filter_map(|host| {
                let key = known_host_key(&host.address, host.port)?;
                Some(((host.address.clone(), host.port), key_art(&key)))
            })
            .collect();
    }

    /// Hosts the search matches, in vault order; none while search is empty,
    /// so Enter on an empty field does nothing.
    fn matching_hosts(&self) -> impl Iterator<Item = &Host> {
        let searching = !self.query.trim().is_empty();
        self.vault
            .hosts()
            .filter(move |host| searching && host.matches(&self.query))
    }

    fn connect(&mut self, connection: Connection, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |state, cx| state.set_value("", window, cx));
        cx.emit(VaultEvent::Connect(connection));
    }

    /// Connects to a saved host. If its settings can't work (its jump host
    /// is gone), opens the editor on them instead.
    fn connect_saved(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host) = self.vault.find(name) else {
            return;
        };
        match self.vault.connection(host) {
            Ok(connection) => self.connect(connection, window, cx),
            Err(_) => {
                self.open_editor(Some(name.to_string()), window, cx);
                if let Some(editor) = &self.editor {
                    editor.update(cx, |editor, cx| editor.show_problems(cx));
                }
            }
        }
    }

    /// Opens the editor on the host called `editing`, or on a new host in
    /// the selected host's group.
    pub fn open_editor(
        &mut self,
        editing: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let group = self
            .vault
            .group_of(&self.selected)
            .unwrap_or("Hosts")
            .to_string();
        let art = editing
            .as_deref()
            .and_then(|name| self.vault.find(name))
            .and_then(|host| self.keys.get(&(host.address.clone(), host.port)))
            .cloned();
        let (vault, live) = (self.vault.clone(), self.live.clone());
        let editor =
            cx.new(|cx| HostEditor::new(vault, editing.as_deref(), &group, art, live, window, cx));
        self._editor_events = Some(cx.subscribe_in(&editor, window, Self::on_editor_event));
        self.editor = Some(editor);
        cx.notify();
    }

    fn on_editor_event(
        &mut self,
        _: &Entity<HostEditor>,
        event: &EditorEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            EditorEvent::Save {
                replacing,
                host,
                group,
                checked,
            } => {
                self.vault
                    .save(replacing.as_deref(), (**host).clone(), group);
                self.persist();
                if let Some((place, latency_ms)) = checked {
                    self.monitor
                        .record(place, Some(*latency_ms), Instant::now());
                }
                self.selected = host.name.clone();
                self.refresh_keys();
            }
            EditorEvent::Delete(name) => {
                if self.vault.delete(name).is_ok() {
                    self.persist();
                    if self.selected == *name {
                        self.selected = self
                            .vault
                            .hosts()
                            .next()
                            .map(|host| host.name.clone())
                            .unwrap_or_default();
                    }
                }
            }
            EditorEvent::Close => {}
        }
        // A deleted import can come back; a host edited to match a session
        // makes it redundant.
        if !matches!(event, EditorEvent::Close) {
            self.putty = putty::importable_here(&self.vault);
        }
        self.editor = None;
        self._editor_events = None;
        cx.notify();
    }

    fn hosts(&self, ui: &Ui, cx: &Context<Self>) -> impl IntoElement {
        let theme = ui.theme;
        div()
            .id("hosts")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(22.))
            .child(self.toolbar(theme, cx))
            .when_some(self.notice.clone(), |d, notice| {
                d.child(
                    div()
                        .px(px(4.))
                        .text_size(px(13.))
                        .text_color(theme.danger)
                        .truncate()
                        .child(notice),
                )
            })
            .when_some(self.destination.clone(), |d, (target, identity)| {
                d.child(destination_card(target, identity, theme, cx))
            })
            .children(self.vault.groups.iter().filter_map(|group| {
                let hosts: Vec<&Host> = group
                    .hosts
                    .iter()
                    .filter(|host| host.matches(&self.query))
                    .collect();
                if hosts.is_empty() {
                    return None;
                }
                Some(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(14.))
                        .child(
                            div()
                                .px(px(4.))
                                .flex()
                                .items_center()
                                .gap(px(14.))
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(group.name.clone()),
                                )
                                .child(div().flex_1().h(px(1.)).bg(theme.line)),
                        )
                        .child(
                            div().flex().flex_wrap().gap(px(14.)).children(
                                hosts.into_iter().map(|host| self.host_card(host, ui, cx)),
                            ),
                        ),
                )
            }))
    }

    fn toolbar(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(12.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(46.))
                    .pl(px(10.))
                    .pr(px(4.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .rounded(px(16.))
                    .bg(theme.card2)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.search).id("search").appearance(false)),
                    )
                    .child(
                        div()
                            .size(px(38.))
                            .flex_none()
                            .rounded_full()
                            .bg(theme.card)
                            .shadow(theme.shadow_sm())
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon(IconName::Search, 17., theme.text2)),
                    ),
            )
            // Offered only while PuTTY has sessions the vault lacks.
            .when(!self.putty.is_empty(), |d| {
                d.child(
                    button("import-putty", theme)
                        .debug_selector(|| "import-putty".into())
                        .h(px(46.))
                        .px(px(16.))
                        .rounded(px(16.))
                        .text_size(px(14.))
                        .child(icon(IconName::Import, 16., theme.text))
                        .child("Import from PuTTY")
                        .on_click(cx.listener(|this, _, _, cx| this.import_putty(cx))),
                )
            })
            // The screen's primary action, so the one solid ink button.
            .child(
                div()
                    .id("new-host")
                    .debug_selector(|| "new-host".into())
                    .h(px(46.))
                    .px(px(20.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(16.))
                    .bg(theme.text)
                    .text_color(theme.card)
                    .shadow(theme.shadow_sm())
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .child(icon(IconName::Plus, 16., theme.card))
                    .child("New host")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_editor(None, window, cx)),
                    ),
            )
    }

    fn host_card(&self, host: &Host, ui: &Ui, cx: &Context<Self>) -> impl IntoElement {
        let selected = host.name == self.selected;
        // The selected card is drawn inverted: an ink surface with paper text.
        let t = if selected {
            Rc::new(ui.theme.inverse())
        } else {
            ui.theme.clone()
        };
        let art = self.keys.get(&(host.address.clone(), host.port)).cloned();
        let trace = self.trace(host);
        let latency = match trace.status {
            HostStatus::Online {
                latency_ms: Some(ms),
            } => format!("{ms} ms"),
            _ => String::new(),
        };
        let card_id = format!("host-{}", host.name);
        let (pick, edit, connect) = (host.name.clone(), host.name.clone(), host.name.clone());

        div()
            .id(SharedString::from(card_id.clone()))
            .debug_selector(move || card_id)
            .w(px(CARD_WIDTH))
            .pt(px(14.))
            .pb(px(12.))
            .pl(px(14.))
            .pr(px(16.))
            .rounded(px(22.))
            .bg(t.card)
            .text_color(t.text)
            .shadow(ui.theme.shadow())
            .flex()
            .flex_col()
            .gap(px(12.))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = pick.clone();
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .child(art_tile(art, 52., 15., &t))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .line_height(px(19.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(host.name.clone()),
                            )
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_size(px(12.))
                                    .line_height(px(16.))
                                    .text_color(t.text2)
                                    .truncate()
                                    .child(host.target()),
                            )
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_size(px(11.5))
                                    .line_height(px(15.))
                                    .text_color(t.text3)
                                    .truncate()
                                    .child(host.tags.join(" / ")),
                            ),
                    )
                    // Settings for the selected host only, so each card
                    // carries one action.
                    .when(selected, |d| {
                        d.child(
                            round_button("edit-host".into(), IconName::Pencil, false, &t)
                                .debug_selector(|| "edit-host".into())
                                .self_start()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.open_editor(Some(edit.clone()), window, cx);
                                })),
                        )
                    })
                    .child({
                        let id = format!("connect-{}", host.name);
                        round_button(id.clone(), IconName::ArrowRight, selected, &t)
                            .debug_selector(move || id)
                            .self_start()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.connect_saved(&connect, window, cx);
                            }))
                    }),
            )
            .child(
                div()
                    .h(px(18.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(if self.editor.is_some() {
                        // Under the editor the live layer would paint over
                        // the dialog, so the trace waits.
                        div().flex_1().into_any_element()
                    } else {
                        ui.trace(
                            format!("trace-{}", host.name),
                            trace,
                            marks::seed(&host.name),
                            t.text,
                            t.text3,
                        )
                        .flex_1()
                        .h_full()
                        .into_any_element()
                    })
                    .child(
                        div()
                            .min_w(px(36.))
                            .flex()
                            .justify_end()
                            .font_family(MONO)
                            .text_size(px(12.))
                            .child(latency),
                    ),
            )
    }
}

impl Render for VaultContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Placeholders re-record their marks during this frame's prepaint.
        self.live.clear();
        let theme = theme::current(cx);
        let ui = Ui {
            theme: &theme,
            live: &self.live,
        };
        div()
            .size_full()
            .relative()
            .flex()
            .gap(px(20.))
            .p(px(20.))
            .bg(theme.win)
            .text_color(theme.text)
            .text_size(px(14.))
            .child(sidebar(&theme))
            .child(self.hosts(&ui, cx))
            .when_some(self.editor.clone(), |d, editor| {
                d.child(
                    div()
                        .id("editor-scrim")
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
                        .child(editor),
                )
            })
    }
}

/// The typed destination, shaped like a host card so it reads as one; its
/// arrow is ink because Enter goes there.
fn destination_card(
    target: Target,
    identity: Option<PathBuf>,
    theme: &Theme,
    cx: &Context<VaultContent>,
) -> impl IntoElement {
    let label = format!("{}@{}:{}", target.user, target.host, target.port);
    div()
        .id("destination")
        .w(px(CARD_WIDTH))
        .p(px(14.))
        .rounded(px(22.))
        .bg(theme.card)
        .shadow(theme.shadow())
        .flex()
        .items_center()
        .gap(px(14.))
        .cursor_pointer()
        .on_click(cx.listener(move |this, _, window, cx| {
            this.connect(
                Connection::typed(target.clone(), identity.clone()),
                window,
                cx,
            );
        }))
        .child(
            div()
                .size(px(52.))
                .flex_none()
                .rounded(px(15.))
                .border_1()
                .border_color(theme.line)
                .flex()
                .items_center()
                .justify_center()
                .child(icon(IconName::SquareTerminal, 20., theme.text)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .font_family(MONO)
                .text_size(px(13.))
                .truncate()
                .child(label),
        )
        .child(round_button(
            "destination-connect".into(),
            IconName::ArrowRight,
            true,
            theme,
        ))
}

/// A round icon button; `filled` makes it solid ink for the primary choice.
fn round_button(id: String, glyph: IconName, filled: bool, theme: &Theme) -> Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .size(px(36.))
        .flex_none()
        .rounded_full()
        .border_1()
        .border_color(theme.line)
        .when(filled, |d| d.bg(theme.text))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .child(icon(
            glyph,
            16.,
            if filled { theme.card } else { theme.text },
        ))
}

/// The local dev server from `tern-devserver`, shown only when its key
/// exists: once directly, and once through itself as a jump host.
fn dev_server_group() -> HostGroup {
    let dev = Host {
        os: "This computer".into(),
        tags: vec!["dev".into()],
        auth: Auth::Key(PathBuf::from(DEV_KEY)),
        ..Host::new("tern-devserver", "tern", "127.0.0.1", 2222)
    };
    let through_itself = Host {
        name: "devserver-via-jump".into(),
        tags: vec!["dev".into(), "jump".into()],
        jump: Some(dev.name.clone()),
        ..dev.clone()
    };
    HostGroup {
        name: "Local dev server".into(),
        hosts: vec![dev, through_itself],
    }
}

fn sidebar(theme: &Theme) -> impl IntoElement {
    let items = [
        (IconName::Server, "Hosts"),
        (IconName::KeyRound, "Keychain"),
        (IconName::CodeXml, "Snippets"),
        (IconName::ArrowLeftRight, "Port forwarding"),
        (IconName::ShieldCheck, "Known hosts"),
        (IconName::Clock, "History"),
    ];
    div()
        .w(px(220.))
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(4.))
        .children(items.into_iter().enumerate().map(|(i, (name, label))| {
            let active = i == 0;
            let fg = if active { theme.text } else { theme.text2 };
            div()
                .h(px(42.))
                .px(px(14.))
                .flex()
                .items_center()
                .gap(px(12.))
                .rounded(px(14.))
                .when(active, |d| d.bg(theme.card).shadow(theme.shadow()))
                .text_color(fg)
                .font_weight(if active {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::MEDIUM
                })
                .child(icon(name, 18., fg))
                .child(label)
        }))
}

#[cfg(test)]
mod tests {
    // Explicit imports: `gpui_kit::*` would shadow Rust's `#[test]`.
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use gpui_kit::Entity;
    use tern_core::draft::Field;
    use tern_core::ssh::Connection;
    use tern_core::{Host, Vault};

    use super::{VaultContent, VaultEvent};
    use crate::live::LiveSlots;

    /// The sample vault, kept in memory only.
    fn open_vault(
        cx: &mut TestAppContext,
    ) -> (
        Entity<VaultContent>,
        &mut VisualTestContext,
        Rc<RefCell<Vec<Connection>>>,
    ) {
        cx.update(gpui_kit::init);
        let (vault, cx) = cx.add_window_view(|window, cx| {
            VaultContent::new(Vault::sample(), None, LiveSlots::default(), window, cx)
        });
        let connects: Rc<RefCell<Vec<Connection>>> = Rc::default();
        let recorded = connects.clone();
        cx.update(|_, cx| {
            cx.subscribe(&vault, move |_, event: &VaultEvent, _| {
                let VaultEvent::Connect(connection) = event;
                recorded.borrow_mut().push(connection.clone());
            })
            .detach()
        });
        (vault, cx, connects)
    }

    /// Clicks the element tagged with `selector`, like a user would.
    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing on screen is tagged {selector}"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn search_filters_hosts_and_connects_to_a_typed_destination(cx: &mut TestAppContext) {
        let (vault, cx, connects) = open_vault(cx);
        let focus_search = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                let search = vault.read(cx).search.clone();
                search.update(cx, |state, cx| state.focus(window, cx));
            });
            cx.run_until_parked();
        };

        // Words filter the hosts; nothing to connect to yet.
        focus_search(cx);
        cx.simulate_input("postgres");
        vault.read_with(cx, |vault, _| {
            let names: Vec<&str> = vault
                .matching_hosts()
                .map(|host| host.name.as_str())
                .collect();
            assert_eq!(names, ["prod-db-01"]);
            assert!(vault.destination.is_none());
        });

        // Enter opens the first match.
        cx.simulate_keystrokes("enter");
        assert_eq!(
            connects
                .borrow()
                .last()
                .map(|connection| connection.target.host.clone()),
            Some("10.0.4.21".into())
        );

        // A destination is offered as it is, and Enter connects to it.
        focus_search(cx);
        cx.simulate_input("deploy@10.0.4.21:2200");
        vault.read_with(cx, |vault, _| assert!(vault.destination.is_some()));
        cx.simulate_keystrokes("enter");
        let target = connects
            .borrow()
            .last()
            .cloned()
            .expect("a connect event")
            .target;
        assert_eq!((target.user.as_str(), target.port), ("deploy", 2200));
        vault.read_with(cx, |vault, cx| {
            assert!(vault.search.read(cx).value().is_empty())
        });
    }

    #[gpui::test]
    fn importing_from_putty_adds_its_sessions_once(cx: &mut TestAppContext) {
        let (vault, cx, _) = open_vault(cx);
        // Seeded here rather than read from this computer's PuTTY.
        let sessions = vec![
            Host::new("nas", "admin", "nas.lan", 22),
            Host::new("router", "root", "10.0.0.1", 2222),
        ];
        vault.update(cx, |vault, cx| {
            vault.offer_putty(sessions);
            cx.notify();
        });

        click(cx, "import-putty");
        vault.read_with(cx, |vault, _| {
            assert_eq!(vault.vault.group_of("nas"), Some("PuTTY"));
            assert_eq!(vault.vault.group_of("router"), Some("PuTTY"));
            assert_eq!(vault.selected, "nas");
        });
        assert!(
            cx.debug_bounds("import-putty").is_none(),
            "nothing is left to import"
        );
    }

    #[gpui::test]
    fn the_host_editor_adds_edits_and_deletes_hosts(cx: &mut TestAppContext) {
        let (vault, cx, connects) = open_vault(cx);
        // Replaces a field's text, the way a user selects it all and types.
        let type_into = |cx: &mut VisualTestContext, field: Field, text: &str| {
            cx.update(|window, cx| {
                let editor = vault.read(cx).editor.clone().expect("the editor is open");
                let input = editor.read(cx).input(field);
                input.update(cx, |state, cx| state.focus(window, cx));
            });
            cx.run_until_parked();
            cx.simulate_keystrokes(if cfg!(target_os = "macos") {
                "cmd-a"
            } else {
                "ctrl-a"
            });
            cx.simulate_input(text);
        };
        let editor_open =
            |cx: &mut VisualTestContext| vault.read_with(cx, |v, _| v.editor.is_some());

        // A new host: saving it empty points at what's missing.
        click(cx, "new-host");
        click(cx, "save-host");
        assert!(editor_open(cx));
        let missing = vault.read_with(cx, |vault, cx| {
            vault.editor.as_ref().unwrap().read(cx).problem_fields()
        });
        assert_eq!(missing, [Field::Name, Field::Address, Field::User]);

        type_into(cx, Field::Name, "db-new");
        type_into(cx, Field::Address, "10.0.0.9");
        type_into(cx, Field::Port, "2200");
        type_into(cx, Field::User, "root");
        click(cx, "save-host");
        assert!(!editor_open(cx));
        vault.read_with(cx, |vault, _| {
            let host = vault.vault.find("db-new").expect("the new host is saved");
            assert_eq!((host.port, host.user.as_str()), (2200, "root"));
            assert_eq!(vault.vault.group_of("db-new"), Some("Production"));
            assert_eq!(vault.selected, "db-new");
        });

        // Editing: route prod-db-01 through bastion, then connect.
        click(cx, "host-prod-db-01");
        click(cx, "edit-host");
        click(cx, "jump-switch");
        click(cx, "jump-picker");
        click(cx, "jump-picker-bastion");
        click(cx, "save-host");
        assert!(!editor_open(cx));
        click(cx, "connect-prod-db-01");
        let connection = connects.borrow().last().cloned().expect("a connect event");
        let jump = connection.jump.expect("connects through the jump host");
        assert_eq!(jump.target.host, "bastion.example.com");

        // Deleting a host others go through is refused; Escape closes.
        click(cx, "host-bastion");
        click(cx, "edit-host");
        click(cx, "delete-host");
        assert!(editor_open(cx));
        cx.simulate_keystrokes("escape");
        assert!(!editor_open(cx));
        vault.read_with(cx, |vault, _| {
            assert!(vault.vault.find("bastion").is_some())
        });

        // Deleting an ordinary host works.
        click(cx, "host-db-new");
        click(cx, "edit-host");
        click(cx, "delete-host");
        assert!(!editor_open(cx));
        vault.read_with(cx, |vault, _| assert!(vault.vault.find("db-new").is_none()));
    }

    #[gpui::test]
    fn checks_hosts_at_their_first_hop(cx: &mut TestAppContext) {
        use std::io::Write as _;
        use std::time::{Duration, Instant};

        use tern_core::HostStatus;

        // A local server that only greets, which is all a check needs.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let _ = stream.write_all(b"SSH-2.0-Test\r\n");
            }
        });
        let mut hosts = Vault::default();
        hosts.save(None, Host::new("gateway", "me", "127.0.0.1", port), "Lab");
        // Behind the gateway, so checked there; its own address is never
        // tried.
        let behind = Host {
            jump: Some("gateway".into()),
            ..Host::new("behind", "me", "192.0.2.1", 22)
        };
        hosts.save(None, behind, "Lab");
        // The check's answer arrives from the SSH runtime's own threads.
        cx.executor().allow_parking();

        cx.update(gpui_kit::init);
        let (vault, cx) = cx.add_window_view(|window, cx| {
            VaultContent::new(hosts, None, LiveSlots::default(), window, cx)
        });
        let status = |cx: &mut VisualTestContext| {
            vault.read_with(cx, |vault, _| {
                vault.trace(vault.vault.find("behind").unwrap()).status
            })
        };
        assert_eq!(status(cx), HostStatus::Unknown);

        vault.update(cx, |vault, cx| vault.run_due_checks(cx));
        assert_eq!(status(cx), HostStatus::Connecting);
        let deadline = Instant::now() + Duration::from_secs(10);
        while status(cx) == HostStatus::Connecting && Instant::now() < deadline {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        let reached = status(cx);
        assert!(
            matches!(
                reached,
                HostStatus::Online {
                    latency_ms: Some(_)
                }
            ),
            "{reached:?}"
        );
        vault.read_with(cx, |vault, _| {
            let gateway = vault.vault.find("gateway").unwrap();
            assert_eq!(vault.trace(gateway).samples.len(), 1);
        });
    }

    #[gpui::test]
    fn an_empty_vault_saves_what_is_added(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("tern-ui-vault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("vault.json");
        cx.update(gpui_kit::init);
        let file = Some(path.clone());
        let (vault, cx) = cx.add_window_view(|window, cx| {
            VaultContent::new(Vault::default(), file, LiveSlots::default(), window, cx)
        });
        let saved = || tern_core::vault::open(&path).vault;

        // Just the toolbar, and nothing written until something changes.
        cx.run_until_parked();
        assert!(cx.debug_bounds("new-host").is_some());
        assert!(!path.exists());

        vault.update(cx, |vault, cx| {
            vault.offer_putty(vec![Host::new("nas", "admin", "nas.lan", 22)]);
            cx.notify();
        });
        click(cx, "import-putty");
        assert_eq!(saved().group_of("nas"), Some("PuTTY"));

        click(cx, "host-nas");
        click(cx, "edit-host");
        click(cx, "delete-host");
        assert!(saved().hosts().next().is_none());
        vault.read_with(cx, |vault, _| assert!(vault.notice.is_none()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
