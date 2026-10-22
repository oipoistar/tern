//! The hosts vault: Tern's home screen. It asks the workspace to open a
//! session with [`VaultEvent::Connect`].

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::host::sample_groups;
use tern_core::ssh::{Target, parse_command};
use tern_core::{Host, HostGroup, HostStatus};

use crate::glass::GlassSlots;
use crate::theme::{self, MONO, Milk, Theme, Tone};
use crate::ui::{Ui, icon, status_dot};

/// Where `tern-devserver` keeps the key it accepts.
const DEV_KEY: &str = "dev/test-ssh/id_ed25519";

pub enum VaultEvent {
    Connect {
        target: Target,
        identity: Option<PathBuf>,
    },
}

impl EventEmitter<VaultEvent> for VaultContent {}

/// Everything on the vault screen except the header and the animation.
pub struct VaultContent {
    groups: Vec<HostGroup>,
    selected: String,
    slots: GlassSlots,
    quick: Entity<InputState>,
    quick_error: Option<SharedString>,
    _subscription: Subscription,
}

impl VaultContent {
    pub fn new(slots: GlassSlots, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let quick = cx.new(|cx| InputState::new(window, cx).placeholder("Search or ssh user@host"));
        let subscription = cx.subscribe_in(
            &quick,
            window,
            |this, quick, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let text = quick.read(cx).value();
                    match parse_command(&text) {
                        Ok((target, identity)) => {
                            this.quick_error = None;
                            quick.update(cx, |state, cx| state.set_value("", window, cx));
                            cx.emit(VaultEvent::Connect { target, identity });
                        }
                        Err(error) => this.quick_error = Some(error.to_string().into()),
                    }
                    cx.notify();
                }
            },
        );

        let mut groups = sample_groups();
        if Path::new(DEV_KEY).exists() {
            groups.insert(0, dev_server_group());
        }
        let selected = groups[0].hosts[0].name.clone();
        Self {
            groups,
            selected,
            slots,
            quick,
            quick_error: None,
            _subscription: subscription,
        }
    }

    fn hosts(&self, ui: &Ui, cx: &Context<Self>) -> impl IntoElement {
        let mut seed = 3.;
        div()
            .id("hosts")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(22.))
            .child(self.toolbar(ui))
            .children(self.groups.iter().map(|group| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(
                        div()
                            .px(px(4.))
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(group.name.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(14.))
                            .children(group.hosts.iter().map(|host| {
                                seed += 1.37;
                                self.host_card(host, ui, seed, cx)
                            })),
                    )
            }))
    }

    fn toolbar(&self, ui: &Ui) -> impl IntoElement {
        let theme = ui.theme;
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
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
                                    .child(Input::new(&self.quick).appearance(false)),
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
                    .child(ui.glass(
                        "new-host",
                        Tone::Brand,
                        Milk::Regular,
                        16.,
                        theme.win,
                        0.7,
                        {
                            let color = theme.glass_text;
                            move || {
                                div()
                                    .h(px(46.))
                                    .px(px(20.))
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .text_size(px(15.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(icon(IconName::Plus, 16., color))
                                    .child("New host")
                            }
                        },
                    )),
            )
            .when_some(self.quick_error.clone(), |d, error| {
                d.child(
                    div()
                        .px(px(12.))
                        .text_size(px(12.))
                        .text_color(theme.danger)
                        .child(error),
                )
            })
    }

    fn host_card(&self, host: &Host, ui: &Ui, seed: f32, cx: &Context<Self>) -> impl IntoElement {
        let theme = ui.theme;
        let selected = host.name == self.selected;
        let name = host.name.clone();
        let monogram = host.monogram();
        let tile = move || {
            div()
                .size(px(44.))
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(13.))
                .font_weight(FontWeight::BOLD)
                .child(monogram.clone())
        };
        let (target, identity) = (host.ssh_target(), host.identity.clone());

        div()
            .id(SharedString::from(format!("host-{}", host.name)))
            .w(px(268.))
            .p(px(14.))
            .rounded(px(22.))
            .bg(theme.card)
            .shadow(if selected {
                theme.selected_shadow()
            } else {
                theme.shadow()
            })
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
                    .gap(px(12.))
                    .child(if selected {
                        ui.glass(
                            format!("tile-{}", host.name),
                            Tone::Brand,
                            Milk::Regular,
                            14.,
                            theme.card,
                            seed,
                            tile,
                        )
                        .into_any_element()
                    } else {
                        tile()
                            .rounded(px(14.))
                            .bg(theme.card2)
                            .text_color(theme.text2)
                            .into_any_element()
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(host.name.clone()),
                            )
                            .child(
                                div()
                                    .font_family(MONO)
                                    .text_size(px(12.))
                                    .text_color(theme.text2)
                                    .truncate()
                                    .child(host.target()),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(status_chip(
                        ui,
                        format!("status-{}", host.name),
                        host.status,
                        seed + 0.6,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .text_color(theme.text2)
                            .truncate()
                            .child(host.tags.join(" · ")),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("connect-{}", host.name)))
                            .h(px(34.))
                            .px(px(14.))
                            .flex()
                            .items_center()
                            .rounded(px(11.))
                            .border_1()
                            .border_color(theme.line)
                            .bg(theme.card)
                            .shadow(theme.shadow_sm())
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .child("Connect")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.emit(VaultEvent::Connect {
                                    target: target.clone(),
                                    identity: identity.clone(),
                                });
                            })),
                    ),
            )
    }
}

impl Render for VaultContent {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Placeholders re-record their surfaces during this frame's prepaint.
        self.slots.clear();
        let theme = theme::current(cx);
        let ui = Ui {
            theme: &theme,
            slots: &self.slots,
        };
        div()
            .size_full()
            .flex()
            .gap(px(20.))
            .p(px(20.))
            .bg(theme.win)
            .text_color(theme.text)
            .text_size(px(14.))
            .child(sidebar(&ui))
            .child(self.hosts(&ui, cx))
    }
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
            status: HostStatus::Online,
            identity: Some(PathBuf::from(DEV_KEY)),
        }],
    }
}

fn sidebar(ui: &Ui) -> impl IntoElement {
    let theme = ui.theme;
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

fn status_chip(ui: &Ui, id: String, status: HostStatus, seed: f32) -> AnyElement {
    let theme = ui.theme;
    fn chip(glyph: AnyElement, label: &'static str) -> Div {
        div()
            .h(px(28.))
            .px(px(11.))
            .flex()
            .items_center()
            .gap(px(7.))
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .child(glyph)
            .child(label)
    }
    match status {
        HostStatus::Online => {
            let theme: Rc<Theme> = theme.clone();
            ui.glass(
                id,
                Tone::Ok,
                Milk::Strong,
                10.,
                ui.theme.card,
                seed,
                move || chip(status_dot(&theme).into_any_element(), "Online"),
            )
            .into_any_element()
        }
        HostStatus::Connecting => {
            let spinner = theme.info_glyph;
            ui.glass(
                id,
                Tone::Info,
                Milk::Strong,
                10.,
                theme.card,
                seed,
                move || {
                    chip(
                        icon(IconName::LoaderCircle, 13., spinner).into_any_element(),
                        "Connecting",
                    )
                },
            )
            .into_any_element()
        }
        HostStatus::Offline => chip(
            div()
                .size(px(7.))
                .rounded_full()
                .border_1()
                .border_color(theme.text2)
                .into_any_element(),
            "Offline",
        )
        .rounded(px(10.))
        .bg(theme.card2)
        .text_color(theme.text2)
        .into_any_element(),
    }
}
