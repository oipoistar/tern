//! The hosts vault: Tern's home screen. It asks the workspace to open a
//! session with [`VaultEvent::Connect`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::host::sample_groups;
use tern_core::randomart::Randomart;
use tern_core::ssh::{Target, key_art, known_host_key, quick_destination};
use tern_core::{Host, HostGroup, HostStatus};

use crate::live::LiveSlots;
use crate::marks;
use crate::theme::{self, MONO, Theme};
use crate::ui::{Ui, icon};

/// Where `tern-devserver` keeps the key it accepts.
const DEV_KEY: &str = "dev/test-ssh/id_ed25519";

/// Wide enough for `user@host` and two tags; three fit beside the sidebar
/// in the default window.
const CARD_WIDTH: f32 = 320.;

pub enum VaultEvent {
    Connect {
        target: Target,
        identity: Option<PathBuf>,
    },
}

impl EventEmitter<VaultEvent> for VaultContent {}

/// Everything on the vault screen except the header.
pub struct VaultContent {
    groups: Vec<HostGroup>,
    selected: String,
    live: LiveSlots,
    /// Art for each host whose key a known_hosts file already trusts, by
    /// (address, port).
    keys: HashMap<(String, u16), Randomart>,
    search: Entity<InputState>,
    query: SharedString,
    /// Set when the search text is itself an SSH destination.
    destination: Option<(Target, Option<PathBuf>)>,
    _subscription: Subscription,
}

impl VaultContent {
    pub fn new(live: LiveSlots, window: &mut Window, cx: &mut Context<Self>) -> Self {
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
                    let next = this.destination.clone().or_else(|| {
                        this.matching_hosts()
                            .next()
                            .map(|host| (host.ssh_target(), host.identity.clone()))
                    });
                    if let Some((target, identity)) = next {
                        this.connect(target, identity, window, cx);
                    }
                }
                _ => {}
            },
        );

        let mut groups = sample_groups();
        if Path::new(DEV_KEY).exists() {
            groups.insert(0, dev_server_group());
        }
        let selected = groups[0].hosts[0].name.clone();
        let mut vault = Self {
            groups,
            selected,
            live,
            keys: HashMap::new(),
            search,
            query: SharedString::default(),
            destination: None,
            _subscription: subscription,
        };
        vault.refresh_keys();
        vault
    }

    /// Re-reads which hosts have a trusted key (known_hosts files only, read
    /// never written), so a key trusted in a session shows up here.
    pub fn refresh_keys(&mut self) {
        self.keys = self
            .groups
            .iter()
            .flat_map(|group| &group.hosts)
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
        self.groups
            .iter()
            .flat_map(|group| &group.hosts)
            .filter(move |host| searching && host.matches(&self.query))
    }

    fn connect(
        &mut self,
        target: Target,
        identity: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search
            .update(cx, |state, cx| state.set_value("", window, cx));
        cx.emit(VaultEvent::Connect { target, identity });
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
            .child(self.toolbar(theme))
            .when_some(self.destination.clone(), |d, (target, identity)| {
                d.child(destination_card(target, identity, theme, cx))
            })
            .children(self.groups.iter().filter_map(|group| {
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

    fn toolbar(&self, theme: &Theme) -> impl IntoElement {
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
            // The screen's primary action, so the one solid ink button.
            .child(
                div()
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
                    .child(icon(IconName::Plus, 16., theme.card))
                    .child("New host"),
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
        let latency = match host.status {
            HostStatus::Online {
                latency_ms: Some(ms),
            } => format!("{ms} ms"),
            _ => String::new(),
        };
        let name = host.name.clone();
        let (target, identity) = (host.ssh_target(), host.identity.clone());

        div()
            .id(SharedString::from(format!("host-{}", host.name)))
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
                this.selected = name.clone();
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .child(art_tile(art, &t))
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
                    .child(
                        connect_button(format!("connect-{}", host.name), selected, &t)
                            .self_start()
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.emit(VaultEvent::Connect {
                                    target: target.clone(),
                                    identity: identity.clone(),
                                });
                            })),
                    ),
            )
            .child(
                div()
                    .h(px(18.))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        ui.trace(
                            format!("trace-{}", host.name),
                            host.status,
                            marks::seed(&host.name),
                            t.text,
                            t.text3,
                        )
                        .flex_1()
                        .h_full(),
                    )
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
            .flex()
            .gap(px(20.))
            .p(px(20.))
            .bg(theme.win)
            .text_color(theme.text)
            .text_size(px(14.))
            .child(sidebar(&theme))
            .child(self.hosts(&ui, cx))
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
            this.connect(target.clone(), identity.clone(), window, cx);
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
        .child(connect_button("destination-connect".into(), true, theme))
}

/// Host-key art in a hairline tile; blank until the host's key is trusted.
fn art_tile(art: Option<Randomart>, theme: &Theme) -> impl IntoElement {
    let (ink, faint) = (theme.text, theme.text3);
    div()
        .size(px(52.))
        .flex_none()
        .rounded(px(15.))
        .border_1()
        .border_color(theme.line)
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    marks::paint_art(bounds, art.as_ref(), ink, faint, window)
                },
            )
            .size_full(),
        )
}

/// A round arrow button; `filled` makes it solid ink for the primary choice.
fn connect_button(id: String, filled: bool, theme: &Theme) -> Stateful<Div> {
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
            IconName::ArrowRight,
            16.,
            if filled { theme.card } else { theme.text },
        ))
}

/// The local dev server from `tern-devserver`, shown only when its key exists.
fn dev_server_group() -> HostGroup {
    HostGroup {
        name: "Local dev server".into(),
        hosts: vec![Host {
            name: "tern-devserver".into(),
            user: "tern".into(),
            address: "127.0.0.1".into(),
            port: 2222,
            os: "This computer".into(),
            tags: vec!["dev".into()],
            status: HostStatus::Online { latency_ms: None },
            identity: Some(PathBuf::from(DEV_KEY)),
        }],
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

    use gpui::TestAppContext;
    use tern_core::ssh::Target;

    use super::{VaultContent, VaultEvent};
    use crate::live::LiveSlots;

    #[gpui::test]
    fn search_filters_hosts_and_connects_to_a_typed_destination(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (vault, cx) =
            cx.add_window_view(|window, cx| VaultContent::new(LiveSlots::default(), window, cx));
        let connects: Rc<RefCell<Vec<Target>>> = Rc::default();
        let _subscription = cx.update(|_, cx| {
            let connects = connects.clone();
            cx.subscribe(&vault, move |_, event: &VaultEvent, _| {
                let VaultEvent::Connect { target, .. } = event;
                connects.borrow_mut().push(target.clone());
            })
        });
        let focus_search = |cx: &mut gpui::VisualTestContext| {
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
            connects.borrow().last().map(|target| target.host.clone()),
            Some("10.0.4.21".into())
        );

        // A destination is offered as it is, and Enter connects to it.
        focus_search(cx);
        cx.simulate_input("deploy@10.0.4.21:2200");
        vault.read_with(cx, |vault, _| assert!(vault.destination.is_some()));
        cx.simulate_keystrokes("enter");
        let target = connects.borrow().last().cloned().expect("a connect event");
        assert_eq!((target.user.as_str(), target.port), ("deploy", 2200));
        vault.read_with(cx, |vault, cx| {
            assert!(vault.search.read(cx).value().is_empty())
        });
    }
}
