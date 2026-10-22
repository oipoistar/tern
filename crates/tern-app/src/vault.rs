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
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::forward::{Forward, ForwardState};
use tern_core::history::{History, Visit};
use tern_core::randomart::Randomart;
use tern_core::reach::{Monitor, Place};
use tern_core::snippet::Snippet;
use tern_core::ssh::{Connection, Target, key_art, known_host_key, probe, quick_destination};
use tern_core::{Auth, Host, HostGroup, HostStatus, Vault, paths, putty};
use tern_core::{keys, known_hosts};

use crate::bench;
use crate::forwarding::{ForwardEvent, ForwardsView, States};
use crate::history::{HistoryEvent, HistoryView};
use crate::host_editor::{EditorEvent, HostEditor};
use crate::keychain::{KeychainEvent, KeychainView};
use crate::known_hosts::KnownHostsView;
use crate::live::LiveSlots;
use crate::marks::{self, Trace};
use crate::snippets::{SnippetEvent, SnippetsView};
use crate::terminal::Preview;
use crate::theme::{self, MONO, TermPalette, Theme};
use crate::ui::{
    Ui, art_tile, button, group_heading, icon, ink_button, search_field, status_dot, switch,
};

/// Where `tern-devserver` keeps the key it accepts.
const DEV_KEY: &str = "dev/test-ssh/id_ed25519";

/// Wide enough for `user@host` and two tags; three fit beside the sidebar
/// in the default window.
const CARD_WIDTH: f32 = 320.;

pub enum VaultEvent {
    Connect(Connection),
    /// Show the session for this connection's saved host, opening one if
    /// none is open, with its Files panel when `files`.
    Open {
        connection: Connection,
        files: bool,
    },
    /// Start or stop a rule in any open session to the saved host `host`.
    Forward {
        host: String,
        forward: Forward,
        on: bool,
    },
}

/// The screens the sidebar switches between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Hosts,
    Keychain,
    Snippets,
    Forwarding,
    KnownHosts,
    History,
}

impl Section {
    /// Sidebar entries in order, with their glyph, label and the section
    /// they open (`None` for screens not built yet).
    const SIDEBAR: [(IconName, &'static str, Option<Section>); 6] = [
        (IconName::Server, "Hosts", Some(Section::Hosts)),
        (IconName::KeyRound, "Keychain", Some(Section::Keychain)),
        (IconName::CodeXml, "Snippets", Some(Section::Snippets)),
        (
            IconName::ArrowLeftRight,
            "Port forwarding",
            Some(Section::Forwarding),
        ),
        (
            IconName::ShieldCheck,
            "Known hosts",
            Some(Section::KnownHosts),
        ),
        (IconName::Clock, "History", Some(Section::History)),
    ];
}

impl EventEmitter<VaultEvent> for VaultContent {}

/// Everything on the vault tab except the header: the sidebar and the
/// screen it picked.
pub struct VaultContent {
    section: Section,
    /// Screens, made when first shown.
    keychain: Option<Entity<KeychainView>>,
    snippets: Option<Entity<SnippetsView>>,
    forwards: Option<Entity<ForwardsView>>,
    known_hosts: Option<Entity<KnownHostsView>>,
    history_view: Option<Entity<HistoryView>>,
    /// Recent connections, and where they're saved (`None` keeps them in
    /// memory).
    history: History,
    history_file: Option<PathBuf>,
    /// How each rule is doing in open sessions.
    forward_states: States,
    /// The open sessions' previews, by saved host.
    open_sessions: HashMap<String, Preview>,
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
    _screen_events: Vec<Subscription>,
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
        content.history = History::load(&paths::history());
        content.history_file = Some(paths::history());
        content.start_checks(window, cx);
        content
    }

    /// Whether the vault is the tab on screen.
    pub fn set_showing(&mut self, showing: bool) {
        self.showing = showing;
    }

    /// Switches the screen beside the sidebar. Screens that show files
    /// read them again, for changes made meanwhile.
    fn show(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        self.section = section;
        match section {
            Section::Hosts => self.refresh_keys(),
            Section::Keychain => match &self.keychain {
                Some(view) => {
                    let vault = self.vault.clone();
                    view.update(cx, |view, cx| view.refresh(vault, cx));
                }
                None => {
                    let (dirs, vault) = (keys::dirs(), self.vault.clone());
                    let view = cx.new(|cx| KeychainView::new(dirs, vault, window, cx));
                    self._screen_events.push(cx.subscribe_in(
                        &view,
                        window,
                        |this, _, event: &KeychainEvent, window, cx| match event {
                            KeychainEvent::Install { host, public } => {
                                this.install_key(host, public, window, cx)
                            }
                        },
                    ));
                    self.keychain = Some(view);
                }
            },
            Section::Snippets => {
                if self.snippets.is_none() {
                    let saved = self.vault.snippets.clone();
                    let view = cx.new(|cx| SnippetsView::new(saved, window, cx));
                    self._screen_events.push(cx.subscribe_in(
                        &view,
                        window,
                        Self::on_snippet_event,
                    ));
                    self.snippets = Some(view);
                }
            }
            Section::Forwarding => match &self.forwards {
                Some(_) => self.refresh_forwards(cx),
                None => {
                    let (hosts, states) = (self.forward_hosts(), self.forward_states.clone());
                    let view = cx.new(|_| ForwardsView::new(hosts, states));
                    self._screen_events.push(cx.subscribe_in(
                        &view,
                        window,
                        Self::on_forward_event,
                    ));
                    self.forwards = Some(view);
                }
            },
            Section::History => match &self.history_view {
                Some(view) => {
                    let visits = self.history.visits.clone();
                    view.update(cx, |view, cx| view.refresh(visits, cx));
                }
                None => {
                    let view = cx.new(|_| HistoryView::new(self.history.visits.clone()));
                    self._screen_events.push(cx.subscribe_in(
                        &view,
                        window,
                        |this, _, event: &HistoryEvent, window, cx| match event {
                            HistoryEvent::Reconnect(index) => {
                                if let Some(visit) = this.history.visits.get(*index) {
                                    let connection = visit.connection(&this.vault);
                                    this.connect(connection, window, cx);
                                }
                            }
                        },
                    ));
                    self.history_view = Some(view);
                }
            },
            Section::KnownHosts => match &self.known_hosts {
                Some(view) => view.update(cx, |view, cx| view.refresh(cx)),
                None => {
                    let files = known_hosts::files();
                    self.known_hosts = Some(cx.new(|cx| KnownHostsView::new(files, window, cx)));
                }
            },
        }
        cx.notify();
    }

    /// Looks every second for checks that are due, and runs them while the
    /// vault shows in the active window.
    fn start_checks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._checks = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let alive = this.update_in(cx, |this, window, cx| {
                    let on_screen = this.showing && this.section == Section::Hosts;
                    if on_screen && window.is_window_active() {
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

    /// The trace of the saved host called `name`, for a session's toolbar.
    pub fn host_trace(&self, name: &str) -> Option<Trace> {
        self.vault.find(name).map(|host| self.trace(host))
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
            section: Section::Hosts,
            keychain: None,
            snippets: None,
            forwards: None,
            known_hosts: None,
            history_view: None,
            history: History::default(),
            history_file: None,
            forward_states: States::default(),
            open_sessions: HashMap::new(),
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
            _screen_events: Vec::new(),
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

    /// Every saved host's name and port forwarding rules.
    fn forward_hosts(&self) -> Vec<(String, Vec<Forward>)> {
        self.vault
            .hosts()
            .map(|host| (host.name.clone(), host.forwards.clone()))
            .collect()
    }

    fn refresh_forwards(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = &self.forwards {
            let (hosts, states) = (self.forward_hosts(), self.forward_states.clone());
            view.update(cx, |view, cx| view.refresh(hosts, states, cx));
        }
    }

    /// How a rule is doing in a session to `host`.
    pub fn set_forward_state(
        &mut self,
        host: String,
        forward: &Forward,
        state: ForwardState,
        cx: &mut Context<Self>,
    ) {
        let key = (host, forward.id());
        match state {
            ForwardState::Stopped => self.forward_states.remove(&key),
            state => self.forward_states.insert(key, state),
        };
        self.refresh_forwards(cx);
    }

    /// The session to `host` ended, and its rules with it.
    pub fn clear_forward_states(&mut self, host: &str, cx: &mut Context<Self>) {
        self.forward_states.retain(|(of, _), _| of != host);
        self.refresh_forwards(cx);
    }

    /// Which saved hosts have a session open, with its preview.
    pub fn set_open_sessions(
        &mut self,
        sessions: HashMap<String, Preview>,
        cx: &mut Context<Self>,
    ) {
        self.open_sessions = sessions;
        cx.notify();
    }

    /// Switches a rule from the side panel, as the Port forwarding screen
    /// does.
    fn switch_forward(&mut self, host: &str, forward: &Forward, on: bool, cx: &mut Context<Self>) {
        self.vault.switch_forward(host, &forward.id(), on);
        self.persist();
        self.refresh_forwards(cx);
        cx.emit(VaultEvent::Forward {
            host: host.to_string(),
            forward: forward.clone(),
            on,
        });
        cx.notify();
    }

    /// Asks for the selected host's session, opening one if needed.
    fn open_selected(&mut self, files: bool, cx: &mut Context<Self>) {
        let Some(host) = self.vault.find(&self.selected) else {
            return;
        };
        if let Ok(connection) = self.vault.connection(host) {
            cx.emit(VaultEvent::Open { connection, files });
        }
    }

    /// Beside the cards, for the selected host: its open session's last
    /// lines, Open terminal and SFTP, and its port forwarding switches.
    fn side_panel(&self, host: &Host, ui: &Ui, cx: &Context<Self>) -> impl IntoElement {
        let t = ui.theme;
        let palette = TermPalette::for_theme(t);
        let art = self.keys.get(&(host.address.clone(), host.port)).cloned();
        let preview = self.open_sessions.get(&host.name).cloned();
        div()
            .id("host-panel")
            .debug_selector(|| "host-panel".into())
            .w(px(300.))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                div()
                    .p(px(16.))
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .rounded(px(22.))
                    .bg(t.card)
                    .shadow(t.shadow())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .child(art_tile(art, 46., 14., t))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.))
                                    .child(
                                        div()
                                            .text_size(px(16.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .truncate()
                                            .child(host.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .font_family(MONO)
                                            .text_size(px(12.))
                                            .text_color(t.text2)
                                            .truncate()
                                            .child(host.target()),
                                    ),
                            ),
                    )
                    .when_some(preview, |d, lines| {
                        d.child(
                            div()
                                .h(px(150.))
                                .p(px(10.))
                                .rounded(px(14.))
                                .bg(palette.background)
                                .child(
                                    div()
                                        .id("host-preview")
                                        .debug_selector(|| "host-preview".into())
                                        .size_full()
                                        .child(
                                            ui.preview(
                                                format!("preview-{}", host.name),
                                                lines,
                                                palette.foreground,
                                            )
                                            .size_full(),
                                        ),
                                ),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(
                                ink_button("side-terminal", t)
                                    .debug_selector(|| "side-terminal".into())
                                    .flex_1()
                                    .justify_center()
                                    .h(px(40.))
                                    .rounded(px(13.))
                                    .child(icon(IconName::SquareTerminal, 15., t.card))
                                    .child("Open terminal")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.open_selected(false, cx)),
                                    ),
                            )
                            .child(
                                button("side-sftp", t)
                                    .debug_selector(|| "side-sftp".into())
                                    .h(px(40.))
                                    .rounded(px(13.))
                                    .child(icon(IconName::FolderOpen, 15., t.text))
                                    .child("SFTP")
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.open_selected(true, cx)),
                                    ),
                            ),
                    ),
            )
            .when(!host.forwards.is_empty(), |d| {
                d.child(
                    div()
                        .p(px(8.))
                        .flex()
                        .flex_col()
                        .rounded(px(22.))
                        .bg(t.card)
                        .shadow(t.shadow())
                        .children(host.forwards.iter().enumerate().map(|(index, forward)| {
                            let running = matches!(
                                self.forward_states.get(&(host.name.clone(), forward.id())),
                                Some(ForwardState::Running)
                            );
                            let id = format!("side-switch-{index}");
                            let (name, toggled) = (host.name.clone(), forward.clone());
                            div()
                                .h(px(44.))
                                .px(px(8.))
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .child(
                                    div()
                                        .w(px(22.))
                                        .flex_none()
                                        .font_family(MONO)
                                        .text_size(px(12.))
                                        .text_color(t.text3)
                                        .child(forward.flag()),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .font_family(MONO)
                                        .text_size(px(12.))
                                        .truncate()
                                        .child(forward.route()),
                                )
                                .when(running, |d| d.child(status_dot(t)))
                                .child(
                                    switch(SharedString::from(id.clone()), forward.on, t)
                                        .debug_selector(move || id)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.switch_forward(&name, &toggled, !toggled.on, cx)
                                        })),
                                )
                        })),
                )
            })
    }

    /// Applies a change from the Port forwarding screen, saves it, and
    /// starts or stops the rules involved in open sessions.
    fn on_forward_event(
        &mut self,
        view: &Entity<ForwardsView>,
        event: &ForwardEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut live = Vec::new();
        match event {
            ForwardEvent::Save {
                host,
                replacing,
                forward,
            } => {
                let old = replacing.as_ref().map(Forward::id);
                if let Err(problem) = self.vault.save_forward(host, old.as_ref(), forward.clone()) {
                    view.update(cx, |view, cx| view.refuse(problem, cx));
                    return;
                }
                if let Some(old) = replacing {
                    live.push((host.clone(), old.clone(), false));
                }
                live.push((host.clone(), forward.clone(), forward.on));
                view.update(cx, |view, cx| view.saved(cx));
            }
            ForwardEvent::Delete { host, forward } => {
                self.vault.delete_forward(host, &forward.id());
                live.push((host.clone(), forward.clone(), false));
            }
            ForwardEvent::Switch { host, forward, on } => {
                self.vault.switch_forward(host, &forward.id(), *on);
                live.push((host.clone(), forward.clone(), *on));
            }
        }
        self.persist();
        self.refresh_forwards(cx);
        for (host, forward, on) in live {
            cx.emit(VaultEvent::Forward { host, forward, on });
        }
        cx.notify();
    }

    /// Adds a finished connection to History and saves it.
    pub fn record_visit(&mut self, visit: Visit, cx: &mut Context<Self>) {
        self.history.record(visit);
        if let Some(path) = &self.history_file {
            // History is a convenience; failing to save it isn't worth a
            // message.
            let _ = self.history.save(path);
        }
        if let Some(view) = &self.history_view {
            let visits = self.history.visits.clone();
            view.update(cx, |view, cx| view.refresh(visits, cx));
        }
    }

    /// The saved snippets, for a session's snippet bar.
    pub fn snippets(&self) -> &[Snippet] {
        &self.vault.snippets
    }

    /// Shows the Snippets screen with the form open on a new snippet.
    pub fn new_snippet(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show(Section::Snippets, window, cx);
        if let Some(view) = &self.snippets {
            view.update(cx, |view, cx| view.open_form(None, window, cx));
        }
    }

    fn on_snippet_event(
        &mut self,
        view: &Entity<SnippetsView>,
        event: &SnippetEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            SnippetEvent::Save { replacing, snippet } => {
                if self
                    .vault
                    .save_snippet(replacing.as_deref(), snippet)
                    .is_err()
                {
                    return;
                }
            }
            SnippetEvent::Delete(name) => self.vault.delete_snippet(name),
        }
        self.persist();
        let saved = self.vault.snippets.clone();
        view.update(cx, |view, cx| view.refresh(saved, cx));
        cx.notify();
    }

    /// Opens a session on `host` that adds `public` to its
    /// `authorized_keys`, as `ssh-copy-id` does, so the session's own
    /// prompts (trust, password) apply and its output shows.
    fn install_key(
        &mut self,
        host: &str,
        public: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self.vault.find(host) else {
            return;
        };
        if let Ok(mut connection) = self.vault.connection(saved) {
            connection.on_login = Some(keys::install_command(public));
            self.connect(connection, window, cx);
        }
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
                        .child(group_heading(group.name.clone(), theme))
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
            .child(search_field("search", &self.search, theme))
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
        let screen: Option<AnyView> = match self.section {
            Section::Hosts => None,
            Section::Keychain => self.keychain.clone().map(Into::into),
            Section::Snippets => self.snippets.clone().map(Into::into),
            Section::Forwarding => self.forwards.clone().map(Into::into),
            Section::KnownHosts => self.known_hosts.clone().map(Into::into),
            Section::History => self.history_view.clone().map(Into::into),
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
            .child(sidebar(self.section, &theme, cx))
            .map(|d| match screen {
                Some(view) => d.child(view),
                None => d
                    .child(self.hosts(&ui, cx))
                    .when_some(self.vault.find(&self.selected), |d, host| {
                        d.child(self.side_panel(host, &ui, cx))
                    }),
            })
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

fn sidebar(current: Section, theme: &Theme, cx: &Context<VaultContent>) -> impl IntoElement {
    div()
        .w(px(220.))
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(4.))
        .children(Section::SIDEBAR.into_iter().map(|(glyph, label, section)| {
            let active = section == Some(current);
            let fg = if active { theme.text } else { theme.text2 };
            let id = format!("nav-{}", label.to_lowercase().replace(' ', "-"));
            div()
                .id(SharedString::from(id.clone()))
                .debug_selector(move || id)
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
                .child(icon(glyph, 18., fg))
                .child(label)
                .when_some(section, |d, section| {
                    d.cursor_pointer().on_click(
                        cx.listener(move |this, _, window, cx| this.show(section, window, cx)),
                    )
                })
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
                if let VaultEvent::Connect(connection) = event {
                    recorded.borrow_mut().push(connection.clone());
                }
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
    fn installing_a_key_runs_it_in_a_session(cx: &mut TestAppContext) {
        let (vault, cx, connects) = open_vault(cx);
        cx.update(|window, cx| {
            vault.update(cx, |vault, cx| {
                vault.install_key("bastion", "ssh-ed25519 AAAA me@laptop", window, cx)
            })
        });
        let connection = connects.borrow().last().cloned().expect("a session opens");
        assert_eq!(connection.target.host, "bastion.example.com");
        let command = connection.on_login.expect("the install runs after login");
        assert!(command.contains("'ssh-ed25519 AAAA me@laptop' >> ~/.ssh/authorized_keys"));
    }

    #[gpui::test]
    fn the_snippets_screen_adds_edits_and_deletes(cx: &mut TestAppContext) {
        let (vault, cx, _) = open_vault(cx);
        let type_into = |cx: &mut VisualTestContext, first: bool, text: &str| {
            cx.update(|window, cx| {
                let view = vault.read(cx).snippets.clone().expect("the screen is open");
                let input = view.read(cx).input(first);
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
        let names = |cx: &mut VisualTestContext| {
            vault.read_with(cx, |vault, _| {
                vault
                    .snippets()
                    .iter()
                    .map(|snippet| snippet.name.clone())
                    .collect::<Vec<_>>()
            })
        };

        click(cx, "nav-snippets");
        click(cx, "new-snippet-screen");
        type_into(cx, true, "Load");
        type_into(cx, false, "uptime");
        click(cx, "save-snippet");
        assert_eq!(names(cx), ["Disk space", "Follow syslog", "Load"]);

        // A clashing name keeps the form open.
        click(cx, "snippet-row-2");
        type_into(cx, true, "disk space");
        click(cx, "save-snippet");
        assert_eq!(names(cx), ["Disk space", "Follow syslog", "Load"]);
        type_into(cx, true, "Uptime");
        click(cx, "save-snippet");
        assert_eq!(names(cx), ["Disk space", "Follow syslog", "Uptime"]);

        click(cx, "snippet-row-0");
        click(cx, "delete-snippet");
        assert_eq!(names(cx), ["Follow syslog", "Uptime"]);
    }

    #[gpui::test]
    fn history_lists_visits_and_reconnects(cx: &mut TestAppContext) {
        use tern_core::history::{Visit, now};

        let (vault, cx, connects) = open_vault(cx);
        let visit = |host: Option<&str>, address: &str| Visit {
            host: host.map(Into::into),
            user: "deploy".into(),
            address: address.into(),
            port: 22,
            auth: tern_core::Auth::Agent,
            started: now() - 120,
            lasted: Some(65),
            ended: "Exited with status 0".into(),
        };
        vault.update(cx, |vault, cx| {
            vault.record_visit(visit(None, "10.0.0.9"), cx);
            vault.record_visit(visit(Some("bastion"), "old.example.com"), cx);
        });
        click(cx, "nav-history");
        assert!(cx.debug_bounds("reconnect-1").is_some());

        // The newest first; a saved host comes back with today's settings.
        click(cx, "reconnect-0");
        let target = connects.borrow().last().cloned().expect("a session").target;
        assert_eq!(target.host, "bastion.example.com");
        click(cx, "reconnect-1");
        let target = connects.borrow().last().cloned().expect("a session").target;
        assert_eq!(target.host, "10.0.0.9");
    }

    #[gpui::test]
    fn port_forwarding_rules_are_saved_and_switched_live(cx: &mut TestAppContext) {
        use tern_core::forward::ForwardKind;

        let (vault, cx, _) = open_vault(cx);
        let switched: Rc<RefCell<Vec<(String, u16, bool)>>> = Rc::default();
        let recorded = switched.clone();
        cx.update(|_, cx| {
            cx.subscribe(&vault, move |_, event: &VaultEvent, _| {
                if let VaultEvent::Forward { host, forward, on } = event {
                    recorded
                        .borrow_mut()
                        .push((host.clone(), forward.listen_port, *on));
                }
            })
            .detach()
        });
        let type_into = |cx: &mut VisualTestContext, port: bool, text: &str| {
            cx.update(|window, cx| {
                let view = vault.read(cx).forwards.clone().expect("the screen is open");
                let (port_input, to_input) = view.read(cx).form_inputs();
                let input = if port { port_input } else { to_input };
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
        let rules = |cx: &mut VisualTestContext, host: &str| {
            vault.read_with(cx, |vault, _| {
                vault.vault.find(host).unwrap().forwards.clone()
            })
        };

        // A new local rule on the first host, prod-db-01.
        click(cx, "nav-port-forwarding");
        click(cx, "new-forward");
        type_into(cx, true, "15432");
        type_into(cx, false, "localhost:5432");
        click(cx, "save-forward");
        let saved = rules(cx, "prod-db-01");
        assert_eq!(saved.len(), 1);
        assert_eq!((saved[0].kind, saved[0].on), (ForwardKind::Local, true));
        assert_eq!(
            switched.borrow().last(),
            Some(&("prod-db-01".to_string(), 15432, true))
        );

        // Switching it off saves that and stops it in open sessions.
        click(cx, "switch-prod-db-01-0");
        assert!(!rules(cx, "prod-db-01")[0].on);
        assert_eq!(
            switched.borrow().last(),
            Some(&("prod-db-01".to_string(), 15432, false))
        );

        // A dynamic rule on another host, then deleting the first.
        click(cx, "new-forward");
        click(cx, "forward-host");
        click(cx, "forward-host-bastion");
        click(cx, "kind-dynamic");
        type_into(cx, true, "1080");
        click(cx, "save-forward");
        assert_eq!(rules(cx, "bastion")[0].kind, ForwardKind::Dynamic);
        click(cx, "forward-prod-db-01-0");
        click(cx, "delete-forward");
        assert!(rules(cx, "prod-db-01").is_empty());

        // A session reports how its rules are doing.
        let socks = rules(cx, "bastion")[0].clone();
        vault.update(cx, |vault, cx| {
            vault.set_forward_state(
                "bastion".into(),
                &socks,
                tern_core::forward::ForwardState::Failed("address in use".into()),
                cx,
            )
        });
        assert_eq!(
            vault.read_with(cx, |vault, _| vault.forward_states.len()),
            1
        );
        vault.update(cx, |vault, cx| vault.clear_forward_states("bastion", cx));
        assert!(vault.read_with(cx, |vault, _| vault.forward_states.is_empty()));
    }

    #[gpui::test]
    fn the_side_panel_opens_sessions_and_switches_rules(cx: &mut TestAppContext) {
        use std::collections::HashMap;

        use tern_core::forward::{Forward, ForwardKind};

        let (vault, cx, _) = open_vault(cx);
        let opened: Rc<RefCell<Vec<(String, bool)>>> = Rc::default();
        let switched: Rc<RefCell<Vec<bool>>> = Rc::default();
        let (seen_open, seen_switch) = (opened.clone(), switched.clone());
        cx.update(|_, cx| {
            cx.subscribe(&vault, move |_, event: &VaultEvent, _| match event {
                VaultEvent::Open { connection, files } => seen_open
                    .borrow_mut()
                    .push((connection.host.clone().unwrap_or_default(), *files)),
                VaultEvent::Forward { on, .. } => seen_switch.borrow_mut().push(*on),
                VaultEvent::Connect(_) => {}
            })
            .detach()
        });
        let rule = Forward::typed(ForwardKind::Local, "8080", "localhost:80").unwrap();
        vault.update(cx, |vault, cx| {
            vault.vault.save_forward("bastion", None, rule).unwrap();
            let preview = Rc::new(RefCell::new(vec!["admin@bastion:~$ uptime".to_string()]));
            vault.set_open_sessions(HashMap::from([("bastion".to_string(), preview)]), cx);
        });

        // A host without a session shows no preview; bastion has one.
        assert!(cx.debug_bounds("host-panel").is_some());
        assert!(cx.debug_bounds("host-preview").is_none());
        click(cx, "host-bastion");
        assert!(cx.debug_bounds("host-preview").is_some());

        click(cx, "side-terminal");
        click(cx, "side-sftp");
        assert_eq!(
            *opened.borrow(),
            [
                ("bastion".to_string(), false),
                ("bastion".to_string(), true)
            ]
        );
        click(cx, "side-switch-0");
        assert_eq!(*switched.borrow(), [false]);
        vault.read_with(cx, |vault, _| {
            assert!(!vault.vault.find("bastion").unwrap().forwards[0].on)
        });
    }

    #[gpui::test]
    fn the_sidebar_switches_screens(cx: &mut TestAppContext) {
        let (_, cx, _) = open_vault(cx);
        assert!(cx.debug_bounds("host-bastion").is_some());
        click(cx, "nav-port-forwarding");
        assert!(cx.debug_bounds("forwarding").is_some());
        click(cx, "nav-history");
        assert!(cx.debug_bounds("history").is_some());

        click(cx, "nav-known-hosts");
        assert!(cx.debug_bounds("known-hosts").is_some());
        assert!(cx.debug_bounds("host-bastion").is_none());
        click(cx, "nav-hosts");
        assert!(cx.debug_bounds("host-bastion").is_some());
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
