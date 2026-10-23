//! The Port forwarding screen: each host's rules (`-L`, `-R`, `-D`) with an
//! ink switch. A rule switched on runs whenever a session to its host is
//! open; switching it while one is open starts or stops it there. A play
//! button runs a rule now without a terminal, in a session of its own. A
//! form at the top adds or edits a rule; the vault saves what it sends.

use std::collections::{HashMap, HashSet};

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::forward::{Forward, ForwardId, ForwardKind, ForwardState};

use crate::theme::{self, MONO, Theme};
use crate::ui::{button, group_heading, icon, ink_button, status_dot, switch};

pub enum ForwardEvent {
    /// Save `forward` on `host`, replacing the rule `replacing`.
    Save {
        host: String,
        replacing: Option<Forward>,
        forward: Forward,
    },
    Delete {
        host: String,
        forward: Forward,
    },
    Switch {
        host: String,
        forward: Forward,
        on: bool,
    },
    /// Run the rule now without a terminal, or stop it.
    RunAlone {
        host: String,
        forward: Forward,
        on: bool,
    },
}

impl EventEmitter<ForwardEvent> for ForwardsView {}

/// How each rule is doing in an open session, by host and rule.
pub type States = HashMap<(String, ForwardId), ForwardState>;

/// Rules running without a terminal, by host and rule.
pub type Alone = HashSet<(String, ForwardId)>;

/// The add or edit form.
struct Form {
    host: String,
    replacing: Option<Forward>,
    kind: ForwardKind,
    port: Entity<InputState>,
    to: Entity<InputState>,
    picking_host: bool,
    problem: Option<SharedString>,
}

pub struct ForwardsView {
    /// Every saved host's name and rules, in vault order.
    hosts: Vec<(String, Vec<Forward>)>,
    states: States,
    alone: Alone,
    form: Option<Form>,
}

impl ForwardsView {
    pub fn new(hosts: Vec<(String, Vec<Forward>)>, states: States, alone: Alone) -> Self {
        Self {
            hosts,
            states,
            alone,
            form: None,
        }
    }

    /// Takes the vault's rules and the sessions' states after a change.
    pub fn refresh(
        &mut self,
        hosts: Vec<(String, Vec<Forward>)>,
        states: States,
        alone: Alone,
        cx: &mut Context<Self>,
    ) {
        self.hosts = hosts;
        self.states = states;
        self.alone = alone;
        cx.notify();
    }

    /// A save the vault refused, shown under the form.
    pub fn refuse(&mut self, problem: String, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.problem = Some(problem.into());
            cx.notify();
        }
    }

    /// The vault saved the form; close it.
    pub fn saved(&mut self, cx: &mut Context<Self>) {
        self.form = None;
        cx.notify();
    }

    /// Opens the form on `editing` (a host's rule), or on a new rule for
    /// the first host.
    fn open_form(
        &mut self,
        editing: Option<(String, Forward)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(first) = self.hosts.first().map(|(host, _)| host.clone()) else {
            return;
        };
        let (host, kind, port, to) = match &editing {
            Some((host, forward)) => (
                host.clone(),
                forward.kind,
                forward.listen_port.to_string(),
                if forward.kind == ForwardKind::Dynamic {
                    String::new()
                } else {
                    format!("{}:{}", forward.to_host, forward.to_port)
                },
            ),
            None => (first, ForwardKind::Local, String::new(), String::new()),
        };
        let port = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Port")
                .default_value(port)
        });
        let to = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("host:port")
                .default_value(to)
        });
        for input in [&port, &to] {
            cx.subscribe(input, |this: &mut Self, _, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.save(cx);
                }
            })
            .detach();
        }
        port.update(cx, |state, cx| state.focus(window, cx));
        self.form = Some(Form {
            host,
            replacing: editing.map(|(_, forward)| forward),
            kind,
            port,
            to,
            picking_host: false,
            problem: None,
        });
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        let port = form.port.read(cx).value();
        let to = form.to.read(cx).value();
        match Forward::typed(form.kind, &port, &to) {
            Ok(mut forward) => {
                // An edited rule keeps its switch.
                if let Some(old) = &form.replacing {
                    forward.on = old.on;
                }
                cx.emit(ForwardEvent::Save {
                    host: form.host.clone(),
                    replacing: form.replacing.clone(),
                    forward,
                });
            }
            Err(problem) => {
                form.problem = Some(problem.into());
                cx.notify();
            }
        }
    }

    #[cfg(test)]
    pub fn form_inputs(&self) -> (Entity<InputState>, Entity<InputState>) {
        let form = self.form.as_ref().expect("the form is open");
        (form.port.clone(), form.to.clone())
    }

    fn form_card(&self, form: &Form, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let field = |input: &Entity<InputState>| {
            div()
                .h(px(44.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(px(12.))
                .bg(t.card2)
                .font_family(MONO)
                .child(Input::new(input).appearance(false))
        };
        let kinds = [
            (ForwardKind::Local, "-L", "kind-local"),
            (ForwardKind::Remote, "-R", "kind-remote"),
            (ForwardKind::Dynamic, "-D", "kind-dynamic"),
        ];
        let hosts: Vec<String> = self.hosts.iter().map(|(host, _)| host.clone()).collect();
        div()
            .p(px(14.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .rounded(px(22.))
            .bg(t.card)
            .shadow(t.shadow())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    // The host the rule belongs to.
                    .child(
                        div()
                            .child(
                                div()
                                    .id("forward-host")
                                    .debug_selector(|| "forward-host".into())
                                    .h(px(44.))
                                    .w(px(200.))
                                    .px(px(12.))
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .rounded(px(12.))
                                    .bg(t.card2)
                                    .cursor_pointer()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .child(form.host.clone()),
                                    )
                                    .child(icon(IconName::ChevronDown, 15., t.text3))
                                    .when(form.replacing.is_none(), |d| {
                                        d.on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(form) = &mut this.form {
                                                form.picking_host = true;
                                                cx.notify();
                                            }
                                        }))
                                    }),
                            )
                            .when(form.picking_host, |d| {
                                let hover = t.card2;
                                d.child(deferred(
                                    anchored().snap_to_window_with_margin(px(8.)).child(
                                        div()
                                            .id("forward-host-menu")
                                            .occlude()
                                            .mt(px(6.))
                                            .min_w(px(220.))
                                            .max_h(px(320.))
                                            .overflow_y_scroll()
                                            .p(px(6.))
                                            .flex()
                                            .flex_col()
                                            .rounded(px(14.))
                                            .bg(t.card)
                                            .border_1()
                                            .border_color(t.line)
                                            .shadow(t.shadow())
                                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                                if let Some(form) = &mut this.form {
                                                    form.picking_host = false;
                                                    cx.notify();
                                                }
                                            }))
                                            .children(hosts.into_iter().map(|host| {
                                                let id = format!("forward-host-{host}");
                                                div()
                                                    .id(SharedString::from(id.clone()))
                                                    .debug_selector(move || id)
                                                    .h(px(36.))
                                                    .px(px(10.))
                                                    .flex()
                                                    .items_center()
                                                    .rounded(px(10.))
                                                    .cursor_pointer()
                                                    .hover(move |s| s.bg(hover))
                                                    .child(host.clone())
                                                    .on_click(cx.listener(move |this, _, _, cx| {
                                                        if let Some(form) = &mut this.form {
                                                            form.host = host.clone();
                                                            form.picking_host = false;
                                                            cx.notify();
                                                        }
                                                    }))
                                            })),
                                    ),
                                ))
                            }),
                    )
                    // The kind, as OpenSSH's flags.
                    .child(
                        div()
                            .flex()
                            .gap(px(4.))
                            .p(px(4.))
                            .rounded(px(14.))
                            .bg(t.card2)
                            .children(kinds.into_iter().map(|(kind, flag, id)| {
                                let on = form.kind == kind;
                                div()
                                    .id(id)
                                    .debug_selector(move || id.into())
                                    .w(px(44.))
                                    .h(px(36.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(11.))
                                    .font_family(MONO)
                                    .text_size(px(13.))
                                    .text_color(if on { t.text } else { t.text2 })
                                    .when(on, |d| d.bg(t.card).shadow(t.shadow_sm()))
                                    .cursor_pointer()
                                    .child(flag)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(form) = &mut this.form {
                                            form.kind = kind;
                                            cx.notify();
                                        }
                                    }))
                            })),
                    )
                    .child(field(&form.port).w(px(110.)).flex_none())
                    .when(form.kind != ForwardKind::Dynamic, |d| {
                        d.child(icon(IconName::ArrowRight, 15., t.text3))
                            .child(field(&form.to).flex_1().min_w_0())
                    })
                    .when(form.kind == ForwardKind::Dynamic, |d| {
                        d.child(div().flex_1())
                    })
                    .when_some(form.replacing.clone(), |d, forward| {
                        let host = form.host.clone();
                        d.child(
                            div()
                                .id("delete-forward")
                                .debug_selector(|| "delete-forward".into())
                                .h(px(44.))
                                .px(px(10.))
                                .flex()
                                .items_center()
                                .text_color(t.danger)
                                .font_weight(FontWeight::SEMIBOLD)
                                .cursor_pointer()
                                .child("Delete")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.form = None;
                                    cx.emit(ForwardEvent::Delete {
                                        host: host.clone(),
                                        forward: forward.clone(),
                                    });
                                })),
                        )
                    })
                    .child(
                        button("cancel-forward", t)
                            .h(px(44.))
                            .px(px(16.))
                            .rounded(px(14.))
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.form = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        ink_button("save-forward", t)
                            .debug_selector(|| "save-forward".into())
                            .h(px(44.))
                            .px(px(18.))
                            .rounded(px(14.))
                            .child("Save")
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
            .when_some(form.problem.clone(), |d, problem| {
                d.child(
                    div()
                        .px(px(4.))
                        .text_size(px(13.))
                        .text_color(t.danger)
                        .child(problem),
                )
            })
    }

    fn row(
        &self,
        host: &str,
        index: usize,
        forward: &Forward,
        t: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let state = self.states.get(&(host.to_string(), forward.id()));
        let alone = self.alone.contains(&(host.to_string(), forward.id()));
        // Running in a terminal's session already: nothing to start here.
        let in_session = !alone && matches!(state, Some(ForwardState::Running));
        let row_id = format!("forward-{host}-{index}");
        let switch_id = format!("switch-{host}-{index}");
        let run_id = format!("run-{host}-{index}");
        let run = (host.to_string(), forward.clone());
        let (editing, toggled) = (
            (host.to_string(), forward.clone()),
            (host.to_string(), forward.clone()),
        );
        div()
            .id(SharedString::from(row_id.clone()))
            .debug_selector(move || row_id)
            .flex()
            .items_center()
            .gap(px(14.))
            .py(px(12.))
            .px(px(14.))
            .rounded(px(18.))
            .bg(t.card)
            .shadow(t.shadow())
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_form(Some(editing.clone()), window, cx)
            }))
            .child(
                div()
                    .w(px(26.))
                    .flex_none()
                    .font_family(MONO)
                    .text_size(px(13.))
                    .text_color(t.text3)
                    .child(forward.flag()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family(MONO)
                    .text_size(px(13.))
                    .truncate()
                    .child(forward.route()),
            )
            .map(|d| match state {
                Some(ForwardState::Running) => d.child(status_dot(t)),
                Some(ForwardState::Failed(reason)) => d.child(
                    div()
                        .max_w(px(240.))
                        .text_size(px(13.))
                        .text_color(t.danger)
                        .truncate()
                        .child(reason.clone()),
                ),
                _ => d,
            })
            .when(!in_session, |d| {
                d.child(
                    div()
                        .id(SharedString::from(run_id.clone()))
                        .debug_selector(move || run_id)
                        .size(px(32.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(10.))
                        .border_1()
                        .border_color(if alone { t.text } else { t.line })
                        .hover(|d| d.bg(t.card2))
                        .child(icon(
                            if alone {
                                IconName::Square
                            } else {
                                IconName::Play
                            },
                            13.,
                            t.text,
                        ))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.stop_propagation();
                            let (host, forward) = run.clone();
                            cx.emit(ForwardEvent::RunAlone {
                                host,
                                forward,
                                on: !alone,
                            });
                        })),
                )
            })
            .child(
                switch(SharedString::from(switch_id.clone()), forward.on, t)
                    .debug_selector(move || switch_id)
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.stop_propagation();
                        let (host, forward) = toggled.clone();
                        let on = !forward.on;
                        cx.emit(ForwardEvent::Switch { host, forward, on });
                    })),
            )
    }
}

impl Render for ForwardsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let t = &theme;
        div()
            .id("forwarding")
            .debug_selector(|| "forwarding".into())
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(22.))
            .child(
                div().flex().justify_end().child(
                    ink_button("new-forward", t)
                        .debug_selector(|| "new-forward".into())
                        .h(px(46.))
                        .px(px(20.))
                        .rounded(px(16.))
                        .text_size(px(15.))
                        .child(icon(IconName::Plus, 16., t.card))
                        .child("New rule")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_form(None, window, cx)),
                        ),
                ),
            )
            .when_some(self.form.as_ref(), |d, form| {
                d.child(self.form_card(form, t, cx))
            })
            .children(
                self.hosts
                    .iter()
                    .filter(|(_, rules)| !rules.is_empty())
                    .map(|(host, rules)| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(10.))
                            .child(group_heading(host.clone(), t))
                            .children(
                                rules
                                    .iter()
                                    .enumerate()
                                    .map(|(index, forward)| self.row(host, index, forward, t, cx)),
                            )
                    }),
            )
    }
}
