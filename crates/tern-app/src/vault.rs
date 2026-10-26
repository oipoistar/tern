//! The hosts vault: Tern's home screen. It asks the workspace to open a
//! session with [`VaultEvent::Connect`], opens the host editor over itself
//! to add or change hosts, and saves every change to the vault file. While
//! it shows in the active window it checks in the background that the hosts
//! answer, and their traces draw what the checks measured. A group can be
//! filled by a command run here, and refreshed from it. The sidebar's
//! switcher opens another vault, which the workspace shows in place of this
//! one ([`VaultEvent::Switch`]).

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::filter::{self, Filter, Reach};
use tern_core::forward::{Forward, ForwardState};
use tern_core::history::{History, Visit};
use tern_core::randomart::Randomart;
use tern_core::reach::{Monitor, Place};
use tern_core::restore::OpenTabs;
use tern_core::secrets;
use tern_core::snippet::Snippet;
use tern_core::sources::{self, Sources};
use tern_core::ssh::{
    ConnectError, Connection, Event, Session, SessionInput, Target, TermSize, key_art,
    known_host_keys_for, probe, quick_destination,
};
use tern_core::sync::Syncer;
use tern_core::vault::Hops;
use tern_core::vaults::{FIRST, VaultEntry};
use tern_core::waiting::Waiting;
use tern_core::{Auth, Host, HostGroup, HostStatus, Vault, openssh, paths, putty};
use tern_core::{keys, known_hosts};

use crate::bench;
use crate::forwarding::{Alone, ForwardEvent, ForwardsView, States};
use crate::history::{HistoryEvent, HistoryView};
use crate::host_editor::{EditorEvent, HostEditor};
use crate::identities::{IdentitiesView, IdentityEvent};
use crate::keychain::{KeychainEvent, KeychainView};
use crate::known_hosts::KnownHostsView;
use crate::live::LiveSlots;
use crate::marks::{self, Trace};
use crate::settings;
use crate::snippets::{SnippetEvent, SnippetsView};
use crate::sync::{self, Status, SyncState, Synced};
use crate::terminal::Preview;
use crate::theme::{self, MONO, TermPalette, Theme};
use crate::ui::{
    Ui, art_tile, button, group_heading, icon, ink_button, search_field, status_dot, switch,
};
use crate::vaults;

/// The line for filling a group from a command run here.
struct SourceForm {
    group: Entity<InputState>,
    command: Entity<InputState>,
    /// Why the last run failed, in one line.
    problem: Option<SharedString>,
    /// The command running.
    _run: Option<Task<()>>,
    _enter: Subscription,
}

/// How long after a change, or after starting, a sync waits.
const SYNC_SETTLE: Duration = Duration::from_secs(2);
/// How often other devices' changes are looked for.
const SYNC_EVERY: Duration = Duration::from_secs(60);
/// How soon after a reachability check ends the next due one starts.
const FILL_AFTER: Duration = Duration::from_millis(100);

/// Where `tern-devserver` keeps the key it accepts.
const DEV_KEY: &str = "dev/test-ssh/id_ed25519";

/// Wide enough for `user@host` and two tags; three fit beside the sidebar
/// in the default window.
const CARD_WIDTH: f32 = 320.;

/// A row of the hosts screen's list, which builds only those on screen.
/// Each says what its height depends on, so that a change to one leaves
/// the others measured.
#[derive(Clone, Debug, PartialEq)]
enum Item {
    /// The toolbar and the lines under it: an open group form (and whether
    /// it shows a problem), a notice, the typed destination.
    Top {
        form: Option<bool>,
        notice: bool,
        destination: bool,
    },
    /// A group's heading, by the group's index in the vault, with its hosts
    /// shown (by index) and whether a command fills it or failed to.
    Heading {
        group: usize,
        shown: Vec<usize>,
        sourced: bool,
        problem: bool,
    },
    /// Hosts of a group, by index: a line of cards, or a row. The last
    /// before the next group is spaced from it.
    Hosts {
        group: usize,
        hosts: Vec<usize>,
        cards: bool,
        last: bool,
    },
}

/// The space between two cards, and between lines of them.
const CARD_GAP: f32 = 14.;

/// How many cards fit side by side in `width`; three before the list is
/// first laid out.
fn cards_per_line(width: Pixels) -> usize {
    if width <= px(0.) {
        return 3;
    }
    (((width + px(CARD_GAP)) / px(CARD_WIDTH + CARD_GAP)).floor() as usize).max(1)
}

/// A session without a shell, carrying a host's rules that run without a
/// terminal.
/// How a failed Wake's notice starts, so a later one that works can clear
/// it and leave other notices alone.
const WAKE_FAILED: &str = "Can't wake it";

struct Background {
    input: SessionInput,
    rules: Vec<Forward>,
    _events: Task<()>,
}

/// What the vault switcher's line names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VaultLine {
    /// A new vault.
    New,
    /// The open vault, renamed.
    Rename,
}

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
    /// Another vault is open now: show it in place of this one.
    Switch,
    /// A host came back: open a tab for it, out of the way, running the
    /// connection's one-off command.
    Run(Connection),
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

/// Where reachability checks go, as the last look for due checks worked
/// out ([`VaultContent::places`]).
struct Places {
    waited: bool,
    all: Vec<Place>,
    wanted: Vec<Place>,
    proxies: HashMap<Place, Option<tern_core::proxy::Proxy>>,
}

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
    /// The Keychain screen shows identities instead of keys.
    show_identities: bool,
    /// Which copy button just copied, for a moment's "Copied".
    copied: Option<&'static str>,
    _copied: Option<Task<()>>,
    identities: Option<Entity<IdentitiesView>>,
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
    /// Rules run without a terminal, by host: a session without a shell
    /// carries them.
    background: HashMap<String, Background>,
    /// The open sessions' previews, by saved host.
    open_sessions: HashMap<String, Preview>,
    /// The same for each window, which the window keeps up to date.
    sessions_by_window: HashMap<EntityId, HashMap<String, Preview>>,
    vault: Vault,
    /// Where changes are saved; `None` keeps them in memory (tests, or a
    /// file this Tern mustn't overwrite).
    file: Option<PathBuf>,
    /// One line about the vault file: why it couldn't be read or saved.
    notice: Option<SharedString>,
    selected: String,
    live: LiveSlots,
    /// The hosts screen's list, what it lists as of the last render, and
    /// how many cards that put on a line, for the width its list last had.
    hosts_list: ListState,
    items: Vec<Item>,
    per_line: usize,
    hosts_width: Rc<Cell<Pixels>>,
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
    /// The commands filling groups here, and where they're kept (nowhere
    /// for tests). They never sync.
    sources: Sources,
    sources_file: Option<PathBuf>,
    source_form: Option<SourceForm>,
    /// Commands waiting for hosts to come back (by host id), where they're
    /// kept, and the line for one being typed (the host's id and field).
    waiting: Waiting,
    waiting_file: Option<PathBuf>,
    wait_line: Option<(String, Entity<InputState>, Subscription)>,
    /// The list of vaults is open under the switcher; the line for a
    /// vault's name being typed there; the vault asked about before it's
    /// deleted.
    vaults_open: bool,
    vault_line: Option<(VaultLine, Entity<InputState>, Subscription)>,
    deleting: Option<String>,
    /// Groups whose command is running, and why a group's failed last.
    refreshing: HashSet<String>,
    source_problems: HashMap<String, SharedString>,
    /// Hosts in `~/.ssh/config` not in the vault yet, likewise.
    openssh: Vec<Host>,
    /// What the background checks found.
    monitor: Monitor,
    /// The vault is the tab on screen; checks run only then.
    showing: bool,
    /// Whether the last look for due checks ran them, and whether only
    /// those of hosts a command waits for: a check that ends starts the
    /// next due the same way, without waiting for the next look.
    checking: Option<bool>,
    /// That look, soon after a check ends, and where the last look found
    /// the places to check.
    filling: Option<Task<()>>,
    places: Option<Places>,
    _checks: Option<Task<()>>,
    _subscription: Subscription,
    _editor_events: Option<Subscription>,
    _screen_events: Vec<Subscription>,
    /// The window, for applying synced settings after background work.
    window: AnyWindowHandle,
    /// Seals and opens sync files, and the passphrase it was made with.
    syncer: Option<(String, Arc<Mutex<Syncer>>)>,
    /// Drawing the hosts' key art, off the UI thread.
    _keys: Option<Task<()>>,
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
        let id = vaults::current(cx);
        let opened = tern_core::vault::open(&paths::vault(&id));
        let mut vault = opened.vault;
        if opened.fresh && id == FIRST && Path::new(DEV_KEY).exists() {
            vault.groups.insert(0, dev_server_group());
        }
        let mut content = Self::new(vault, opened.path, live, window, cx);
        content.notice = opened.notice.map(Into::into);
        content.history = History::load(&paths::history(&id));
        content.history_file = Some(paths::history(&id));
        content.sources = Sources::load(&paths::sources(&id));
        content.sources_file = Some(paths::sources(&id));
        content.waiting = Waiting::load(&paths::waiting(&id));
        content.waiting_file = Some(paths::waiting(&id));
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

    /// The connections that open `open`'s tabs again, and where the tab
    /// on screen ends up.
    pub fn reopened(&self, open: &OpenTabs, cx: &App) -> (Vec<Vec<Connection>>, Option<usize>) {
        // Other vaults are read only for tabs this one doesn't have.
        let here = [&self.vault];
        let elsewhere = open.tabs.iter().flatten().any(|pane| {
            matches!(pane, tern_core::restore::Opened::Host { .. })
                && pane.connection(&here).is_none()
        });
        if !elsewhere {
            return open.connections(&here);
        }
        let others: Vec<Vault> = vaults::list(cx)
            .vaults
            .iter()
            .filter(|vault| vault.id != vaults::current(cx))
            .filter_map(|vault| tern_core::vault::read(&paths::vault(&vault.id)))
            .collect();
        let all: Vec<&Vault> = std::iter::once(&self.vault).chain(&others).collect();
        open.connections(&all)
    }

    /// Every saved host's name and `user@address`, in vault order.
    pub fn host_entries(&self) -> Vec<(String, String)> {
        self.vault
            .hosts()
            .map(|host| (host.name.clone(), host.target()))
            .collect()
    }

    /// Connects to the saved host `name`, as its card's arrow does.
    pub fn open_host(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = name.to_string();
        self.connect_saved(name, window, cx);
    }

    /// Gives the vault the keyboard, for moving through hosts.
    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.focus.focus(window, cx);
    }

    /// Switches the screen beside the sidebar. Screens that show files
    /// read them again, for changes made meanwhile.
    /// Shows the screen called `name` (`keychain`, `identities`,
    /// `snippets`, `forwarding`, `known-hosts`, `history`; anything else is
    /// Hosts, `vaults` with the list of vaults open).
    pub fn show_named(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.show_identities = name == "identities";
        self.vaults_open = name == "vaults";
        let section = match name {
            "keychain" | "identities" => Section::Keychain,
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
            Section::Hosts => self.refresh_keys(cx),
            Section::Keychain if self.show_identities => match &self.identities {
                Some(view) => {
                    let identities = self.vault.identities.clone();
                    view.update(cx, |view, cx| view.refresh(identities, cx));
                }
                None => {
                    let identities = self.vault.identities.clone();
                    let view = cx.new(|_| IdentitiesView::new(identities));
                    self._screen_events.push(cx.subscribe_in(
                        &view,
                        window,
                        Self::on_identity_event,
                    ));
                    self.identities = Some(view);
                }
            },
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
                    let alone = self.running_alone();
                    let view = cx.new(|_| ForwardsView::new(hosts, states, alone));
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
    /// vault shows in the active window; those of hosts a command waits
    /// for, always.
    pub(crate) fn start_checks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._checks = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                let alive = this.update_in(cx, |this, window, cx| {
                    let on_screen = this.showing && this.section == Section::Hosts;
                    this.checking = if on_screen && window.is_window_active() {
                        Some(false)
                    } else if !this.waiting.is_empty() {
                        // A host something waits for is watched regardless.
                        Some(true)
                    } else {
                        None
                    };
                    if let Some(waited) = this.checking {
                        this.run_due_checks(waited, true, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
        }));
    }

    /// Checks the hosts due a check: all of them, or `waited` only those
    /// a command waits for. The look once a second works out where they
    /// are (`fresh`); a slot freeing in between goes by that.
    fn run_due_checks(&mut self, waited: bool, fresh: bool, cx: &mut Context<Self>) {
        if fresh
            || self
                .places
                .as_ref()
                .is_none_or(|places| places.waited != waited)
        {
            self.places = Some(self.places(waited));
        }
        let Some(places) = &self.places else {
            return;
        };
        let due = self
            .monitor
            .due_among(&places.all, &places.wanted, Instant::now());
        if due.is_empty() {
            return;
        }
        for place in due {
            let proxy = places.proxies.get(&place).cloned().flatten();
            let check = probe(&place.0, place.1, proxy);
            cx.spawn(async move |this, cx| {
                let latency_ms = check.await.ok().map(|reached| reached.latency_ms);
                let _ = this.update(cx, |this, cx| {
                    this.monitor.record(&place, latency_ms, Instant::now());
                    if latency_ms.is_some() {
                        this.run_waiting(&place, cx);
                    }
                    // Its slot goes to the next due soon, not at the next
                    // look; checks ending together share one look.
                    if this.checking.is_some() && this.filling.is_none() {
                        this.filling = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(FILL_AFTER).await;
                            let _ = this.update(cx, |this, cx| {
                                this.filling = None;
                                if let Some(waited) = this.checking {
                                    this.run_due_checks(waited, false, cx);
                                }
                            });
                        }));
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        // A first check shows as connecting.
        cx.notify();
    }

    /// Where the vault's hosts are checked: each place once, in vault
    /// order, with the proxy its first hop dials through; `waited`, only
    /// those of hosts a command waits for are wanted.
    fn places(&self, waited: bool) -> Places {
        let hops = self.vault.hops();
        let mut places = Places {
            waited,
            all: Vec::new(),
            wanted: Vec::new(),
            proxies: HashMap::new(),
        };
        let mut wanted = HashSet::new();
        for host in self.vault.hosts() {
            let hop = hops.first_hop_host(host);
            let place = (hop.address.clone(), hop.port);
            if (!waited || self.waiting.command(&host.id).is_some()) && wanted.insert(place.clone())
            {
                places.wanted.push(place.clone());
            }
            if !places.proxies.contains_key(&place) {
                places.proxies.insert(place.clone(), hop.proxy.clone());
                places.all.push(place);
            }
        }
        places
    }

    /// The hosts the search and the filter let through, with their group,
    /// in vault order.
    fn visible(&self) -> Vec<(&str, &Host)> {
        let hops = self.vault.hops();
        self.vault
            .groups
            .iter()
            .flat_map(|group| {
                group
                    .hosts
                    .iter()
                    .map(move |host| (group.name.as_str(), host))
            })
            .filter(|(group, host)| self.lets_through(group, host, &hops))
            .collect()
    }

    /// Whether the search and the filter let `host`, of `group`, through.
    fn lets_through<'a>(&self, group: &str, host: &'a Host, hops: &Hops<'a>) -> bool {
        host.matches(&self.query)
            && self
                .filter
                .matches(group, host, self.monitor.status(&hops.first_hop(host)))
    }

    /// What the hosts screen lists: the hosts the search and the filter
    /// let through, by group, `per_line` cards to a line.
    fn items(&self, per_line: usize) -> Vec<Item> {
        let mut items = vec![Item::Top {
            form: self.source_form.as_ref().map(|form| form.problem.is_some()),
            notice: self.notice.is_some(),
            destination: self.destination.is_some(),
        }];
        let hops = self.vault.hops();
        let cards = self.layout == Layout::Cards;
        for (index, group) in self.vault.groups.iter().enumerate() {
            let shown: Vec<usize> = (0..group.hosts.len())
                .filter(|&at| self.lets_through(&group.name, &group.hosts[at], &hops))
                .collect();
            if shown.is_empty() {
                continue;
            }
            let lines: Vec<&[usize]> = shown.chunks(if cards { per_line } else { 1 }).collect();
            let count = lines.len();
            items.push(Item::Heading {
                group: index,
                shown: shown.clone(),
                sourced: self.sources.command(&group.name).is_some(),
                problem: self.source_problems.contains_key(&group.name),
            });
            items.extend(lines.into_iter().enumerate().map(|(n, line)| Item::Hosts {
                group: index,
                hosts: line.to_vec(),
                cards,
                last: n + 1 == count,
            }));
        }
        items
    }

    /// Brings the list up to `items`. Only the rows between those that
    /// stayed the same at either end are measured again, and the scroll
    /// stays where it was.
    fn set_items(&mut self, items: Vec<Item>) {
        let old = &self.items;
        let same = old.iter().zip(&items).take_while(|(a, b)| a == b).count();
        let same_end = old[same..]
            .iter()
            .rev()
            .zip(items[same..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        if same < old.len() || same < items.len() {
            self.hosts_list
                .splice(same..old.len() - same_end, items.len() - same - same_end);
        }
        self.items = items;
    }

    /// Arrows move the selection, Enter connects, E edits; only while the
    /// hosts screen itself has focus, not a field on it.
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window) || self.editor.is_some() || self.section != Section::Hosts
        {
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

    /// The saved hosts, groups, snippets and identities.
    pub fn vault(&self) -> &Vault {
        &self.vault
    }

    /// What the last check of `host`'s first hop found.
    pub fn status_of(&self, host: &Host) -> HostStatus {
        self.monitor.status(&self.vault.first_hop(host))
    }

    /// Saves `host` into `group`, in place of the host called `replacing`
    /// when it's a change: from the editor, or from an agent.
    pub fn save_host(
        &mut self,
        replacing: Option<&str>,
        host: Host,
        group: &str,
        cx: &mut Context<Self>,
    ) {
        let name = host.name.clone();
        self.vault.save(replacing, host, group);
        // Rules run without a terminal go by the host's name.
        if let Some(old) = replacing.filter(|old| *old != name) {
            self.end_alone(old, cx);
        }
        self.persist(cx);
        self.refresh_keys(cx);
        // A host changed to match an import makes it redundant.
        self.refresh_imports();
        cx.notify();
    }

    /// Deletes the saved host `name`, with its saved password. Refused
    /// while other hosts go through it; the error names them.
    pub fn delete_host(&mut self, name: &str, cx: &mut Context<Self>) -> Result<(), Vec<String>> {
        let id = self.vault.find(name).map(|host| host.id.clone());
        self.vault.delete(name)?;
        self.end_alone(name, cx);
        if let Some(id) = id {
            let _ = sync::secrets(cx).forget(&tern_core::secrets::host_password(&id));
        }
        self.persist(cx);
        if self.selected == name {
            self.selected = self
                .vault
                .hosts()
                .next()
                .map(|host| host.name.clone())
                .unwrap_or_default();
        }
        // A deleted import can come back.
        self.refresh_imports();
        cx.notify();
        Ok(())
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
            show_identities: false,
            copied: None,
            _copied: None,
            identities: None,
            snippets: None,
            forwards: None,
            known_hosts: None,
            history_view: None,
            history: History::default(),
            history_file: None,
            forward_states: States::default(),
            background: HashMap::new(),
            open_sessions: HashMap::new(),
            sessions_by_window: HashMap::new(),
            vault,
            file,
            notice: None,
            selected,
            live,
            hosts_list: ListState::new(0, ListAlignment::Top, px(300.)),
            items: Vec::new(),
            per_line: cards_per_line(px(0.)),
            hosts_width: Rc::default(),
            keys: HashMap::new(),
            search,
            query: SharedString::default(),
            destination: None,
            editor: None,
            putty: Vec::new(),
            sources: Sources::default(),
            sources_file: None,
            source_form: None,
            waiting: Waiting::default(),
            waiting_file: None,
            wait_line: None,
            vaults_open: false,
            vault_line: None,
            deleting: None,
            refreshing: HashSet::new(),
            source_problems: HashMap::new(),
            openssh: Vec::new(),
            monitor: Monitor::default(),
            showing: true,
            checking: None,
            filling: None,
            places: None,
            _checks: None,
            _subscription: subscription,
            _editor_events: None,
            _screen_events: Vec::new(),
            window: window.window_handle(),
            syncer: None,
            _keys: None,
            _sync: None,
            _sync_soon: None,
            _sync_loop: None,
        };
        content.refresh_keys(cx);
        content.refresh_imports();
        content
    }

    /// Opens the vault `id` in place of this one.
    fn open_vault(&mut self, id: String, cx: &mut Context<Self>) {
        vaults::update(cx, |vaults| vaults.current = id);
        cx.emit(VaultEvent::Switch);
    }

    /// Opens the switcher's line for a new vault's name, or the open
    /// vault's.
    fn open_vault_line(&mut self, line: VaultLine, window: &mut Window, cx: &mut Context<Self>) {
        let name = match line {
            VaultLine::New => String::new(),
            VaultLine::Rename => vaults::list(cx).current().name.clone(),
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Name")
                .default_value(name)
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        let enter = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.take_vault_line(cx);
            }
        });
        self.deleting = None;
        self.vault_line = Some((line, input, enter));
        cx.notify();
    }

    /// Makes the vault named on the line and opens it, or renames the
    /// open one.
    fn take_vault_line(&mut self, cx: &mut Context<Self>) {
        let Some((line, input, _)) = self.vault_line.take() else {
            return;
        };
        let name = input.read(cx).value().to_string();
        match line {
            VaultLine::New => {
                let mut id = String::new();
                vaults::update(cx, |vaults| id = vaults.add(&name));
                self.open_vault(id, cx);
            }
            VaultLine::Rename => {
                let open = vaults::current(cx);
                vaults::update(cx, |vaults| vaults.rename(&open, &name));
            }
        }
        cx.notify();
    }

    /// Deletes the vault `id`, which isn't open (nor the first): its
    /// files, its sync passphrase and its hosts' saved passwords. Devices
    /// it synced with keep their copies.
    fn delete_vault(&mut self, id: String, cx: &mut Context<Self>) {
        self.deleting = None;
        if id == vaults::current(cx) {
            return;
        }
        let mut removed = false;
        vaults::update(cx, |vaults| removed = vaults.remove(&id));
        if !removed {
            return;
        }
        let store = sync::secrets(cx);
        let _ = store.forget(&secrets::sync_passphrase(&id));
        if let Some(deleted) = tern_core::vault::read(&paths::vault(&id)) {
            for host in deleted.hosts() {
                let _ = store.forget(&secrets::host_password(&host.id));
            }
        }
        let _ = std::fs::remove_dir_all(paths::vault_dir(&id));
        cx.notify();
    }

    /// The open vault's name, which opens the list of vaults under it:
    /// each opens with a click, the open one renames, others delete, and a
    /// new one starts at the end.
    fn vault_switcher(&self, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let vaults = vaults::list(cx);
        let open = vaults.current().clone();
        let header = div()
            .id("vaults")
            .debug_selector(|| "vaults".into())
            .h(px(42.))
            .px(px(14.))
            .flex()
            .items_center()
            .gap(px(12.))
            .rounded(px(14.))
            .border_1()
            .border_color(t.line)
            .cursor_pointer()
            .child(icon(IconName::Vault, 18., t.text))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(open.name.clone()),
            )
            .child(icon(IconName::ChevronsUpDown, 14., t.text2))
            .on_click(cx.listener(|this, _, _, cx| {
                this.vaults_open = !this.vaults_open;
                this.vault_line = None;
                this.deleting = None;
                cx.notify();
            }));
        let new_line = matches!(&self.vault_line, Some((VaultLine::New, ..)));
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(header)
            .when(self.vaults_open, |d| {
                d.children(
                    vaults
                        .vaults
                        .iter()
                        .map(|vault| self.vault_row(vault, vault.id == open.id, t, cx)),
                )
                .child(if new_line {
                    self.vault_name_line(t, cx)
                } else {
                    div()
                        .id("vault-new")
                        .debug_selector(|| "vault-new".into())
                        .h(px(36.))
                        .px(px(14.))
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .rounded(px(12.))
                        .text_color(t.text2)
                        .font_weight(FontWeight::MEDIUM)
                        .cursor_pointer()
                        .hover(|d| d.bg(t.card2))
                        .child(icon(IconName::Plus, 16., t.text2))
                        .child("New vault")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_vault_line(VaultLine::New, window, cx)
                        }))
                        .into_any_element()
                })
            })
    }

    /// A vault in the switcher's list: the open one as a card that
    /// renames, others to open with a click or delete.
    fn vault_row(
        &self,
        vault: &VaultEntry,
        open: bool,
        t: &Theme,
        cx: &Context<Self>,
    ) -> AnyElement {
        if open && matches!(&self.vault_line, Some((VaultLine::Rename, ..))) {
            return self.vault_name_line(t, cx);
        }
        let id = vault.id.clone();
        let tag = format!("vault-{}", vault.name);
        let name = div()
            .flex_1()
            .min_w_0()
            .truncate()
            .child(vault.name.clone());
        if self.deleting.as_deref() == Some(id.as_str()) {
            let (keep, gone) = (id.clone(), id);
            return div()
                .h(px(36.))
                .pl(px(14.))
                .pr(px(4.))
                .flex()
                .items_center()
                .gap(px(6.))
                .rounded(px(12.))
                .border_1()
                .border_color(t.danger)
                .child(name)
                .child(
                    div()
                        .id("vault-delete-confirm")
                        .debug_selector(|| "vault-delete-confirm".into())
                        .h(px(28.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .rounded(px(9.))
                        .text_size(px(12.5))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.danger)
                        .cursor_pointer()
                        .child("Delete")
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.delete_vault(gone.clone(), cx)),
                        ),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("vault-keep-{keep}")))
                        .size(px(28.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(9.))
                        .cursor_pointer()
                        .hover(|d| d.bg(t.card2))
                        .child(icon(IconName::Close, 12., t.text2))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.deleting = None;
                            cx.notify();
                        })),
                )
                .into_any_element();
        }
        let action = if open {
            Some(("vault-rename".to_string(), IconName::Pencil))
        } else if id != FIRST {
            Some((format!("vault-delete-{}", vault.name), IconName::Trash))
        } else {
            None
        };
        div()
            .id(SharedString::from(tag.clone()))
            .debug_selector(move || tag)
            .h(px(36.))
            .pl(px(14.))
            .pr(px(4.))
            .flex()
            .items_center()
            .gap(px(6.))
            .rounded(px(12.))
            .map(|d| {
                if open {
                    d.bg(t.card)
                        .shadow(t.shadow())
                        .font_weight(FontWeight::SEMIBOLD)
                } else {
                    d.text_color(t.text2)
                        .font_weight(FontWeight::MEDIUM)
                        .cursor_pointer()
                        .hover(|d| d.bg(t.card2))
                }
            })
            .child(name)
            .when_some(action, |d, (action, glyph)| {
                let (id, selector) = (id.clone(), action.clone());
                d.child(
                    div()
                        .id(SharedString::from(action))
                        .debug_selector(move || selector)
                        .size(px(28.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(9.))
                        .cursor_pointer()
                        .hover(|d| d.bg(t.card2))
                        .child(icon(glyph, 13., t.text2))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            if open {
                                this.open_vault_line(VaultLine::Rename, window, cx);
                            } else {
                                this.vault_line = None;
                                this.deleting = Some(id.clone());
                                cx.notify();
                            }
                        })),
                )
            })
            .when(!open, |d| {
                d.on_click(cx.listener(move |this, _, _, cx| this.open_vault(id.clone(), cx)))
            })
            .into_any_element()
    }

    /// The switcher's line for a vault's name, with a way out.
    fn vault_name_line(&self, t: &Theme, cx: &Context<Self>) -> AnyElement {
        let Some((_, input, _)) = &self.vault_line else {
            return div().into_any_element();
        };
        div()
            .h(px(36.))
            .pl(px(12.))
            .pr(px(4.))
            .flex()
            .items_center()
            .gap(px(6.))
            .rounded(px(12.))
            .bg(t.card2)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(input).appearance(false)),
            )
            .child(
                div()
                    .id("vault-line-ok")
                    .debug_selector(|| "vault-line-ok".into())
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(9.))
                    .bg(t.text)
                    .cursor_pointer()
                    .child(icon(IconName::Check, 13., t.card))
                    .on_click(cx.listener(|this, _, _, cx| this.take_vault_line(cx))),
            )
            .child(
                div()
                    .id("vault-line-cancel")
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(9.))
                    .cursor_pointer()
                    .hover(|d| d.bg(t.card2))
                    .child(icon(IconName::Close, 12., t.text2))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.vault_line = None;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// Runs what waited for the hosts reached through `place`, which
    /// answered: each in a tab of its own, opened out of the way.
    fn run_waiting(&mut self, place: &Place, cx: &mut Context<Self>) {
        let back: Vec<Host> = self
            .vault
            .hosts()
            .filter(|host| self.vault.first_hop(host) == *place)
            .filter(|host| self.waiting.command(&host.id).is_some())
            .cloned()
            .collect();
        if back.is_empty() {
            return;
        }
        for host in back {
            let Some(command) = self.waiting.take(&host.id) else {
                continue;
            };
            if let Ok(mut connection) = self.vault.connection(&host) {
                connection.once = Some(command);
                cx.emit(VaultEvent::Run(connection));
            }
        }
        self.save_waiting();
        cx.notify();
    }

    /// Opens the line for a command to run once the host `id` is back.
    fn open_wait_line(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Command"));
        input.update(cx, |input, cx| input.focus(window, cx));
        let enter = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.queue_wait(cx);
            }
        });
        self.wait_line = Some((id, input, enter));
        cx.notify();
    }

    /// Has the line's command wait for its host.
    fn queue_wait(&mut self, cx: &mut Context<Self>) {
        if let Some((id, input, _)) = self.wait_line.take() {
            let command = input.read(cx).value().to_string();
            self.waiting.set(&id, &command);
            self.save_waiting();
        }
        cx.notify();
    }

    fn save_waiting(&self) {
        if let Some(file) = &self.waiting_file {
            // Not written down, it only doesn't wait past a restart.
            paths::write_json_soon(file, &self.waiting);
        }
    }

    /// For a host that doesn't answer: Run when back, its line while a
    /// command is typed, or the command waiting with a way to drop it.
    fn waiting_row(&self, host: &Host, t: &Theme, cx: &Context<Self>) -> Option<AnyElement> {
        let id = host.id.clone();
        if let Some((typing, input, _)) = &self.wait_line
            && *typing == id
        {
            return Some(
                div()
                    .h(px(38.))
                    .pl(px(12.))
                    .pr(px(4.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(11.))
                    .bg(t.card2)
                    .child(icon(IconName::Clock, 13., t.text2))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(MONO)
                            .text_size(px(12.))
                            .child(Input::new(input).appearance(false)),
                    )
                    .child(
                        ink_button("wait-queue", t)
                            .debug_selector(|| "wait-queue".into())
                            .h(px(30.))
                            .px(px(12.))
                            .rounded(px(9.))
                            .text_size(px(12.5))
                            .child("Queue")
                            .on_click(cx.listener(|this, _, _, cx| this.queue_wait(cx))),
                    )
                    .into_any_element(),
            );
        }
        if let Some(command) = self.waiting.command(&id) {
            let command = command.to_string();
            return Some(
                div()
                    .h(px(38.))
                    .pl(px(12.))
                    .pr(px(4.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(11.))
                    .border_1()
                    .border_color(t.line)
                    .child(icon(IconName::Clock, 13., t.text))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(MONO)
                            .text_size(px(12.))
                            .truncate()
                            .child(command),
                    )
                    .child(
                        div()
                            .id("wait-cancel")
                            .debug_selector(|| "wait-cancel".into())
                            .size(px(28.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(9.))
                            .cursor_pointer()
                            .hover(|d| d.bg(t.card2))
                            .child(icon(IconName::Close, 12., t.text2))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.waiting.set(&id, "");
                                this.save_waiting();
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        let place = self.vault.first_hop(host);
        (self.monitor.status(&place) == HostStatus::Offline).then(|| {
            button("wait", t)
                .debug_selector(|| "wait".into())
                .justify_center()
                .h(px(34.))
                .rounded(px(11.))
                .text_size(px(12.5))
                .child(icon(IconName::Clock, 13., t.text2))
                .child("Run when back")
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_wait_line(id.clone(), window, cx)
                }))
                .into_any_element()
        })
    }

    /// Opens the line for filling a group from a command: `group`'s, with
    /// its command, or a new one.
    fn open_source(&mut self, group: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let name = group.unwrap_or_default();
        let command = self.sources.command(&name).unwrap_or_default().to_string();
        let new = name.is_empty();
        let group = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Group")
                .default_value(name)
        });
        let command = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Command")
                .default_value(command)
        });
        let first = if new { &group } else { &command };
        first.update(cx, |input, cx| input.focus(window, cx));
        let enter = cx.subscribe_in(&command, window, |this, _, event: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.fill_from_form(cx);
            }
        });
        self.source_form = Some(SourceForm {
            group,
            command,
            problem: None,
            _run: None,
            _enter: enter,
        });
        cx.notify();
    }

    /// Runs the line's command and fills its group with the hosts it
    /// prints, keeping the command for refreshes; an empty command stops
    /// the group being filled.
    fn fill_from_form(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.source_form else {
            return;
        };
        let group = form.group.read(cx).value().trim().to_string();
        let command = form.command.read(cx).value().trim().to_string();
        if group.is_empty() {
            form.problem = Some("A group needs a name".into());
            return cx.notify();
        }
        if command.is_empty() {
            self.sources.set(&group, "");
            self.save_sources();
            self.source_form = None;
            return cx.notify();
        }
        let run = cx.background_spawn({
            let command = command.clone();
            async move { sources::run(&command, sources::WAIT) }
        });
        form.problem = None;
        form._run = Some(cx.spawn(async move |this, cx| {
            let printed = run.await;
            let _ = this.update(cx, |this, cx| {
                match this.refill(&group, &command, printed, cx) {
                    Ok(()) => this.source_form = None,
                    Err(problem) => {
                        if let Some(form) = &mut this.source_form {
                            form.problem = Some(problem.into());
                            form._run = None;
                        }
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Runs `group`'s command again and keeps the group to what it prints.
    fn refresh_source(&mut self, group: String, cx: &mut Context<Self>) {
        let Some(command) = self.sources.command(&group).map(str::to_string) else {
            return;
        };
        if !self.refreshing.insert(group.clone()) {
            return;
        }
        let run = cx.background_spawn({
            let command = command.clone();
            async move { sources::run(&command, sources::WAIT) }
        });
        cx.spawn(async move |this, cx| {
            let printed = run.await;
            let _ = this.update(cx, |this, cx| {
                this.refreshing.remove(&group);
                match this.refill(&group, &command, printed, cx) {
                    Ok(()) => this.source_problems.remove(&group),
                    Err(problem) => this.source_problems.insert(group, problem.into()),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Fills `group` with the hosts a run of `command` printed, in
    /// OpenSSH's config form, and keeps the command for it.
    fn refill(
        &mut self,
        group: &str,
        command: &str,
        printed: Result<String, String>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let printed = printed?;
        let dir = paths::ssh_dir().unwrap_or_default();
        let user = std::env::var("USERNAME")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_default();
        let hosts = openssh::described(&printed, &dir, &user);
        sources::refill(&mut self.vault, group, hosts)?;
        self.sources.set(group, command);
        self.save_sources();
        if self.vault.find(&self.selected).is_none() {
            self.selected = self
                .vault
                .hosts()
                .next()
                .map(|host| host.name.clone())
                .unwrap_or_default();
        }
        self.persist(cx);
        self.refresh_keys(cx);
        Ok(())
    }

    fn save_sources(&self) {
        if let Some(file) = &self.sources_file {
            // Not written down, a source only isn't there next time.
            paths::write_json_soon(file, &self.sources);
        }
    }

    /// The line for filling a group from a command: its group and command,
    /// Fill and Cancel, and why the last run failed below.
    fn source_line(
        &self,
        form: &SourceForm,
        theme: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let running = form._run.is_some();
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .h(px(46.))
                    .pl(px(14.))
                    .pr(px(5.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .rounded(px(16.))
                    .bg(theme.card)
                    .border_1()
                    .border_color(if form.problem.is_some() {
                        theme.danger
                    } else {
                        theme.line
                    })
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "escape" {
                            this.source_form = None;
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .child(icon(IconName::SquareTerminal, 16., theme.text2))
                    .child(
                        div()
                            .w(px(160.))
                            .flex_none()
                            .text_size(px(13.))
                            .child(Input::new(&form.group).appearance(false)),
                    )
                    .child(div().w(px(1.)).h(px(22.)).bg(theme.line))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(MONO)
                            .text_size(px(12.5))
                            .child(Input::new(&form.command).appearance(false)),
                    )
                    .child(
                        ink_button("fill-source", theme)
                            .debug_selector(|| "fill-source".into())
                            .h(px(36.))
                            .px(px(16.))
                            .rounded(px(12.))
                            .text_size(px(13.))
                            .map(|d| {
                                if running {
                                    d.child(icon(IconName::LoaderCircle, 15., theme.card))
                                } else {
                                    d.child("Fill")
                                }
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.fill_from_form(cx))),
                    )
                    .child(
                        button("cancel-source", theme)
                            .debug_selector(|| "cancel-source".into())
                            .h(px(36.))
                            .px(px(14.))
                            .rounded(px(12.))
                            .text_size(px(13.))
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.source_form = None;
                                cx.notify();
                            })),
                    ),
            )
            .when_some(form.problem.clone(), |d, problem| {
                d.child(
                    div()
                        .px(px(4.))
                        .text_size(px(12.))
                        .text_color(theme.danger)
                        .truncate()
                        .child(problem),
                )
            })
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
        let added = hosts.len();
        self.vault.import(hosts, group);
        // The first one added, under the name it got.
        if let Some(first) = self
            .vault
            .groups
            .iter()
            .find(|have| have.name == group)
            .and_then(|group| group.hosts.get(group.hosts.len().checked_sub(added)?))
        {
            self.selected = first.name.clone();
        }
        self.persist(cx);
        self.refresh_keys(cx);
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

    /// Saves a change made here, and syncs it shortly.
    fn persist(&mut self, cx: &mut Context<Self>) {
        self.write_vault(cx);
        if vaults::sync_settings(cx).folder.is_some() {
            self._sync_soon = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(SYNC_SETTLE).await;
                let _ = this.update(cx, |this, cx| this.sync_now(cx));
            }));
        }
    }

    /// Syncs now, if a folder and passphrase are set and no sync is
    /// running already.
    pub fn sync_now(&mut self, cx: &mut Context<Self>) {
        let settings = vaults::sync_view(cx);
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
        let Some(passphrase) = sync::secrets(cx).load(&vaults::passphrase(cx)) else {
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
        let vault = self.vault.clone();
        let work = cx
            .background_executor()
            .spawn(async move { sync::run(&syncer, &vault, &settings) });
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
    fn finish_sync(&mut self, result: Result<Synced, String>, cx: &mut Context<Self>) {
        let Synced {
            merged,
            devices,
            relink,
            refused,
        } = match result {
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
        let settings = vaults::sync_view(cx);
        // Changes made here while the sync ran are newer than what it read.
        let synced = tern_core::sync::snapshot(&merged.vault, &settings, Vec::new());
        let mut vault = tern_core::sync::merge(&self.vault, &settings, &[synced]).vault;
        vault.relink_keys(&relink);
        if vault != self.vault {
            self.vault = vault;
            self.write_vault(cx);
            if self.vault.find(&self.selected).is_none() {
                self.selected = self
                    .vault
                    .hosts()
                    .next()
                    .map(|host| host.name.clone())
                    .unwrap_or_default();
            }
            self.refresh_keys(cx);
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
        sync::set_state(
            cx,
            SyncState {
                status: match refused {
                    None => Status::Synced(tern_core::host::now_ms()),
                    Some(reason) => Status::Failed(reason.into()),
                },
                devices,
            },
        );
        cx.notify();
    }

    /// Writes the vault to its file, if it has one, on a thread of its own
    /// so an edit never waits for the disk. A failure shows once it's
    /// known, until a later save works.
    fn write_vault(&mut self, cx: &mut Context<Self>) {
        self.filter.forget_missing(&self.vault);
        let Some(path) = self.file.clone() else {
            return;
        };
        // Its answer is waited for in the background too, so that the
        // writing thread only ever hands it over (and tests stay in step).
        let written = self.vault.save_soon(&path);
        let written = cx.background_spawn(async move { written.wait() });
        cx.spawn(async move |this, cx| {
            let written = written.await;
            let _ = this.update(cx, |this, cx| {
                this.notice = written
                    .err()
                    .map(|error| format!("Can't save {}: {error}", path.display()).into());
                cx.notify();
            });
        })
        .detach();
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
    pub fn refresh_keys(&mut self, cx: &mut Context<Self>) {
        let mut places: Vec<(String, u16)> = self
            .vault
            .hosts()
            .map(|host| (host.address.clone(), host.port))
            .collect();
        places.sort();
        places.dedup();
        // Off the UI thread, each known_hosts file read once: hashed names
        // take a hash per host and line, which adds up in big files.
        let work = cx.background_executor().spawn(async move {
            known_host_keys_for(&places)
                .into_iter()
                .map(|(place, key)| (place, key_art(&key)))
                .collect::<HashMap<_, _>>()
        });
        self._keys = Some(cx.spawn(async move |this, cx| {
            let keys = work.await;
            let _ = this.update(cx, |this, cx| {
                this.keys = keys;
                cx.notify();
            });
        }));
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
            let alone = self.running_alone();
            view.update(cx, |view, cx| view.refresh(hosts, states, alone, cx));
        }
    }

    /// The rules running without a terminal.
    fn running_alone(&self) -> Alone {
        self.background
            .iter()
            .flat_map(|(host, background)| {
                background
                    .rules
                    .iter()
                    .map(move |rule| (host.clone(), rule.id()))
            })
            .collect()
    }

    /// Runs `forward` of `host` now without a terminal (`on`), or stops it.
    /// The host's rules share one session without a shell, which closes
    /// with its last rule.
    /// Stops `forward` on `host` if it runs without a terminal.
    fn stop_alone(&mut self, host: &str, forward: &Forward, cx: &mut Context<Self>) {
        let running = self.background.get(host).is_some_and(|background| {
            background
                .rules
                .iter()
                .any(|rule| rule.id() == forward.id())
        });
        if running {
            self.run_alone(host, forward, false, cx);
        }
    }

    /// Ends `host`'s session without a terminal, if it has one.
    fn end_alone(&mut self, host: &str, cx: &mut Context<Self>) {
        if let Some(background) = self.background.remove(host) {
            background.input.close();
            for rule in background.rules {
                self.set_forward_state(host.to_string(), &rule, ForwardState::Stopped, cx);
            }
        }
    }

    fn run_alone(&mut self, host: &str, forward: &Forward, on: bool, cx: &mut Context<Self>) {
        if !on {
            if let Some(background) = self.background.get_mut(host) {
                background.input.forward(forward.clone(), false);
                background.rules.retain(|rule| rule.id() != forward.id());
                if background.rules.is_empty() {
                    background.input.close();
                    self.background.remove(host);
                }
            }
            self.set_forward_state(host.to_string(), forward, ForwardState::Stopped, cx);
            return;
        }
        if let Some(background) = self.background.get_mut(host) {
            background.input.forward(forward.clone(), true);
            background.rules.push(forward.clone());
            self.refresh_forwards(cx);
            return;
        }
        let Some(connection) = self
            .vault
            .find(host)
            .and_then(|saved| self.vault.connection(saved).ok())
        else {
            return;
        };
        // Saved passwords only: nothing here can ask for one.
        let store = sync::secrets(cx);
        let saved = |secret: Option<&str>| secret.and_then(|secret| store.load(secret));
        let password = (connection.auth == Auth::Password)
            .then(|| saved(connection.secret.as_deref()))
            .flatten();
        let mut request = connection.request(
            TermSize { cols: 80, rows: 24 },
            password.as_deref(),
            None,
            |jump| saved(jump.secret.as_deref()),
        );
        request.forwards = vec![forward.clone()];
        request.shell = false;
        let (input, mut events) = Session::open(request).into_parts();
        let name = host.to_string();
        let task = cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                let ended = matches!(
                    event,
                    Event::Closed { .. } | Event::Lost | Event::Failed(_) | Event::Prompts(_)
                );
                if this
                    .update(cx, |this, cx| this.on_background_event(&name, event, cx))
                    .is_err()
                    || ended
                {
                    break;
                }
            }
        });
        self.background.insert(
            host.to_string(),
            Background {
                input,
                rules: vec![forward.clone()],
                _events: task,
            },
        );
        self.refresh_forwards(cx);
    }

    /// What a session without a shell reports. It can't ask for anything,
    /// so a question or a refusal ends it, the reason on its rules.
    fn on_background_event(&mut self, host: &str, event: Event, cx: &mut Context<Self>) {
        let reason = match event {
            Event::Forwarding { forward, state } => {
                return self.set_forward_state(host.to_string(), &forward, state, cx);
            }
            Event::Failed(ConnectError::UnknownHostKey { .. }) => {
                Some("Connect once to trust its key".to_string())
            }
            Event::Failed(ConnectError::HostKeyChanged { .. }) => {
                Some("The host key changed".to_string())
            }
            Event::Failed(ConnectError::AuthFailed { .. }) | Event::Prompts(_) => {
                Some("Signing in needs a terminal".to_string())
            }
            Event::Failed(ConnectError::JumpRefused(message) | ConnectError::Other(message)) => {
                Some(message)
            }
            Event::Lost => Some("Connection lost".to_string()),
            Event::Closed { .. } => None,
            Event::Connected | Event::Output(_) => return,
        };
        let Some(background) = self.background.remove(host) else {
            return;
        };
        background.input.close();
        for rule in background.rules {
            let state = match &reason {
                Some(reason) => ForwardState::Failed(reason.clone()),
                None => ForwardState::Stopped,
            };
            self.set_forward_state(host.to_string(), &rule, state, cx);
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

    /// The session to `host` ended, and its rules with it; those running
    /// without a terminal go on.
    pub fn clear_forward_states(&mut self, host: &str, cx: &mut Context<Self>) {
        let alone = self.running_alone();
        self.forward_states
            .retain(|key, _| key.0 != host || alone.contains(key));
        self.refresh_forwards(cx);
    }

    /// Which saved hosts have a session open in the window `from`, with its
    /// preview; the vault shows those of every window.
    pub fn set_open_sessions(
        &mut self,
        from: EntityId,
        sessions: HashMap<String, Preview>,
        cx: &mut Context<Self>,
    ) {
        if sessions.is_empty() {
            self.sessions_by_window.remove(&from);
        } else {
            self.sessions_by_window.insert(from, sessions);
        }
        self.open_sessions = self
            .sessions_by_window
            .values()
            .flatten()
            .map(|(host, preview)| (host.clone(), preview.clone()))
            .collect();
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
                    )
                    // The host for OpenSSH: a command line, or a config block.
                    .child(
                        div()
                            .flex()
                            .gap(px(8.))
                            .child(self.copy_button("copy-command", "Copy command", host, t, cx))
                            .child(self.copy_button("copy-config", "Copy config", host, t, cx)),
                    )
                    .when_some(host.mac.clone(), |d, mac| {
                        d.child(self.wake_button(&mac, t, cx))
                    })
                    .children(self.waiting_row(host, t, cx)),
            )
            .when(!host.notes.is_empty(), |d| {
                d.child(
                    div()
                        .id("host-notes")
                        .debug_selector(|| "host-notes".into())
                        .p(px(14.))
                        .rounded(px(22.))
                        .bg(t.card)
                        .shadow(t.shadow())
                        .text_size(px(13.))
                        .text_color(t.text2)
                        .child(host.notes.clone()),
                )
            })
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
                    self.stop_alone(host, old, cx);
                    live.push((host.clone(), old.clone(), false));
                }
                live.push((host.clone(), forward.clone(), forward.on));
                view.update(cx, |view, cx| view.saved(cx));
            }
            ForwardEvent::Delete { host, forward } => {
                self.stop_alone(host, forward, cx);
                self.vault.delete_forward(host, &forward.id());
                live.push((host.clone(), forward.clone(), false));
            }
            ForwardEvent::Switch { host, forward, on } => {
                self.vault.switch_forward(host, &forward.id(), *on);
                live.push((host.clone(), forward.clone(), *on));
            }
            ForwardEvent::RunAlone { host, forward, on } => {
                self.run_alone(host, forward, *on, cx);
                cx.notify();
                return;
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
    pub fn record_visit(&mut self, visit: Visit, host_id: Option<&str>, cx: &mut Context<Self>) {
        // A session from another vault, still open after a switch.
        if let Some(id) = host_id
            && !self.vault.hosts().any(|host| host.id == id)
            && let Some(other) = vaults::list(cx).holding(id)
        {
            let path = paths::history(&other);
            let mut history = History::load(&path);
            history.record(visit);
            paths::write_json_soon(&path, &history);
            return;
        }
        self.history.record(visit);
        if let Some(path) = &self.history_file {
            // History is a convenience; failing to save it isn't worth a
            // message.
            paths::write_json_soon(path, &self.history);
        }
        if let Some(view) = &self.history_view {
            let visits = self.history.visits.clone();
            view.update(cx, |view, cx| view.refresh(visits, cx));
        }
    }

    /// The connections History holds, newest first.
    #[cfg(test)]
    pub fn visits(&self) -> &[Visit] {
        &self.history.visits
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

    /// Copies `host` for OpenSSH: `copy-command` as an `ssh` command line,
    /// `copy-config` as a `~/.ssh/config` block. Says "Copied" a moment.
    fn copy_button(
        &self,
        id: &'static str,
        label: &'static str,
        host: &Host,
        t: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let copied = self.copied == Some(id);
        let name = host.name.clone();
        button(id, t)
            .debug_selector(move || id.into())
            .flex_1()
            .justify_center()
            .h(px(34.))
            .rounded(px(11.))
            .text_size(px(12.5))
            .child(icon(
                if copied {
                    IconName::Check
                } else {
                    IconName::Copy
                },
                13.,
                t.text2,
            ))
            .child(if copied { "Copied" } else { label })
            .on_click(cx.listener(move |this, _, _, cx| {
                let Some(host) = this.vault.find(&name) else {
                    return;
                };
                let text = match id {
                    "copy-command" => openssh::ssh_command(&this.vault, host),
                    _ => openssh::config_block(&this.vault, host),
                };
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                this.copied = Some(id);
                this._copied = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(1500))
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        this.copied = None;
                        cx.notify();
                    });
                }));
                cx.notify();
            }))
    }

    /// Wakes the host over the local network; says "Sent" a moment, or why
    /// it couldn't.
    fn wake_button(&self, mac: &str, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let sent = self.copied == Some("wake");
        let mac = mac.to_string();
        button("wake", t)
            .debug_selector(|| "wake".into())
            .justify_center()
            .h(px(34.))
            .rounded(px(11.))
            .text_size(px(12.5))
            .child(icon(
                if sent {
                    IconName::Check
                } else {
                    IconName::Power
                },
                13.,
                t.text2,
            ))
            .child(if sent { "Sent" } else { "Wake" })
            .on_click(cx.listener(move |this, _, _, cx| {
                let woken =
                    tern_core::wol::parse_mac(&mac).and_then(|mac| tern_core::wol::wake(&mac));
                match woken {
                    Ok(()) => {
                        this.copied = Some("wake");
                        if this
                            .notice
                            .as_ref()
                            .is_some_and(|notice| notice.starts_with(WAKE_FAILED))
                        {
                            this.notice = None;
                        }
                        this._copied = Some(cx.spawn(async move |this, cx| {
                            cx.background_executor()
                                .timer(Duration::from_millis(1500))
                                .await;
                            let _ = this.update(cx, |this, cx| {
                                this.copied = None;
                                cx.notify();
                            });
                        }));
                    }
                    Err(problem) => this.notice = Some(format!("{WAKE_FAILED}: {problem}").into()),
                }
                cx.notify();
            }))
    }

    /// Opens the saved hosts `names` side by side, four panes to a tab.
    fn open_together(&mut self, names: &[String], cx: &mut Context<Self>) {
        let connections: Vec<Connection> = names
            .iter()
            .filter_map(|name| self.vault.find(name))
            .filter_map(|host| self.vault.connection(host).ok())
            .collect();
        cx.emit(VaultEvent::OpenTogether(connections));
    }

    /// The Keychain screen: a Keys / Identities switch over the one shown.
    fn keychain_screen(&self, t: &Theme, cx: &Context<Self>) -> AnyElement {
        let parts = [
            (false, "Keys", "keychain-keys"),
            (true, "Identities", "keychain-identities"),
        ];
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                div().flex().child(
                    div()
                        .flex()
                        .gap(px(4.))
                        .p(px(4.))
                        .rounded(px(14.))
                        .bg(t.card2)
                        .children(parts.into_iter().map(|(identities, label, id)| {
                            let on = self.show_identities == identities;
                            div()
                                .id(id)
                                .debug_selector(move || id.into())
                                .h(px(34.))
                                .px(px(16.))
                                .flex()
                                .items_center()
                                .rounded(px(11.))
                                .text_size(px(13.))
                                .font_weight(if on {
                                    FontWeight::SEMIBOLD
                                } else {
                                    FontWeight::MEDIUM
                                })
                                .text_color(if on { t.text } else { t.text2 })
                                .when(on, |d| d.bg(t.card).shadow(t.shadow_sm()))
                                .cursor_pointer()
                                .child(label)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.show_identities = identities;
                                    this.show(Section::Keychain, window, cx);
                                    cx.notify();
                                }))
                        })),
                ),
            )
            .child(div().flex_1().min_h_0().flex().map(|d| {
                match (self.show_identities, &self.identities, &self.keychain) {
                    (true, Some(view), _) => d.child(view.clone()),
                    (false, _, Some(view)) => d.child(view.clone()),
                    _ => d,
                }
            }))
            .into_any_element()
    }

    /// Saves or deletes an identity from the Keychain screen; a password
    /// goes to the system's credential store, never the vault file.
    fn on_identity_event(
        &mut self,
        view: &Entity<IdentitiesView>,
        event: &IdentityEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = sync::secrets(cx);
        match event {
            IdentityEvent::Save {
                identity,
                password,
                save_password,
            } => {
                let name = tern_core::identity::identity_password(&identity.id);
                if identity.auth == Auth::Password && *save_password {
                    if let Some(password) = password {
                        let _ = store.store(&name, password);
                    }
                } else {
                    let _ = store.forget(&name);
                }
                self.vault.save_identity(identity.clone());
            }
            IdentityEvent::Delete(id) => match self.vault.delete_identity(id) {
                Ok(()) => {
                    let _ = store.forget(&tern_core::identity::identity_password(id));
                }
                Err(users) => {
                    let verb = if users.len() == 1 { "uses" } else { "use" };
                    let problem = format!("{} {verb} this identity", users.join(", "));
                    view.update(cx, |view, cx| view.refuse(problem, cx));
                    return;
                }
            },
        }
        self.persist(cx);
        let identities = self.vault.identities.clone();
        view.update(cx, |view, cx| view.refresh(identities, cx));
        cx.notify();
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
            // Each host's session types the snippet once its shell first
            // starts, after the host's own command.
            SnippetEvent::Run { snippet, hosts } => {
                let connections: Vec<Connection> = hosts
                    .iter()
                    .filter_map(|name| self.vault.find(name))
                    .filter_map(|host| self.vault.connection(host).ok())
                    .map(|mut connection| {
                        connection.once = Some(snippet.line());
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
            connection.once = Some(keys::install_command(public));
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
                self.save_host(replacing.as_deref(), (**host).clone(), group, cx);
                if let Some((place, latency_ms)) = checked {
                    self.monitor
                        .record(place, Some(*latency_ms), Instant::now());
                }
                self.selected = host.name.clone();
            }
            EditorEvent::Delete(name) => {
                let _ = self.delete_host(name, cx);
            }
            EditorEvent::Close => {}
        }
        self.editor = None;
        self._editor_events = None;
        cx.notify();
    }

    /// The hosts screen: a list that builds only the rows on screen, so a
    /// vault of thousands costs what the hosts in view do. Laid out at a
    /// width that fits another number of cards to a line, it lines them up
    /// again.
    fn hosts(&self, cx: &Context<Self>) -> impl IntoElement {
        let (width, per_line, entity) = (self.hosts_width.clone(), self.per_line, cx.entity_id());
        let cards = self.layout == Layout::Cards;
        div()
            .id("hosts")
            .flex_1()
            .min_w_0()
            .h_full()
            .relative()
            .child(
                list(
                    self.hosts_list.clone(),
                    cx.processor(|this, ix, _, cx| this.host_item(ix, cx)),
                )
                .size_full(),
            )
            .child(
                canvas(
                    move |bounds, _, cx| {
                        width.set(bounds.size.width);
                        if cards && cards_per_line(bounds.size.width) != per_line {
                            cx.notify(entity);
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
    }

    /// Row `ix` of the hosts screen's list, built as it comes on screen.
    fn host_item(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme::current(cx);
        let ui = Ui {
            theme: &theme,
            live: &self.live,
        };
        match self.items.get(ix) {
            Some(Item::Top { .. }) => self.hosts_top(&ui, cx).into_any_element(),
            Some(Item::Heading { group, shown, .. }) => {
                self.heading(*group, shown, &ui, cx).into_any_element()
            }
            Some(Item::Hosts {
                group,
                hosts,
                cards,
                last,
            }) => {
                let group = &self.vault.groups[*group];
                let space = match (*last, *cards) {
                    (true, _) => 22.,
                    (false, true) => CARD_GAP,
                    (false, false) => 8.,
                };
                // Each row of the list as wide as the list, as it would be in a
                // column.
                let line = div().w_full().pb(px(space)).flex();
                if *cards {
                    line.gap(px(CARD_GAP))
                        .children(
                            hosts
                                .iter()
                                .map(|&at| self.host_card(&group.hosts[at], &ui, cx)),
                        )
                        .into_any_element()
                } else {
                    line.flex_col()
                        .children(
                            hosts
                                .iter()
                                .map(|&at| self.host_row(&group.hosts[at], &ui, cx)),
                        )
                        .into_any_element()
                }
            }
            None => div().into_any_element(),
        }
    }

    /// The top of the hosts screen: the toolbar, and under it an open
    /// group form, a notice and the typed destination.
    fn hosts_top(&self, ui: &Ui, cx: &Context<Self>) -> impl IntoElement {
        let theme = ui.theme;
        div()
            .w_full()
            .pb(px(22.))
            .flex()
            .flex_col()
            .gap(px(22.))
            .child(self.toolbar(theme, cx))
            .when_some(self.source_form.as_ref(), |d, form| {
                d.child(self.source_line(form, theme, cx))
            })
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
    }

    /// The heading of the group at `index`, whose hosts `shown` are on the
    /// screen.
    fn heading(
        &self,
        index: usize,
        shown: &[usize],
        ui: &Ui,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = ui.theme;
        let group = &self.vault.groups[index];
        // The hosts shown, opened side by side in one go.
        let together: Vec<String> = shown
            .iter()
            .map(|&at| group.hosts[at].name.clone())
            .collect();
        let open_all = (together.len() > 1).then(|| {
            let id = format!("open-group-{}", group.name);
            div()
                .id(SharedString::from(id.clone()))
                .debug_selector(move || id)
                .h(px(28.))
                .px(px(10.))
                .flex()
                .items_center()
                .gap(px(6.))
                .rounded(px(9.))
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text2)
                .cursor_pointer()
                .hover(|d| d.bg(theme.card2))
                .child(icon(IconName::LayoutGrid, 13., theme.text2))
                .child("Open all")
                .on_click(cx.listener(move |this, _, _, cx| this.open_together(&together, cx)))
        });
        // A group a command fills: Refresh runs it again (its
        // icon turning while it runs), the pencil opens it.
        let sourced = self.sources.command(&group.name).is_some().then(|| {
            let (refresh, change) = (
                format!("refresh-group-{}", group.name),
                format!("source-group-{}", group.name),
            );
            let (again, open) = (group.name.clone(), group.name.clone());
            let running = self.refreshing.contains(&group.name);
            let small = |id: String, glyph: IconName| {
                div()
                    .id(SharedString::from(id.clone()))
                    .debug_selector(move || id)
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(9.))
                    .cursor_pointer()
                    .hover(|d| d.bg(theme.card2))
                    .child(icon(glyph, 13., theme.text2))
            };
            div()
                .flex()
                .child(
                    small(
                        refresh,
                        if running {
                            IconName::LoaderCircle
                        } else {
                            IconName::RefreshCw
                        },
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.refresh_source(again.clone(), cx)),
                    ),
                )
                .child(small(change, IconName::Pencil).on_click(cx.listener(
                    move |this, _, window, cx| this.open_source(Some(open.clone()), window, cx),
                )))
        });
        let problem = self.source_problems.get(&group.name).cloned();
        div()
            .w_full()
            .pb(px(14.))
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                group_heading(group.name.clone(), theme)
                    .children(open_all)
                    .children(sourced),
            )
            .when_some(problem, |d, problem| {
                d.child(
                    div()
                        .px(px(4.))
                        .text_size(px(12.))
                        .text_color(theme.danger)
                        .truncate()
                        .child(problem),
                )
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
            .child(
                button("new-source", theme)
                    .debug_selector(|| "new-source".into())
                    .h(px(46.))
                    .px(px(16.))
                    .rounded(px(16.))
                    .text_size(px(14.))
                    .child(icon(IconName::SquareTerminal, 16., theme.text))
                    .child("Group from a command")
                    .on_click(
                        cx.listener(|this, _, window, cx| this.open_source(None, window, cx)),
                    ),
            )
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
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .text_size(px(15.))
                                            .line_height(px(19.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .truncate()
                                            .child(host.name.clone()),
                                    )
                                    // Something waits for it to come back.
                                    .when(self.waiting.command(&host.id).is_some(), |d| {
                                        let id = format!("waiting-{}", host.name);
                                        d.child(
                                            div()
                                                .debug_selector(move || id)
                                                .flex_none()
                                                .child(icon(IconName::Clock, 13., t.text2)),
                                        )
                                    }),
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
        if self.section == Section::Hosts {
            self.per_line = cards_per_line(self.hosts_width.get());
            self.set_items(self.items(self.per_line));
        }
        let theme = theme::current(cx);
        let ui = Ui {
            theme: &theme,
            live: &self.live,
        };
        let screen: Option<AnyElement> = match self.section {
            Section::Hosts => None,
            Section::Keychain => Some(self.keychain_screen(&theme, cx)),
            Section::Snippets => self.snippets.clone().map(|view| view.into_any_element()),
            Section::Forwarding => self.forwards.clone().map(|view| view.into_any_element()),
            Section::KnownHosts => self.known_hosts.clone().map(|view| view.into_any_element()),
            Section::History => self
                .history_view
                .clone()
                .map(|view| view.into_any_element()),
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
            .child(
                div()
                    .w(px(220.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(16.))
                    .child(self.vault_switcher(&theme, cx))
                    .child(sidebar(self.section, &theme, cx)),
            )
            .map(|d| match screen {
                Some(view) => d.child(view),
                None => d
                    .child(self.hosts(cx))
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
    fn a_slot_freeing_between_looks_goes_by_the_last_look(cx: &mut TestAppContext) {
        use std::time::Instant;

        let (vault, cx, _) = open_vault(cx);
        vault.update(cx, |vault, cx| {
            // Every place checked just now: nothing is due, nothing dialled.
            for place in vault.places(false).all {
                vault.monitor.record(&place, Some(1), Instant::now());
            }
            vault.run_due_checks(false, true, cx);
            let looked = vault.places.as_ref().unwrap().all.len();
            vault.vault.groups[0]
                .hosts
                .push(Host::new("new", "me", "10.9.9.9", 22));
            // Between looks, the places are where the last look found them;
            // the next look sees the vault as it is.
            vault.run_due_checks(false, false, cx);
            assert_eq!(vault.places.as_ref().unwrap().all.len(), looked);
            let new = ("10.9.9.9".to_string(), 22);
            vault.monitor.record(&new, Some(1), Instant::now());
            vault.run_due_checks(false, true, cx);
            assert_eq!(vault.places.as_ref().unwrap().all.len(), looked + 1);
        });
    }

    #[gpui::test]
    fn builds_only_the_hosts_on_screen(cx: &mut TestAppContext) {
        use gpui::{ScrollDelta, ScrollWheelEvent, point, px};
        use tern_core::HostGroup;

        cx.update(gpui_kit::init);
        let mut many = Vault::default();
        many.groups.push(HostGroup {
            name: "Many".into(),
            hosts: (0..300)
                .map(|i| Host::new(&format!("h{i:03}"), "me", "10.0.0.1", 22))
                .collect(),
        });
        let (_, cx) = cx.add_window_view(|window, cx| {
            VaultContent::new(many, None, LiveSlots::default(), window, cx)
        });
        cx.run_until_parked();
        let first = cx
            .debug_bounds("host-h000")
            .expect("the first card is built");
        assert!(
            cx.debug_bounds("host-h299").is_none(),
            "far below, not built"
        );

        // Scrolled to the end, a wheel turn at a time as the list measures
        // what comes into view, the last cards are built, and the first no
        // longer.
        for _ in 0..40 {
            cx.simulate_event(ScrollWheelEvent {
                position: first.center(),
                delta: ScrollDelta::Pixels(point(px(0.), px(-1000.))),
                ..Default::default()
            });
            cx.run_until_parked();
        }
        assert!(cx.debug_bounds("host-h299").is_some());
        assert!(cx.debug_bounds("host-h000").is_none());
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
    fn fills_a_group_from_a_command_and_keeps_to_it(cx: &mut TestAppContext) {
        use gpui_kit::component::input::InputState;

        use super::SourceForm;

        let (vault, cx, _) = open_vault(cx);
        let typed = |cx: &mut VisualTestContext,
                     field: fn(&SourceForm) -> Entity<InputState>,
                     text: &str| {
            cx.update(|window, cx| {
                let form = vault.read(cx).source_form.as_ref().map(field);
                let input = form.expect("the line is open");
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
        // A command printing hosts in OpenSSH's config form.
        let printing = |hosts: &[(&str, &str)]| {
            if cfg!(windows) {
                let lines: Vec<String> = hosts
                    .iter()
                    .map(|(name, address)| format!("echo Host {name}& echo   HostName {address}"))
                    .collect();
                format!("({})", lines.join("& "))
            } else {
                let lines: String = hosts
                    .iter()
                    .map(|(name, address)| format!("Host {name}\\n  HostName {address}\\n"))
                    .collect();
                format!("printf '{lines}'")
            }
        };
        let group_of = |cx: &mut VisualTestContext, name: &str| {
            vault.read_with(cx, |vault, _| {
                vault.vault.group_of(name).map(str::to_string)
            })
        };

        click(cx, "new-source");
        typed(cx, |form| form.group.clone(), "Fleet");
        typed(
            cx,
            |form| form.command.clone(),
            &printing(&[("fleet-web", "10.0.7.1")]),
        );
        click(cx, "fill-source");
        assert_eq!(group_of(cx, "fleet-web").as_deref(), Some("Fleet"));
        assert!(vault.read_with(cx, |vault, _| vault.source_form.is_none()));

        // The pencil opens its command; a new one fills it anew.
        click(cx, "source-group-Fleet");
        let two = printing(&[("fleet-web", "10.0.7.1"), ("fleet-db", "10.0.7.2")]);
        typed(cx, |form| form.command.clone(), &two);
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(group_of(cx, "fleet-db").as_deref(), Some("Fleet"));
        click(cx, "refresh-group-Fleet");
        assert_eq!(group_of(cx, "fleet-db").as_deref(), Some("Fleet"));

        // A command that fails says why, and the group stays as it was.
        click(cx, "source-group-Fleet");
        typed(cx, |form| form.command.clone(), "exit 3");
        click(cx, "fill-source");
        let problem = vault.read_with(cx, |vault, _| {
            vault
                .source_form
                .as_ref()
                .and_then(|form| form.problem.clone())
        });
        assert_eq!(problem.as_deref(), Some("It ended with status 3"));
        assert_eq!(group_of(cx, "fleet-web").as_deref(), Some("Fleet"));
        click(cx, "cancel-source");
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
        let command = connection.once.expect("the install runs after login");
        assert!(command.contains("'ssh-ed25519 AAAA me@laptop' >> ~/.ssh/authorized_keys"));
    }

    #[gpui::test]
    fn wakes_hosts_with_a_mac_address(cx: &mut TestAppContext) {
        let (vault, cx, _) = open_vault(cx);
        let type_mac = |cx: &mut VisualTestContext, text: &str| {
            cx.update(|window, cx| {
                let editor = vault.read(cx).editor.clone().expect("the editor is open");
                let input = editor.read(cx).input(Field::Mac);
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
        assert!(cx.debug_bounds("wake").is_none());
        click(cx, "host-bastion");
        click(cx, "edit-host");
        type_mac(cx, "aa:bb");
        click(cx, "save-host");
        let problems = vault.read_with(cx, |vault, cx| {
            vault.editor.as_ref().unwrap().read(cx).problem_fields()
        });
        assert_eq!(problems, [Field::Mac]);
        type_mac(cx, "AA-BB-CC-01-02-03");
        click(cx, "save-host");
        vault.read_with(cx, |vault, _| {
            let host = vault.vault.find("bastion").unwrap();
            assert_eq!(host.mac.as_deref(), Some("aa:bb:cc:01:02:03"));
        });
        // Only a host with a MAC address can be woken.
        assert!(cx.debug_bounds("wake").is_some());
        click(cx, "host-prod-db-01");
        assert!(cx.debug_bounds("wake").is_none());
    }

    #[gpui::test]
    fn keeps_notes_about_a_host(cx: &mut TestAppContext) {
        let (vault, cx, _) = open_vault(cx);
        click(cx, "host-bastion");
        click(cx, "edit-host");
        cx.update(|window, cx| {
            let editor = vault.read(cx).editor.clone().expect("the editor is open");
            let input = editor.read(cx).notes_input();
            input.update(cx, |state, cx| state.focus(window, cx));
        });
        cx.run_until_parked();
        cx.simulate_input("Reboots on Sundays");
        click(cx, "save-host");
        vault.read_with(cx, |vault, _| {
            assert_eq!(
                vault.vault.find("bastion").unwrap().notes,
                "Reboots on Sundays"
            )
        });
        assert!(cx.debug_bounds("host-notes").is_some());

        // Search finds a host by its notes.
        cx.update(|window, cx| {
            let search = vault.read(cx).search.clone();
            search.update(cx, |state, cx| state.focus(window, cx));
        });
        cx.run_until_parked();
        cx.simulate_input("sundays");
        cx.run_until_parked();
        assert!(cx.debug_bounds("host-bastion").is_some());
        assert!(cx.debug_bounds("host-prod-db-01").is_none());
    }

    #[gpui::test]
    fn copies_a_host_for_openssh(cx: &mut TestAppContext) {
        let (vault, cx, _) = open_vault(cx);
        click(cx, "host-bastion");
        click(cx, "copy-command");
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap();
        let expected = vault.read_with(cx, |vault, _| {
            tern_core::openssh::ssh_command(&vault.vault, vault.vault.find("bastion").unwrap())
        });
        assert_eq!(copied, expected);
        assert!(copied.starts_with("ssh "), "{copied}");
        vault.read_with(cx, |vault, _| {
            assert_eq!(vault.copied, Some("copy-command"))
        });
        click(cx, "copy-config");
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap();
        assert!(copied.starts_with("Host bastion\n"), "{copied}");
    }

    #[gpui::test]
    fn opens_a_group_side_by_side(cx: &mut TestAppContext) {
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
        click(cx, "open-group-Production");
        let hosts: Vec<String> = together
            .borrow()
            .iter()
            .map(|connection| connection.host.clone().unwrap())
            .collect();
        let expected: Vec<String> = vault.read_with(cx, |vault, _| {
            vault.vault.groups[0]
                .hosts
                .iter()
                .map(|host| host.name.clone())
                .collect()
        });
        assert_eq!(hosts, expected);
    }

    #[gpui::test]
    fn hosts_share_identities(cx: &mut TestAppContext) {
        use std::sync::Arc;

        use tern_core::identity::identity_password;
        use tern_core::secrets::{MemorySecrets, Secrets as _};

        use crate::sync::SecretsStore;

        let (vault, cx, _) = open_vault(cx);
        let secrets = Arc::new(MemorySecrets::default());
        let kept = secrets.clone();
        cx.update(|_, cx| cx.set_global(SecretsStore(secrets)));
        // Types into the identity form's field: name, user, key, password.
        let type_into = |cx: &mut VisualTestContext, field: usize, text: &str| {
            cx.update(|window, cx| {
                let view = vault
                    .read(cx)
                    .identities
                    .clone()
                    .expect("the screen is open");
                let input = view.read(cx).inputs().expect("the form is open")[field].clone();
                input.update(cx, |state, cx| state.focus(window, cx));
            });
            cx.run_until_parked();
            cx.simulate_input(text);
        };

        click(cx, "nav-keychain");
        click(cx, "keychain-identities");
        click(cx, "new-identity");
        type_into(cx, 0, "ops");
        type_into(cx, 1, "operator");
        click(cx, "identity-password");
        type_into(cx, 3, "s3cret");
        click(cx, "save-identity");
        let ops = vault.read_with(cx, |vault, _| vault.vault.identities[0].clone());
        assert_eq!((ops.name.as_str(), ops.user.as_str()), ("ops", "operator"));
        assert_eq!(
            kept.load(&identity_password(&ops.id)).as_deref(),
            Some("s3cret")
        );

        // A host takes the identity's user and sign-in.
        click(cx, "nav-hosts");
        click(cx, "host-prod-db-01");
        click(cx, "edit-host");
        click(cx, "identity-picker");
        click(cx, "identity-picker-ops");
        click(cx, "save-host");
        vault.read_with(cx, |vault, _| {
            let host = vault.vault.find("prod-db-01").unwrap();
            assert_eq!(host.identity.as_deref(), Some(ops.id.as_str()));
            let connection = vault.vault.connection(host).unwrap();
            assert_eq!(connection.target.user, "operator");
            assert_eq!(connection.secret, Some(identity_password(&ops.id)));
        });

        // Deleting it is refused while a host uses it.
        click(cx, "nav-keychain");
        click(cx, "identity-0");
        click(cx, "delete-identity");
        vault.read_with(cx, |vault, _| assert_eq!(vault.vault.identities.len(), 1));
    }

    #[gpui::test]
    fn runs_a_rule_without_a_terminal(cx: &mut TestAppContext) {
        use std::time::{Duration, Instant};

        use tern_core::forward::{Forward, ForwardKind, ForwardState};

        // Nothing listens on port 1, so the session fails at once, here.
        let mut hosts = Vault::default();
        let mut dead = Host::new("dead", "me", "127.0.0.1", 1);
        let rule = Forward::typed(ForwardKind::Local, "18080", "localhost:80").unwrap();
        dead.forwards.push(rule.clone());
        hosts.save(None, dead, "Lab");
        // The session's events arrive from the SSH runtime's own threads.
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        let (vault, cx) = cx.add_window_view(|window, cx| {
            VaultContent::new(hosts, None, LiveSlots::default(), window, cx)
        });
        let key = ("dead".to_string(), rule.id());
        let state = |cx: &mut VisualTestContext| {
            vault.read_with(cx, |vault, _| vault.forward_states.get(&key).cloned())
        };

        click(cx, "nav-port-forwarding");
        click(cx, "run-dead-0");
        // Running, unless already refused: Linux refuses at once, Windows
        // after a second or so.
        vault.read_with(cx, |vault, _| {
            assert!(
                vault.running_alone().contains(&key)
                    || matches!(
                        vault.forward_states.get(&key),
                        Some(ForwardState::Failed(_))
                    )
            )
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(state(cx), Some(ForwardState::Failed(_))) && Instant::now() < deadline {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        // The reason, in a few words, and nothing left running.
        assert_eq!(
            state(cx),
            Some(ForwardState::Failed("connection refused".into()))
        );
        vault.read_with(cx, |vault, _| assert!(vault.running_alone().is_empty()));
    }

    #[gpui::test]
    fn deleting_a_rule_stops_it_running_without_a_terminal(cx: &mut TestAppContext) {
        use tern_core::forward::{Forward, ForwardKind};

        // A server that never answers keeps the session signing in.
        let quiet = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = quiet.local_addr().unwrap().port();
        let mut hosts = Vault::default();
        let mut host = Host::new("quiet", "me", "127.0.0.1", port);
        host.forwards
            .push(Forward::typed(ForwardKind::Local, "18081", "localhost:80").unwrap());
        hosts.save(None, host, "Lab");
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        let (vault, cx) = cx.add_window_view(|window, cx| {
            VaultContent::new(hosts, None, LiveSlots::default(), window, cx)
        });
        let running = |cx: &mut VisualTestContext| {
            vault.read_with(cx, |vault, _| !vault.running_alone().is_empty())
        };

        click(cx, "nav-port-forwarding");
        click(cx, "run-quiet-0");
        assert!(running(cx));
        click(cx, "forward-quiet-0");
        click(cx, "delete-forward");
        assert!(!running(cx));
        drop(quiet);
    }

    #[gpui::test]
    fn the_hosts_keys_only_work_on_the_hosts_screen(cx: &mut TestAppContext) {
        let (vault, cx, connects) = open_vault(cx);
        click(cx, "nav-snippets");
        cx.update(|window, cx| vault.update(cx, |vault, cx| vault.focus(window, cx)));
        cx.simulate_keystrokes("e enter down");
        vault.read_with(cx, |vault, _| {
            assert!(vault.editor.is_none());
            assert_eq!(vault.selected, "prod-db-01");
        });
        assert!(connects.borrow().is_empty());
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
        // Typed once, not at every login like a host's own command.
        assert!(connections.iter().all(|connection| {
            connection.once.as_deref() == Some(command.trim_end()) && connection.on_login.is_none()
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
            vault.record_visit(visit(None, "10.0.0.9"), None, cx);
            vault.record_visit(visit(Some("bastion"), "old.example.com"), None, cx);
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
                VaultEvent::Connect(_)
                | VaultEvent::OpenTogether(_)
                | VaultEvent::Run(_)
                | VaultEvent::Switch => {}
            })
            .detach()
        });
        let rule = Forward::typed(ForwardKind::Local, "8080", "localhost:80").unwrap();
        vault.update(cx, |vault, cx| {
            vault.vault.save_forward("bastion", None, rule).unwrap();
            let preview = Rc::new(RefCell::new(vec!["admin@bastion:~$ uptime".to_string()]));
            let window = cx.entity_id();
            vault.set_open_sessions(
                window,
                HashMap::from([("bastion".to_string(), preview)]),
                cx,
            );
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
    fn a_command_waits_for_its_host_to_come_back(cx: &mut TestAppContext) {
        use std::time::Instant;

        let (vault, cx, _) = open_vault(cx);
        let ran: Rc<RefCell<Vec<Connection>>> = Rc::default();
        let seen = ran.clone();
        cx.update(|_, cx| {
            cx.subscribe(&vault, move |_, event: &VaultEvent, _| {
                if let VaultEvent::Run(connection) = event {
                    seen.borrow_mut().push(connection.clone());
                }
            })
            .detach()
        });
        click(cx, "host-bastion");
        assert!(cx.debug_bounds("wait").is_none());

        // A host that doesn't answer offers to run something once it does.
        let place = vault.update(cx, |vault, cx| {
            let place = vault.vault.first_hop(vault.vault.find("bastion").unwrap());
            vault.monitor.record(&place, None, Instant::now());
            cx.notify();
            place
        });
        click(cx, "wait");
        cx.simulate_input("sudo reboot-check");
        click(cx, "wait-queue");
        assert!(cx.debug_bounds("waiting-bastion").is_some());
        assert!(cx.debug_bounds("wait-cancel").is_some());

        // It answers: the command runs in a tab of its own, once.
        vault.update(cx, |vault, cx| {
            vault.monitor.record(&place, Some(20), Instant::now());
            vault.run_waiting(&place, cx);
            vault.run_waiting(&place, cx);
        });
        assert_eq!(ran.borrow().len(), 1);
        assert_eq!(ran.borrow()[0].once.as_deref(), Some("sudo reboot-check"));
        cx.run_until_parked();
        assert!(cx.debug_bounds("waiting-bastion").is_none());
        assert!(cx.debug_bounds("wait").is_none());

        // Dropped before then, nothing runs.
        vault.update(cx, |vault, cx| {
            vault.monitor.record(&place, None, Instant::now());
            let id = vault.vault.find("bastion").unwrap().id.clone();
            vault.waiting.set(&id, "uptime");
            cx.notify();
        });
        click(cx, "wait-cancel");
        vault.update(cx, |vault, cx| vault.run_waiting(&place, cx));
        assert_eq!(ran.borrow().len(), 1);
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
        click(cx, "guard-switch");
        click(cx, "keep-shell-switch");
        click(cx, "save-host");
        assert!(!editor_open(cx));
        vault.read_with(cx, |vault, _| {
            let host = vault.vault.find("db-new").expect("the new host is saved");
            assert_eq!((host.port, host.user.as_str()), (2200, "root"));
            assert!(host.guarded && host.keep_shell);
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

        vault.update(cx, |vault, cx| vault.run_due_checks(false, true, cx));
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
