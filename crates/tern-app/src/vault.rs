//! The hosts vault: Tern's home screen. It asks the workspace to open a
//! session with [`VaultEvent::Connect`], opens the host editor over itself
//! to add or change hosts, and saves every change to the vault file. While
//! it shows in the active window it checks in the background that the hosts
//! answer, and their traces draw what the checks measured.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::filter::{self, Filter, Reach};
use tern_core::forward::{Forward, ForwardState};
use tern_core::history::{History, Visit};
use tern_core::randomart::Randomart;
use tern_core::reach::{Monitor, Place};
use tern_core::secrets::SYNC_PASSPHRASE;
use tern_core::snippet::Snippet;
use tern_core::ssh::{Connection, Target, key_art, known_host_key, probe, quick_destination};
use tern_core::sync::{Merged, Syncer};
use tern_core::{Auth, Host, HostGroup, HostStatus, Vault, openssh, paths, putty};
use tern_core::{keys, known_hosts};

use crate::bench;
use crate::forwarding::{ForwardEvent, ForwardsView, States};
use crate::history::{HistoryEvent, HistoryView};
use crate::host_editor::{EditorEvent, HostEditor};
use crate::keychain::{KeychainEvent, KeychainView};
use crate::known_hosts::KnownHostsView;
use crate::live::LiveSlots;
use crate::marks::{self, Trace};
use crate::settings;
use crate::snippets::{SnippetEvent, SnippetsView};
use crate::sync::{self, Status, SyncState};
use crate::terminal::Preview;
use crate::theme::{self, MONO, TermPalette, Theme};
use crate::ui::{
    Ui, art_tile, button, group_heading, icon, ink_button, search_field, status_dot, switch,
};

/// How long after a change, or after starting, a sync waits.
const SYNC_SETTLE: Duration = Duration::from_secs(2);
/// How often other devices' changes are looked for.
const SYNC_EVERY: Duration = Duration::from_secs(60);

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
    /// Open these side by side, as split panes.
    OpenTogether(Vec<Connection>),
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

/// How the hosts screen lays hosts out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    Cards,
    List,
}

/// Everything on the vault tab except the header: the sidebar and the
/// screen it picked.
pub struct VaultContent {
    section: Section,
    layout: Layout,
    filter: Filter,
    filter_open: bool,
    /// Held by the hosts screen itself, for arrows, Enter and E.
    focus: FocusHandle,
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
    /// Hosts in `~/.ssh/config` not in the vault yet, likewise.
    openssh: Vec<Host>,
    /// What the background checks found.
    monitor: Monitor,
    /// The vault is the tab on screen; checks run only then.
    showing: bool,
    _checks: Option<Task<()>>,
    _subscription: Subscription,
    _editor_events: Option<Subscription>,
    _screen_events: Vec<Subscription>,
    /// The window, for applying synced settings after background work.
    window: AnyWindowHandle,
    /// Seals and opens sync files, and the passphrase it was made with.
    syncer: Option<(String, Arc<Mutex<Syncer>>)>,
    /// The sync running now, the one waiting after a change, and the
    /// minute-by-minute one.
    _sync: Option<Task<()>>,
    _sync_soon: Option<Task<()>>,
    _sync_loop: Option<Task<()>>,
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
        // Sync at start, then every minute for other devices' changes.
        content._sync_loop = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SYNC_SETTLE).await;
                if this.update(cx, |this, cx| this.sync_now(cx)).is_err() {
                    break;
                }
                cx.background_executor().timer(SYNC_EVERY).await;
            }
        }));
        content
    }

    /// Whether the vault is the tab on screen.
    pub fn set_showing(&mut self, showing: bool) {
        self.showing = showing;
    }

    /// Gives the vault the keyboard, for moving through hosts.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus.focus(window, cx);
    }

    /// Switches the screen beside the sidebar. Screens that show files
    /// read them again, for changes made meanwhile.
    /// Shows the screen called `name` (`keychain`, `snippets`,
    /// `forwarding`, `known-hosts`, `history`; anything else is Hosts).
    pub fn show_named(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let section = match name {
            "keychain" => Section::Keychain,
            "snippets" => Section::Snippets,
            "forwarding" => Section::Forwarding,
            "known-hosts" => Section::KnownHosts,
            "history" => Section::History,
            _ => Section::Hosts,
        };
        self.show(section, window, cx);
    }

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
                let hosts: Vec<String> = self.vault.hosts().map(|host| host.name.clone()).collect();
                if let Some(view) = &self.snippets {
                    view.update(cx, |view, cx| view.set_hosts(hosts.clone(), cx));
                }
                if self.snippets.is_none() {
                    let saved = self.vault.snippets.clone();
                    let view = cx.new(|cx| SnippetsView::new(saved, hosts, window, cx));
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
        let mut proxies = Vec::new();
        for host in self.vault.hosts() {
            let place = self.vault.first_hop(host);
            if !places.contains(&place) {
                proxies.push(self.vault.first_hop_host(host).proxy.clone());
                places.push(place);
            }
        }
        let due = self.monitor.due(&places, Instant::now());
        if due.is_empty() {
            return;
        }
        for place in due {
            let proxy = places
                .iter()
                .position(|each| *each == place)
                .and_then(|at| proxies[at].clone());
            let check = probe(&place.0, place.1, proxy);
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

    /// The hosts the search and the filter let through, with their group,
    /// in vault order.
    fn visible(&self) -> Vec<(&str, &Host)> {
        self.vault
            .groups
            .iter()
            .flat_map(|group| {
                group
                    .hosts
                    .iter()
                    .map(move |host| (group.name.as_str(), host))
            })
            .filter(|(group, host)| {
                host.matches(&self.query)
                    && self.filter.matches(group, host, self.trace(host).status)
            })
            .collect()
    }

    /// Arrows move the selection, Enter connects, E edits; only while the
    /// hosts screen itself has focus, not a field on it.
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window) || self.editor.is_some() {
            return;
        }
        let modifiers = &event.keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform {
            return;
        }
        let delta = match event.keystroke.key.as_str() {
            "up" | "left" => -1,
            "down" | "right" => 1,
            "enter" => {
                let selected = self.selected.clone();
                self.connect_saved(&selected, window, cx);
                cx.stop_propagation();
                return;
            }
            "e" => {
                if self.vault.find(&self.selected).is_some() {
                    self.open_editor(Some(self.selected.clone()), window, cx);
                }
                cx.stop_propagation();
                return;
            }
            _ => return,
        };
        let visible: Vec<&str> = self
            .visible()
            .iter()
            .map(|(_, host)| host.name.as_str())
            .collect();
        if let Some(next) = filter::step(&visible, &self.selected, delta) {
            self.selected = next.to_string();
        }
        cx.stop_propagation();
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
            layout: Layout::Cards,
            filter: Filter::default(),
            filter_open: false,
            focus: cx.focus_handle(),
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
            openssh: Vec::new(),
            monitor: Monitor::default(),
            showing: true,
            _checks: None,
            _subscription: subscription,
            _editor_events: None,
            _screen_events: Vec::new(),
            window: window.window_handle(),
            syncer: None,
            _sync: None,
            _sync_soon: None,
            _sync_loop: None,
        };
        content.refresh_keys();
        content.refresh_imports();
        content
    }

    /// Adds every PuTTY session not in the vault yet to the PuTTY group and
    /// selects the first.
    fn import_putty(&mut self, cx: &mut Context<Self>) {
        let hosts = std::mem::take(&mut self.putty);
        self.import(hosts, putty::GROUP, cx);
    }

    /// Adds every `~/.ssh/config` host not in the vault yet to the OpenSSH
    /// group and selects the first.
    fn import_openssh(&mut self, cx: &mut Context<Self>) {
        let hosts = std::mem::take(&mut self.openssh);
        self.import(hosts, openssh::GROUP, cx);
    }

    fn import(&mut self, hosts: Vec<Host>, group: &str, cx: &mut Context<Self>) {
        if let Some(first) = hosts.first() {
            self.selected = first.name.clone();
        }
        for host in hosts {
            self.vault.save(None, host, group);
        }
        self.persist(cx);
        self.refresh_keys();
        // Either source may have offered what the other just added.
        let vault = &self.vault;
        let in_vault = |host: &Host| {
            vault.hosts().any(|have| {
                have.user == host.user && have.address == host.address && have.port == host.port
            })
        };
        self.putty.retain(|host| !in_vault(host));
        self.openssh.retain(|host| !in_vault(host));
        cx.notify();
    }

    /// Re-reads what PuTTY and `~/.ssh/config` have that the vault lacks.
    /// Tests offer their own instead of reading this computer's.
    fn refresh_imports(&mut self) {
        if cfg!(not(test)) {
            self.putty = putty::importable_here(&self.vault);
            self.openssh = openssh::importable_here(&self.vault);
        }
    }

    /// Writes the vault to its file, if it has one. A failure stays on
    /// screen until a later save works.
    /// Saves a change made here, and syncs it shortly.
    fn persist(&mut self, cx: &mut Context<Self>) {
        self.write_vault();
        if settings::current(cx).sync.folder.is_some() {
            self._sync_soon = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SYNC_SETTLE).await;
                let _ = this.update(cx, |this, cx| this.sync_now(cx));
            }));
        }
    }

    /// Syncs now, if a folder and passphrase are set and no sync is
    /// running already.
    pub fn sync_now(&mut self, cx: &mut Context<Self>) {
        let settings = settings::current(cx);
        if settings.sync.folder.is_none() {
            if sync::state(cx).status != Status::Off {
                sync::set_state(cx, SyncState::default());
            }
            return;
        }
        if self._sync.is_some() {
            return;
        }
        let devices = sync::state(cx).devices;
        let Some(passphrase) = sync::secrets(cx).load(SYNC_PASSPHRASE) else {
            sync::set_state(
                cx,
                SyncState {
                    status: Status::Failed("Set the sync passphrase".into()),
                    devices,
                },
            );
            return;
        };
        let syncer = match &self.syncer {
            Some((made_with, syncer)) if *made_with == passphrase => syncer.clone(),
            _ => {
                let syncer = Arc::new(Mutex::new(Syncer::new(&passphrase)));
                self.syncer = Some((passphrase, syncer.clone()));
                syncer
            }
        };
        sync::set_state(
            cx,
            SyncState {
                status: Status::Syncing,
                devices,
            },
        );
        let keys =
            tern_core::sync::keys_to_sync(&keys::list_in(&keys::dirs()), &settings.sync.keys);
        let vault = self.vault.clone();
        let work = cx
            .background_executor()
            .spawn(async move { sync::run(&syncer, &vault, &settings, keys) });
        self._sync = Some(cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                this._sync = None;
                this.finish_sync(result, cx);
            });
        }));
    }

    /// Takes in what a sync brought: hosts and snippets, newer settings
    /// and keys this device lacks.
    fn finish_sync(
        &mut self,
        result: Result<(Merged, Vec<(String, u64)>), String>,
        cx: &mut Context<Self>,
    ) {
        let (merged, devices) = match result {
            Ok(done) => done,
            Err(reason) => {
                let devices = sync::state(cx).devices;
                sync::set_state(
                    cx,
                    SyncState {
                        status: Status::Failed(reason.into()),
                        devices,
                    },
                );
                return;
            }
        };
        let settings = settings::current(cx);
        // Changes made here while the sync ran are newer than what it read.
        let synced = tern_core::sync::snapshot(&merged.vault, &settings, Vec::new());
        let vault = tern_core::sync::merge(&self.vault, &settings, &[synced]).vault;
        if vault != self.vault {
            self.vault = vault;
            self.write_vault();
            if self.vault.find(&self.selected).is_none() {
                self.selected = self
                    .vault
                    .hosts()
                    .next()
                    .map(|host| host.name.clone())
                    .unwrap_or_default();
            }
            self.refresh_keys();
            self.refresh_forwards(cx);
            if let Some(view) = &self.snippets {
                let saved = self.vault.snippets.clone();
                view.update(cx, |view, cx| view.refresh(saved, cx));
            }
        }
        if let Some(newer) = &merged.settings {
            settings::take_synced(cx, newer);
            let window = self.window;
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, cx| settings::apply_appearance(window, cx));
            });
        }
        let have = keys::list_in(&keys::dirs());
        let _ = tern_core::sync::receive_keys(&paths::keys(), &merged.keys, &have);
        sync::set_state(
            cx,
            SyncState {
                status: Status::Synced(tern_core::host::now_ms()),
                devices,
            },
        );
        cx.notify();
    }

    fn write_vault(&mut self) {
        self.filter.forget_missing(&self.vault);
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

    #[cfg(test)]
    fn offer_openssh(&mut self, hosts: Vec<Host>) {
        self.openssh = hosts;
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
        self.persist(cx);
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
        let palette = TermPalette::current(cx);
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
        self.persist(cx);
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
            // Each host's session types the snippet once its shell starts,
            // after the host's own command.
            SnippetEvent::Run { snippet, hosts } => {
                let connections: Vec<Connection> = hosts
                    .iter()
                    .filter_map(|name| self.vault.find(name))
                    .filter_map(|host| self.vault.connection(host).ok())
                    .map(|mut connection| {
                        connection.on_login =
                            Some(snippet.after_login(connection.on_login.as_deref()));
                        connection
                    })
                    .collect();
                cx.emit(VaultEvent::OpenTogether(connections));
                return;
            }
        }
        self.persist(cx);
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
                self.persist(cx);
                if let Some((place, latency_ms)) = checked {
                    self.monitor
                        .record(place, Some(*latency_ms), Instant::now());
                }
                self.selected = host.name.clone();
                self.refresh_keys();
            }
            EditorEvent::Delete(name) => {
                let id = self.vault.find(name).map(|host| host.id.clone());
                if self.vault.delete(name).is_ok() {
                    if let Some(id) = id {
                        let _ = sync::secrets(cx).forget(&tern_core::secrets::host_password(&id));
                    }
                    self.persist(cx);
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
            self.refresh_imports();
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
            .children({
                let visible = self.visible();
                self.vault.groups.iter().filter_map(move |group| {
                    let hosts: Vec<&Host> = visible
                        .iter()
                        .filter(|(of, _)| *of == group.name)
                        .map(|(_, host)| *host)
                        .collect();
                    if hosts.is_empty() {
                        return None;
                    }
                    let hosts = match self.layout {
                        Layout::Cards => div().flex().flex_wrap().gap(px(14.)).children(
                            hosts
                                .into_iter()
                                .map(|host| self.host_card(host, ui, cx).into_any_element()),
                        ),
                        Layout::List => div().flex().flex_col().gap(px(8.)).children(
                            hosts
                                .into_iter()
                                .map(|host| self.host_row(host, ui, cx).into_any_element()),
                        ),
                    };
                    Some(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(14.))
                            .child(group_heading(group.name.clone(), theme))
                            .child(hosts),
                    )
                })
            })
    }

    /// A host as a row of the list: art, name, target, tags, trace and
    /// latency, and its actions. The selected one is inverted.
    fn host_row(&self, host: &Host, ui: &Ui, cx: &Context<Self>) -> impl IntoElement {
        let selected = host.name == self.selected;
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
        let row_id = format!("row-{}", host.name);
        let (pick, edit, connect) = (host.name.clone(), host.name.clone(), host.name.clone());
        div()
            .id(SharedString::from(row_id.clone()))
            .debug_selector(move || row_id)
            .h(px(60.))
            .pl(px(10.))
            .pr(px(12.))
            .flex()
            .items_center()
            .gap(px(14.))
            .rounded(px(18.))
            .bg(t.card)
            .text_color(t.text)
            .shadow(ui.theme.shadow())
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selected = pick.clone();
                this.focus.focus(window, cx);
                cx.notify();
            }))
            .child(art_tile(art, 40., 12., &t))
            .child(
                div()
                    .w(px(180.))
                    .flex_none()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .truncate()
                    .child(host.name.clone()),
            )
            .child(
                div()
                    .w(px(200.))
                    .flex_none()
                    .font_family(MONO)
                    .text_size(px(12.))
                    .text_color(t.text2)
                    .truncate()
                    .child(host.target()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family(MONO)
                    .text_size(px(11.5))
                    .text_color(t.text3)
                    .truncate()
                    .child(host.tags.join(" / ")),
            )
            .child(if self.editor.is_some() {
                div().w(px(110.)).into_any_element()
            } else {
                ui.trace(
                    format!("row-trace-{}", host.name),
                    trace,
                    marks::seed(&host.name),
                    t.text,
                    t.text3,
                )
                .w(px(110.))
                .h(px(16.))
                .into_any_element()
            })
            .child(
                div()
                    .w(px(44.))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .font_family(MONO)
                    .text_size(px(12.))
                    .child(latency),
            )
            .when(selected, |d| {
                d.child(
                    round_button(format!("edit-{}", host.name), IconName::Pencil, false, &t)
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
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.connect_saved(&connect, window, cx);
                    }))
            })
    }

    /// The Filter button, its count of chosen values beside its name, and
    /// the menu it opens: groups, tags and reachability to show.
    fn filter_button(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let count = self.filter.count();
        let groups: Vec<String> = self
            .vault
            .groups
            .iter()
            .map(|group| group.name.clone())
            .collect();
        let tags = filter::tags(&self.vault);
        let hover = theme.card2;
        let option = |id: String, label: String, on: bool| {
            div()
                .id(SharedString::from(id.clone()))
                .debug_selector(move || id)
                .h(px(34.))
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(10.))
                .rounded(px(10.))
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(div().flex_1().min_w_0().truncate().child(label))
                .child(
                    div()
                        .w(px(14.))
                        .when(on, |d| d.child(icon(IconName::Check, 14., theme.text))),
                )
        };
        let heading = |name: &'static str| {
            div()
                .px(px(10.))
                .pt(px(8.))
                .pb(px(2.))
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text2)
                .child(name)
        };
        div()
            .child(
                button("filter", theme)
                    .debug_selector(|| "filter".into())
                    .h(px(46.))
                    .px(px(16.))
                    .rounded(px(16.))
                    .text_size(px(14.))
                    .child(icon(IconName::Funnel, 15., theme.text))
                    .child("Filter")
                    .when(count > 0, |d| {
                        d.child(
                            div()
                                .font_family(MONO)
                                .text_size(px(13.))
                                .text_color(theme.text2)
                                .child(count.to_string()),
                        )
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.filter_open = true;
                        cx.notify();
                    })),
            )
            .when(self.filter_open, |d| {
                let reaches = [
                    (Reach::Online, "Online"),
                    (Reach::Offline, "Offline"),
                    (Reach::Unchecked, "Not checked"),
                ];
                d.child(deferred(
                    anchored().snap_to_window_with_margin(px(8.)).child(
                        div()
                            .id("filter-menu")
                            .occlude()
                            .mt(px(6.))
                            .w(px(260.))
                            .max_h(px(420.))
                            .overflow_y_scroll()
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
                                this.filter_open = false;
                                cx.notify();
                            }))
                            .child(heading("Groups"))
                            .children(groups.into_iter().map(|group| {
                                let on = self.filter.groups.contains(&group);
                                let picked = group.clone();
                                option(format!("filter-group-{group}"), group, on).on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        this.filter.toggle_group(&picked);
                                        cx.notify();
                                    }),
                                )
                            }))
                            .when(!tags.is_empty(), |d| d.child(heading("Tags")))
                            .children(tags.into_iter().map(|tag| {
                                let on = self.filter.tags.contains(&tag);
                                let picked = tag.clone();
                                option(format!("filter-tag-{tag}"), tag, on).on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.filter.toggle_tag(&picked);
                                        cx.notify();
                                    },
                                ))
                            }))
                            .child(heading("Status"))
                            .children(reaches.into_iter().map(|(reach, label)| {
                                let on = self.filter.reach.contains(&reach);
                                option(
                                    format!("filter-status-{}", label.to_lowercase()),
                                    label.into(),
                                    on,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.filter.toggle_reach(reach);
                                        cx.notify();
                                    },
                                ))
                            }))
                            .when(count > 0, |d| {
                                d.child(div().my(px(4.)).h(px(1.)).bg(theme.line)).child(
                                    option("filter-clear".into(), "Clear".into(), false).on_click(
                                        cx.listener(|this, _, _, cx| {
                                            this.filter = Filter::default();
                                            cx.notify();
                                        }),
                                    ),
                                )
                            }),
                    ),
                ))
            })
    }

    /// Cards or a list, as a well of two glyphs.
    fn layout_switch(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        div()
            .h(px(46.))
            .flex()
            .items_center()
            .gap(px(4.))
            .p(px(4.))
            .rounded(px(16.))
            .bg(theme.card2)
            .children(
                [
                    (Layout::Cards, IconName::LayoutGrid, "view-cards"),
                    (Layout::List, IconName::List, "view-list"),
                ]
                .into_iter()
                .map(|(layout, glyph, id)| {
                    let on = self.layout == layout;
                    div()
                        .id(id)
                        .debug_selector(move || id.into())
                        .size(px(38.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(12.))
                        .when(on, |d| d.bg(theme.card).shadow(theme.shadow_sm()))
                        .cursor_pointer()
                        .child(icon(glyph, 16., if on { theme.text } else { theme.text2 }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.layout = layout;
                            cx.notify();
                        }))
                }),
            )
    }

    fn toolbar(&self, theme: &Theme, cx: &Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(12.))
            .child(search_field("search", &self.search, theme))
            .child(self.layout_switch(theme, cx))
            .child(self.filter_button(theme, cx))
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
            .when(!self.openssh.is_empty(), |d| {
                d.child(
                    button("import-openssh", theme)
                        .debug_selector(|| "import-openssh".into())
                        .h(px(46.))
                        .px(px(16.))
                        .rounded(px(16.))
                        .text_size(px(14.))
                        .child(icon(IconName::Import, 16., theme.text))
                        .child("Import from OpenSSH")
                        .on_click(cx.listener(|this, _, _, cx| this.import_openssh(cx))),
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
            .on_click(cx.listener(move |this, _, window, cx| {
                this.selected = pick.clone();
                this.focus.focus(window, cx);
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
            .id("vault")
            .track_focus(&self.focus)
            .key_context("Vault")
            .on_key_down(cx.listener(Self::key_down))
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
        id: tern_core::host::new_id(),
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
    use gpui_kit::{AppContext as _, Entity};
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
    fn importing_from_openssh_keeps_jump_hosts(cx: &mut TestAppContext) {
        let (vault, cx, _) = open_vault(cx);
        // Seeded here rather than read from this computer's ~/.ssh/config.
        let config = "Host gw\n  HostName gw.lan\n  User ops\nHost app\n  HostName app.lan\n  User ops\n  ProxyJump gw\n";
        let hosts = vault.read_with(cx, |vault, _| {
            tern_core::openssh::importable(&vault.vault, config, std::path::Path::new("."), "me")
        });
        vault.update(cx, |vault, cx| {
            vault.offer_openssh(hosts);
            cx.notify();
        });

        click(cx, "import-openssh");
        vault.read_with(cx, |vault, _| {
            assert_eq!(vault.vault.group_of("app"), Some("OpenSSH"));
            let app = vault.vault.find("app").unwrap();
            let connection = vault.vault.connection(app).unwrap();
            assert_eq!(connection.jumps[0].target.host, "gw.lan");
            assert_eq!(vault.selected, "gw");
        });
        assert!(cx.debug_bounds("import-openssh").is_none());
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
    fn runs_a_snippet_on_several_hosts(cx: &mut TestAppContext) {
        let (vault, cx, _) = open_vault(cx);
        let together: Rc<RefCell<Vec<Connection>>> = Rc::default();
        let seen = together.clone();
        cx.update(|_, cx| {
            cx.subscribe(&vault, move |_, event: &VaultEvent, _| {
                if let VaultEvent::OpenTogether(connections) = event {
                    *seen.borrow_mut() = connections.clone();
                }
            })
            .detach()
        });
        click(cx, "nav-snippets");
        let command = vault.read_with(cx, |vault, _| vault.vault.snippets[0].command.clone());

        // Nothing chosen, nothing runs; then two hosts.
        click(cx, "run-snippet-0");
        click(cx, "run-snippet");
        assert!(together.borrow().is_empty());
        click(cx, "run-on-bastion");
        click(cx, "run-on-prod-db-01");
        click(cx, "run-on-prod-db-01");
        click(cx, "run-on-prod-web-01");
        click(cx, "run-snippet");
        let connections = together.borrow().clone();
        let hosts: Vec<String> = connections
            .iter()
            .map(|connection| connection.host.clone().unwrap())
            .collect();
        assert_eq!(hosts, ["bastion", "prod-web-01"]);
        assert!(connections.iter().all(|connection| {
            connection
                .on_login
                .as_deref()
                .is_some_and(|typed| typed.ends_with(command.trim_end()))
        }));
        // The chooser closes once it runs.
        assert!(cx.debug_bounds("run-snippet").is_none());
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
                VaultEvent::Connect(_) | VaultEvent::OpenTogether(_) => {}
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
    fn lists_filters_and_moves_with_the_keyboard(cx: &mut TestAppContext) {
        let (vault, cx, connects) = open_vault(cx);
        let selected = |cx: &mut VisualTestContext| vault.read_with(cx, |v, _| v.selected.clone());

        // The list shows the same hosts as rows.
        click(cx, "view-list");
        assert!(cx.debug_bounds("row-bastion").is_some());
        assert!(cx.debug_bounds("host-bastion").is_none());

        // A tag and a group narrow it; the button counts what's chosen.
        click(cx, "filter");
        click(cx, "filter-tag-docker");
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(cx.debug_bounds("row-stage-api").is_some());
        assert!(cx.debug_bounds("row-bastion").is_none());
        vault.read_with(cx, |vault, _| assert_eq!(vault.filter.count(), 1));
        click(cx, "filter-group-Production");
        assert!(
            cx.debug_bounds("row-stage-api").is_none(),
            "docker hosts aren't in Production"
        );
        click(cx, "filter-clear");
        vault.read_with(cx, |vault, _| assert_eq!(vault.filter.count(), 0));

        // Arrows move through what's on screen, Enter connects, E edits.
        click(cx, "row-prod-web-02");
        cx.simulate_keystrokes("down");
        assert_eq!(selected(cx), "bastion");
        cx.simulate_keystrokes("up up");
        assert_eq!(selected(cx), "prod-web-01");
        cx.simulate_keystrokes("enter");
        assert_eq!(
            connects.borrow().last().map(|c| c.target.host.clone()),
            Some("web-01.internal".into())
        );
        cx.simulate_keystrokes("e");
        vault.read_with(cx, |vault, _| assert!(vault.editor.is_some()));
    }

    #[gpui::test]
    fn syncs_with_another_device_through_a_folder(cx: &mut TestAppContext) {
        use std::sync::Arc;

        use tern_core::secrets::{MemorySecrets, SYNC_PASSPHRASE, Secrets as _};
        use tern_core::settings::Settings;
        use tern_core::sync::{SyncSettings, Syncer, snapshot};

        use crate::sync::{SecretsStore, Status};

        let folder = std::env::temp_dir().join(format!("tern-ui-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap();
        // The other device already wrote its file, with a host we lack.
        let mut theirs = Vault::default();
        theirs.save(None, Host::new("nas", "admin", "nas.lan", 22), "Home");
        let there = Settings {
            sync: SyncSettings {
                folder: Some(folder.clone()),
                device: "Laptop".into(),
                device_id: "laptop".into(),
                ..SyncSettings::default()
            },
            ..Settings::default()
        };
        Syncer::new("open sesame")
            .write(&folder, &snapshot(&theirs, &there, Vec::new()))
            .unwrap();

        let (vault, cx, _) = open_vault(cx);
        let secrets = Arc::new(MemorySecrets::default());
        secrets.store(SYNC_PASSPHRASE, "open sesame").unwrap();
        cx.update(|_, cx| {
            cx.set_global(SecretsStore(secrets));
            crate::settings::update(cx, |settings| {
                settings.sync.folder = Some(folder.clone());
                settings.sync.device = "Desktop".into();
                settings.sync.device_id = "desktop".into();
            });
        });
        vault.update(cx, |vault, cx| vault.sync_now(cx));
        cx.run_until_parked();

        vault.read_with(cx, |vault, _| {
            assert_eq!(vault.vault.group_of("nas"), Some("Home"));
            assert!(vault.vault.find("bastion").is_some(), "ours are kept");
        });
        let state = cx.update(|_, cx| crate::sync::state(cx));
        assert!(
            matches!(state.status, Status::Synced(_)),
            "{:?}",
            state.status
        );
        assert_eq!(state.devices[0].0, "Laptop");
        assert!(folder.join("Tern").join("desktop.tern").exists());
        std::fs::remove_dir_all(&folder).unwrap();
    }

    #[gpui::test]
    fn saves_passwords_in_the_keychain_only(cx: &mut TestAppContext) {
        use std::sync::Arc;

        use tern_core::secrets::{MemorySecrets, Secrets as _, host_password};

        use crate::sync::SecretsStore;
        use crate::terminal::TerminalView;

        let (vault, cx, _) = open_vault(cx);
        let secrets = Arc::new(MemorySecrets::default());
        let kept = secrets.clone();
        cx.update(|_, cx| cx.set_global(SecretsStore(secrets)));
        let editor = |cx: &mut VisualTestContext| {
            vault.read_with(cx, |vault, _| {
                vault.editor.clone().expect("the editor is open")
            })
        };

        click(cx, "host-bastion");
        click(cx, "edit-host");
        click(cx, "sign-in-password");
        let open = editor(cx);
        cx.update(|window, cx| {
            let input = open.read(cx).password_input();
            input.update(cx, |state, cx| state.focus(window, cx));
        });
        cx.run_until_parked();
        cx.simulate_input("s3cret");
        click(cx, "save-password");
        click(cx, "save-host");
        let id = vault.read_with(cx, |vault, _| {
            vault.vault.find("bastion").unwrap().id.clone()
        });
        assert_eq!(kept.load(&host_password(&id)).as_deref(), Some("s3cret"));

        // Never in the vault file.
        let dir = std::env::temp_dir().join(format!("tern-ui-password-{}", std::process::id()));
        let path = dir.join("vault.json");
        vault.read_with(cx, |vault, _| vault.vault.save_to(&path).unwrap());
        assert!(!std::fs::read_to_string(&path).unwrap().contains("s3cret"));
        std::fs::remove_dir_all(&dir).unwrap();

        // A session tries it before asking.
        let connection = vault.read_with(cx, |vault, _| {
            vault
                .vault
                .connection(vault.vault.find("bastion").unwrap())
                .unwrap()
        });
        let terminal = cx.new(|cx| TerminalView::unconnected(connection, cx));
        terminal.read_with(cx, |terminal, _| {
            assert_eq!(terminal.password(), Some("s3cret"))
        });

        // Reopened, the switch is on; deleting the host forgets it.
        click(cx, "edit-host");
        let open = editor(cx);
        open.read_with(cx, |editor, _| assert!(editor.saves_password()));
        click(cx, "delete-host");
        assert_eq!(kept.load(&host_password(&id)), None);
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
        // Environment variables are NAME=value pairs.
        type_into(cx, Field::Env, "TZ");
        click(cx, "save-host");
        let problems = vault.read_with(cx, |vault, cx| {
            vault.editor.as_ref().unwrap().read(cx).problem_fields()
        });
        assert_eq!(problems, [Field::Env]);
        type_into(cx, Field::Env, "LANG=C.UTF-8 TZ=UTC");
        // A proxy is written as a URL.
        click(cx, "proxy-switch");
        type_into(cx, Field::Proxy, "proxy.lan:1080");
        click(cx, "save-host");
        let problems = vault.read_with(cx, |vault, cx| {
            vault.editor.as_ref().unwrap().read(cx).problem_fields()
        });
        assert_eq!(problems, [Field::Proxy]);
        type_into(cx, Field::Proxy, "socks5://proxy.lan:1080");
        click(cx, "save-host");
        assert!(!editor_open(cx));
        vault.read_with(cx, |vault, _| {
            let host = vault.vault.find("db-new").expect("the new host is saved");
            assert_eq!((host.port, host.user.as_str()), (2200, "root"));
            assert_eq!(host.env.get("TZ").map(String::as_str), Some("UTC"));
            assert_eq!(host.env.len(), 2);
            let proxy = host.proxy.as_ref().expect("the proxy is saved");
            assert_eq!(proxy.to_string(), "socks5://proxy.lan:1080");
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
        let hops = |connects: &Rc<RefCell<Vec<Connection>>>| -> Vec<String> {
            let connection = connects.borrow().last().cloned().expect("a connect event");
            connection
                .jumps
                .into_iter()
                .map(|jump| jump.target.host)
                .collect()
        };
        assert_eq!(hops(&connects), ["bastion.example.com"]);

        // A chain: prod-web-01 through prod-db-01, which goes through
        // bastion. Bastion can't then go through either of them.
        click(cx, "host-prod-web-01");
        click(cx, "edit-host");
        click(cx, "jump-switch");
        click(cx, "jump-picker");
        click(cx, "jump-picker-prod-db-01");
        click(cx, "save-host");
        click(cx, "connect-prod-web-01");
        assert_eq!(hops(&connects), ["bastion.example.com", "10.0.4.21"]);
        click(cx, "host-bastion");
        click(cx, "edit-host");
        click(cx, "jump-switch");
        click(cx, "jump-picker");
        assert!(cx.debug_bounds("jump-picker-prod-db-01").is_none());
        assert!(cx.debug_bounds("jump-picker-prod-web-01").is_none());
        assert!(cx.debug_bounds("jump-picker-pi-homelab").is_some());
        cx.simulate_keystrokes("escape");
        if editor_open(cx) {
            cx.simulate_keystrokes("escape");
        }
        assert!(!editor_open(cx));

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
