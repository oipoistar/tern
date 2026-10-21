//! The hosts vault: Tern's home screen.

use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::host::sample_groups;
use tern_core::{Host, HostGroup, HostStatus};

use crate::glass::{Glass, glass};
use crate::theme::{MONO, Milk, Theme, Tone};

/// Voxel glass drifts slowly, so 20 fps is plenty; nothing else animates.
const FRAME: Duration = Duration::from_millis(50);

pub struct Vault {
    dark: bool,
    groups: Vec<HostGroup>,
    selected: String,
    started: Instant,
    _ticker: Task<()>,
}

impl Vault {
    /// Starts in the system's light/dark appearance; `TERN_THEME=light|dark`
    /// overrides it (handy for screenshots).
    pub fn new(window: &Window, cx: &mut Context<Self>) -> Self {
        let dark = match std::env::var("TERN_THEME").as_deref() {
            Ok("dark") => true,
            Ok("light") => false,
            _ => matches!(
                window.appearance(),
                WindowAppearance::Dark | WindowAppearance::VibrantDark
            ),
        };
        let ticker = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(FRAME).await;
                // Freeze the glass while another app has focus.
                let alive = this.update_in(cx, |_, window, cx| {
                    if window.is_window_active() && !cx.reduce_motion() {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        Self {
            dark,
            groups: sample_groups(),
            selected: "prod-db-01".into(),
            started: Instant::now(),
            _ticker: ticker,
        }
    }

    fn header(&self, theme: &Theme, time: f32, cx: &Context<Self>) -> impl IntoElement {
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
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(glass(
                        Glass {
                            tone: Tone::Brand,
                            milk: Milk::Regular,
                            radius: px(11.),
                            underlay: theme.win,
                            seed: 0.,
                        },
                        theme,
                        time,
                        div()
                            .size(px(34.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(tern_mark(theme.glass_text)),
                    ))
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::BOLD)
                            .child("Tern"),
                    ),
            )
            .child(tabs(theme))
            .child(div().flex_1())
            .child(glass(
                Glass {
                    tone: Tone::Ok,
                    milk: Milk::Strong,
                    radius: px(12.),
                    underlay: theme.win,
                    seed: 1.4,
                },
                theme,
                time,
                div()
                    .h(px(34.))
                    .px(px(14.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(13.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(icon(IconName::Check, 14., theme.ok_glyph))
                    .child("Synced · Google Drive"),
            ))
            .child(
                div()
                    .id("theme-toggle")
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
                    .child(icon(
                        if self.dark {
                            IconName::Sun
                        } else {
                            IconName::Moon
                        },
                        18.,
                        theme.text2,
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.dark = !this.dark;
                        cx.notify();
                    })),
            )
    }

    fn hosts(&self, theme: &Theme, time: f32, cx: &Context<Self>) -> impl IntoElement {
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
            .child(toolbar(theme, time))
            .children(self.groups.iter().map(|group| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(4.))
                            .child(
                                div()
                                    .text_size(px(15.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(group.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(theme.text3)
                                    .child(format!("{} hosts", group.hosts.len())),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(14.))
                            .children(group.hosts.iter().map(|host| {
                                seed += 1.37;
                                self.host_card(host, theme, time, seed, cx)
                            })),
                    )
            }))
    }

    fn host_card(
        &self,
        host: &Host,
        theme: &Theme,
        time: f32,
        seed: f32,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let selected = host.name == self.selected;
        let name = host.name.clone();
        let tile = div()
            .size(px(44.))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.))
            .font_weight(FontWeight::BOLD)
            .child(host.monogram());

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
                        glass(
                            Glass {
                                tone: Tone::Brand,
                                milk: Milk::Regular,
                                radius: px(14.),
                                underlay: theme.card,
                                seed,
                            },
                            theme,
                            time,
                            tile,
                        )
                        .into_any_element()
                    } else {
                        tile.rounded(px(14.))
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
                    .child(status_chip(host.status, theme, time, seed + 0.6))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(theme.text2)
                            .truncate()
                            .child(host.tags.join(" · ")),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(12.))
                            .text_color(theme.text3)
                            .child(format!("{} · port {}", host.os, host.port)),
                    )
                    .child(
                        div()
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
                            .child(if selected { "Open" } else { "Connect" }),
                    ),
            )
    }
}

impl Render for Vault {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = if self.dark {
            Theme::dark()
        } else {
            Theme::light()
        };
        let time = if cx.reduce_motion() {
            0.
        } else {
            self.started.elapsed().as_secs_f32()
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.win)
            .text_color(theme.text)
            .text_size(px(14.))
            .child(self.header(&theme, time, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap(px(20.))
                    .p(px(20.))
                    .child(sidebar(&theme, time))
                    .child(self.hosts(&theme, time, cx)),
            )
    }
}

fn icon(name: IconName, size: f32, color: Hsla) -> Icon {
    Icon::new(name).w(px(size)).h(px(size)).text_color(color)
}

fn status_dot(theme: &Theme) -> Div {
    div()
        .size(px(7.))
        .rounded_full()
        .bg(theme.ok_glyph)
        .shadow(vec![theme.status_dot_halo()])
}

fn tabs(theme: &Theme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(4.))
        .p(px(4.))
        .rounded(px(16.))
        .bg(theme.card2)
        .child(
            div()
                .h(px(34.))
                .px(px(14.))
                .flex()
                .items_center()
                .gap(px(8.))
                .rounded(px(12.))
                .bg(theme.card)
                .shadow(theme.shadow_sm())
                .text_size(px(13.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(icon(IconName::Server, 16., theme.text))
                .child("Vault"),
        )
        .child(
            div()
                .h(px(34.))
                .px(px(12.))
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(13.))
                .text_color(theme.text2)
                .child(status_dot(theme))
                .child("prod-db-01"),
        )
}

fn sidebar(theme: &Theme, time: f32) -> impl IntoElement {
    let items = [
        (IconName::Server, "Hosts", "7"),
        (IconName::KeyRound, "Keychain", "4"),
        (IconName::CodeXml, "Snippets", "18"),
        (IconName::ArrowLeftRight, "Port forwarding", "2"),
        (IconName::ShieldCheck, "Known hosts", "9"),
        (IconName::Clock, "History", ""),
    ];
    div()
        .w(px(220.))
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(4.))
        .children(
            items
                .into_iter()
                .enumerate()
                .map(|(i, (name, label, count))| {
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
                        .child(div().flex_1().child(label))
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme.text3)
                                .child(count),
                        )
                }),
        )
        .child(div().flex_1())
        .child(sync_card(theme, time))
}

fn sync_card(theme: &Theme, time: f32) -> impl IntoElement {
    div()
        .p(px(14.))
        .rounded(px(20.))
        .bg(theme.card)
        .shadow(theme.shadow())
        .flex()
        .flex_col()
        .gap(px(10.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(glass(
                    Glass {
                        tone: Tone::Ok,
                        milk: Milk::Regular,
                        radius: px(12.),
                        underlay: theme.card,
                        seed: 2.1,
                    },
                    theme,
                    time,
                    div()
                        .size(px(36.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(icon(IconName::Cloud, 18., theme.glass_text)),
                ))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(px(13.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Google Drive"),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme.text3)
                                .child("Synced 2 min ago"),
                        ),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(12.))
                .text_color(theme.text2)
                .child(icon(IconName::Lock, 14., theme.text2))
                .child("Encrypted on this device"),
        )
}

fn toolbar(theme: &Theme, time: f32) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .h(px(46.))
                .pl(px(18.))
                .pr(px(4.))
                .flex()
                .items_center()
                .gap(px(10.))
                .rounded(px(16.))
                .bg(theme.card2)
                .child(
                    div()
                        .flex_1()
                        .text_color(theme.text3)
                        .child("Search hosts, or ssh user@host"),
                )
                .child(
                    div()
                        .px(px(8.))
                        .py(px(3.))
                        .rounded(px(8.))
                        .border_1()
                        .border_color(theme.line)
                        .text_size(px(12.))
                        .text_color(theme.text3)
                        .child("Ctrl K"),
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
        .child(glass(
            Glass {
                tone: Tone::Brand,
                milk: Milk::Regular,
                radius: px(16.),
                underlay: theme.win,
                seed: 0.7,
            },
            theme,
            time,
            div()
                .h(px(46.))
                .px(px(20.))
                .flex()
                .items_center()
                .gap(px(8.))
                .text_size(px(15.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(icon(IconName::Plus, 16., theme.glass_text))
                .child("New host"),
        ))
}

fn status_chip(status: HostStatus, theme: &Theme, time: f32, seed: f32) -> AnyElement {
    let chip = |glyph: AnyElement, label: &'static str| {
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
    };
    let on_glass = |tone| Glass {
        tone,
        milk: Milk::Strong,
        radius: px(10.),
        underlay: theme.card,
        seed,
    };
    match status {
        HostStatus::Online => glass(
            on_glass(Tone::Ok),
            theme,
            time,
            chip(status_dot(theme).into_any_element(), "Online"),
        )
        .into_any_element(),
        HostStatus::Connecting => glass(
            on_glass(Tone::Info),
            theme,
            time,
            chip(
                icon(IconName::LoaderCircle, 13., theme.info_glyph).into_any_element(),
                "Connecting",
            ),
        )
        .into_any_element(),
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

/// The Tern mark: wings and a forked tail, stroked on a 24-unit grid.
fn tern_mark(color: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let scale = bounds.size.width.as_f32() / 24.;
            let at = |x: f32, y: f32| {
                point(
                    bounds.origin.x + px(x * scale),
                    bounds.origin.y + px(y * scale),
                )
            };
            let mut mark = PathBuilder::stroke(px(2.2 * scale));
            mark.move_to(at(2., 9.));
            mark.cubic_bezier_to(at(12., 11.), at(6., 6.), at(9., 7.));
            mark.cubic_bezier_to(at(22., 9.), at(15., 7.), at(18., 6.));
            mark.move_to(at(12., 11.));
            mark.line_to(at(9.5, 19.));
            mark.move_to(at(12., 11.));
            mark.line_to(at(14.5, 19.));
            if let Ok(path) = mark.build() {
                window.paint_path(path, color);
            }
        },
    )
    .size(px(20.))
}
