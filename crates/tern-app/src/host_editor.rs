//! The host editor: a dialog over the vault for adding or changing a saved
//! host. It fills in a [`HostDraft`], which tern-core checks before the vault
//! saves anything.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::draft::{Field, HostDraft, Problem};
use tern_core::proxy::Proxy;
use tern_core::randomart::Randomart;
use tern_core::reach::Place;
use tern_core::secrets::host_password;
use tern_core::ssh::{KeyInfo, key_file_info, probe};
use tern_core::{Auth, Host, Vault};

use crate::live::LiveSlots;
use crate::marks::{self, Trace};
use crate::sync::secrets;
use crate::theme::{self, MONO, Theme};
use crate::ui::{Ui, art_tile, button, icon, ink_button, switch};

pub enum EditorEvent {
    /// Save `host` into `group`; `replacing` is its name before editing.
    /// `checked` is a Test connection result for the host's first hop.
    Save {
        replacing: Option<String>,
        host: Box<Host>,
        group: String,
        checked: Option<(Place, u32)>,
    },
    Delete(String),
    Close,
}

impl EventEmitter<EditorEvent> for HostEditor {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SignIn {
    Key,
    Password,
    Agent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Menu {
    Group,
    Jump,
}

/// The latest "Test connection" result.
enum Test {
    Idle,
    Running,
    /// `hop` names the jump host when that's what was checked; `at` is the
    /// address and port that answered.
    Reached {
        hop: Option<String>,
        latency_ms: u32,
        at: (String, u16),
    },
    Failed(SharedString),
}

pub struct HostEditor {
    /// A snapshot of the vault, for checks and choices.
    vault: Vault,
    /// The host's name before editing; `None` for a new host.
    replacing: Option<String>,
    host_art: Option<Randomart>,
    name: Entity<InputState>,
    address: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    tag: Entity<InputState>,
    on_login: Entity<InputState>,
    env: Entity<InputState>,
    /// The proxy's URL, used while `proxy_on`.
    proxy: Entity<InputState>,
    proxy_on: bool,
    /// For password sign-in: typed to save it, empty to keep the one saved.
    password: Entity<InputState>,
    /// Keep the password in the system's credential store.
    save_password: bool,
    /// The host already has a saved password.
    password_saved: bool,
    group: String,
    sign_in: SignIn,
    /// The key file for key sign-in, kept while another method is picked.
    key: Option<(PathBuf, Option<KeyInfo>)>,
    /// `Some("")` while switched on with no host chosen yet.
    jump: Option<String>,
    tags: Vec<String>,
    keep_alive: bool,
    agent_forwarding: bool,
    menu: Option<Menu>,
    test: Test,
    problems: Vec<Problem>,
    /// A one-line message that belongs to no field, like why a delete was
    /// refused.
    notice: Option<SharedString>,
    live: LiveSlots,
    focus: FocusHandle,
    _test: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl HostEditor {
    /// Edits the host called `editing`, or a new one in `group`. `host_art`
    /// is the art of the host's known key, if any.
    pub fn new(
        vault: Vault,
        editing: Option<&str>,
        group: &str,
        host_art: Option<Randomart>,
        live: LiveSlots,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let host = editing.and_then(|name| vault.find(name));
        let draft = match host {
            Some(host) => HostDraft::edit(host, vault.group_of(&host.name).unwrap_or(group)),
            None => HostDraft::new(group),
        };
        let name = text_input(&draft.name, "", window, cx);
        let address = text_input(&draft.address, "", window, cx);
        let port = text_input(&draft.port, "", window, cx);
        let user = text_input(&draft.user, "", window, cx);
        let tag = text_input("", "Add tag", window, cx);
        let on_login = text_input(&draft.on_login, "", window, cx);
        let env = text_input(&draft.env, "NAME=value", window, cx);
        let proxy = text_input(
            draft.proxy.as_deref().unwrap_or_default(),
            "socks5://host:1080",
            window,
            cx,
        );
        let password_saved =
            host.is_some_and(|host| secrets(cx).load(&host_password(&host.id)).is_some());
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(if password_saved {
                    "••••••••"
                } else {
                    ""
                })
                .masked(true)
        });

        let mut subscriptions: Vec<Subscription> = [&name, &address, &port, &user]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this: &mut Self, _, event: &InputEvent, cx| {
                    if let InputEvent::Change = event {
                        this.recheck(cx);
                        cx.notify();
                    }
                })
            })
            .collect();
        subscriptions.push(cx.subscribe_in(
            &tag,
            window,
            |this, tag, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let typed = tag.read(cx).value().to_string();
                    this.add_tag(&typed, cx);
                    tag.update(cx, |state, cx| state.set_value("", window, cx));
                }
            },
        ));
        name.update(cx, |state, cx| state.focus(window, cx));

        let (sign_in, key) = match &draft.auth {
            Auth::Key(path) => (SignIn::Key, Some((path.clone(), key_file_info(path)))),
            Auth::Password => (SignIn::Password, None),
            Auth::Agent => (SignIn::Agent, None),
        };
        Self {
            replacing: host.map(|host| host.name.clone()),
            vault,
            host_art,
            name,
            address,
            port,
            user,
            tag,
            on_login,
            env,
            proxy_on: draft.proxy.is_some(),
            proxy,
            password,
            save_password: password_saved,
            password_saved,
            group: draft.group,
            sign_in,
            key,
            jump: draft.jump,
            tags: draft.tags,
            keep_alive: draft.keep_alive,
            agent_forwarding: draft.agent_forwarding,
            menu: None,
            test: Test::Idle,
            problems: Vec::new(),
            notice: None,
            live,
            focus: cx.focus_handle(),
            _test: None,
            _subscriptions: subscriptions,
        }
    }

    /// The form as it stands.
    fn draft(&self, cx: &App) -> HostDraft {
        let text = |input: &Entity<InputState>| input.read(cx).value().to_string();
        HostDraft {
            name: text(&self.name),
            address: text(&self.address),
            port: text(&self.port),
            user: text(&self.user),
            group: self.group.clone(),
            auth: match self.sign_in {
                SignIn::Key => Auth::Key(
                    self.key
                        .as_ref()
                        .map(|(path, _)| path.clone())
                        .unwrap_or_default(),
                ),
                SignIn::Password => Auth::Password,
                SignIn::Agent => Auth::Agent,
            },
            jump: self.jump.clone(),
            proxy: self.proxy_on.then(|| text(&self.proxy)),
            tags: self.tags.clone(),
            keep_alive: self.keep_alive,
            agent_forwarding: self.agent_forwarding,
            on_login: text(&self.on_login),
            env: text(&self.env),
        }
    }

    /// Shows what's wrong with the host as saved, like a jump host that no
    /// longer exists.
    pub fn show_problems(&mut self, cx: &mut Context<Self>) {
        self.problems = self
            .draft(cx)
            .check(&self.vault, self.replacing.as_deref())
            .err()
            .unwrap_or_default();
        cx.notify();
    }

    #[cfg(test)]
    pub fn input(&self, field: Field) -> Entity<InputState> {
        match field {
            Field::Name => self.name.clone(),
            Field::Address => self.address.clone(),
            Field::Port => self.port.clone(),
            Field::User => self.user.clone(),
            Field::Env => self.env.clone(),
            Field::Proxy => self.proxy.clone(),
            Field::Key | Field::Jump => unreachable!("{field:?} isn't typed"),
        }
    }

    #[cfg(test)]
    pub fn problem_fields(&self) -> Vec<Field> {
        self.problems.iter().map(|problem| problem.field).collect()
    }

    /// After a refused save, keeps the problems current as they're fixed.
    fn recheck(&mut self, cx: &App) {
        self.notice = None;
        if !self.problems.is_empty() {
            self.problems = self
                .draft(cx)
                .check(&self.vault, self.replacing.as_deref())
                .err()
                .unwrap_or_default();
        }
    }

    pub fn save(&mut self, cx: &mut Context<Self>) {
        match self.draft(cx).check(&self.vault, self.replacing.as_deref()) {
            Ok(host) => {
                self.keep_password(&host, cx);
                // A check of the place the vault will check is fresh news.
                let first_hop = self.vault.first_hop(&host);
                let checked = match &self.test {
                    Test::Reached { latency_ms, at, .. } if *at == first_hop => {
                        Some((first_hop, *latency_ms))
                    }
                    _ => None,
                };
                cx.emit(EditorEvent::Save {
                    replacing: self.replacing.clone(),
                    host: Box::new(host),
                    group: self.group.clone(),
                    checked,
                });
            }
            Err(problems) => {
                self.problems = problems;
                self.notice = None;
                cx.notify();
            }
        }
    }

    /// Saves the typed password for `host` in the system's credential
    /// store, or forgets the saved one when the switch is off or the host
    /// no longer signs in with a password. Never in the vault file.
    fn keep_password(&mut self, host: &Host, cx: &mut Context<Self>) {
        let name = host_password(&host.id);
        let typed = self.password.read(cx).value().to_string();
        let store = secrets(cx);
        if host.auth == Auth::Password && self.save_password {
            if !typed.is_empty() {
                let _ = store.store(&name, &typed);
            }
        } else if self.password_saved {
            let _ = store.forget(&name);
        }
    }

    #[cfg(test)]
    pub fn password_input(&self) -> Entity<InputState> {
        self.password.clone()
    }

    #[cfg(test)]
    pub fn saves_password(&self) -> bool {
        self.save_password
    }

    fn delete(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.replacing.clone() else {
            return;
        };
        let dependents = self.vault.dependents(&name);
        if dependents.is_empty() {
            cx.emit(EditorEvent::Delete(name));
        } else {
            let verb = if dependents.len() == 1 { "goes" } else { "go" };
            self.notice =
                Some(format!("{} {verb} through this host", dependents.join(", ")).into());
            cx.notify();
        }
    }

    /// Checks the first hop: the first jump host when there are any,
    /// otherwise the host as typed.
    fn test_connection(&mut self, cx: &mut Context<Self>) {
        let draft = self.draft(cx);
        let via = draft.jump.as_deref().and_then(|name| self.vault.find(name));
        let via = via.map(|via| {
            self.vault
                .jumps(via)
                .ok()
                .and_then(|chain| chain.first().copied())
                .unwrap_or(via)
        });
        let (hop, address, port, proxy) = match via {
            Some(via) => (
                Some(via.name.clone()),
                via.address.clone(),
                via.port,
                via.proxy.clone(),
            ),
            None => {
                let proxy = match draft.proxy.as_deref().map(Proxy::parse) {
                    Some(Err(problem)) => {
                        self.test = Test::Failed(problem.into());
                        cx.notify();
                        return;
                    }
                    parsed => parsed.and_then(Result::ok),
                };
                let port = draft
                    .port
                    .trim()
                    .parse::<u16>()
                    .ok()
                    .filter(|&port| port > 0);
                let address = draft.address.trim();
                match port {
                    Some(port) if !address.is_empty() => (None, address.to_string(), port, proxy),
                    _ => {
                        self.test = Test::Failed("enter a host and port first".into());
                        cx.notify();
                        return;
                    }
                }
            }
        };
        self.test = Test::Running;
        let check = probe(&address, port, proxy);
        self._test = Some(cx.spawn(async move |this, cx| {
            let result = check.await;
            let _ = this.update(cx, |editor, cx| {
                editor.test = match result {
                    Ok(reached) => Test::Reached {
                        hop,
                        latency_ms: reached.latency_ms,
                        at: (address, port),
                    },
                    Err(reason) => Test::Failed(match hop {
                        Some(hop) => format!("{hop}: {reason}").into(),
                        None => reason.into(),
                    }),
                };
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn set_sign_in(&mut self, sign_in: SignIn, cx: &mut Context<Self>) {
        self.sign_in = sign_in;
        if sign_in == SignIn::Key && self.key.is_none() {
            self.key = default_key().map(|path| {
                let info = key_file_info(&path);
                (path, info)
            });
        }
        self.recheck(cx);
        cx.notify();
    }

    fn choose_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Use key".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = chosen.await
                && let Some(path) = paths.into_iter().next()
            {
                let info = key_file_info(&path);
                let _ = this.update(cx, |editor, cx| {
                    editor.key = Some((path, info));
                    editor.recheck(cx);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn pick(&mut self, menu: Menu, value: String, cx: &mut Context<Self>) {
        match menu {
            Menu::Group => self.group = value,
            Menu::Jump => self.jump = Some(value),
        }
        self.menu = None;
        self.recheck(cx);
        cx.notify();
    }

    fn add_tag(&mut self, tag: &str, cx: &mut Context<Self>) {
        let tag = tag.trim();
        if !tag.is_empty() && !self.tags.iter().any(|have| have.eq_ignore_ascii_case(tag)) {
            self.tags.push(tag.to_string());
        }
        cx.notify();
    }

    fn header(&self, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let typed = self.name.read(cx).value();
        let title: SharedString = if typed.trim().is_empty() {
            "New host".into()
        } else {
            typed
        };
        div()
            .flex()
            .items_center()
            .gap(px(14.))
            .child(art_tile(self.host_art.clone(), 46., 14., t))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(20.))
                    .font_weight(FontWeight::BOLD)
                    .truncate()
                    .child(title),
            )
            .child(
                button("close-editor", t)
                    .debug_selector(|| "close-editor".into())
                    .size(px(40.))
                    .px_0()
                    .justify_center()
                    .child(icon(IconName::Close, 14., t.text2))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(EditorEvent::Close))),
            )
    }

    /// Where the host is: name, address, port and group.
    fn place_card(&self, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let groups = self
            .vault
            .groups
            .iter()
            .map(|group| (group.name.clone(), None))
            .collect();
        card(t)
            .child(self.field("Name", &self.name, Some(Field::Name), false, t))
            .child(
                div()
                    .flex()
                    .gap(px(10.))
                    .child(div().flex_1().min_w_0().child(self.field(
                        "Host",
                        &self.address,
                        Some(Field::Address),
                        true,
                        t,
                    )))
                    .child(div().w(px(92.)).flex_none().child(self.field(
                        "Port",
                        &self.port,
                        Some(Field::Port),
                        true,
                        t,
                    ))),
            )
            .child(labelled("Group", t).child(self.picker(
                "group-picker",
                Menu::Group,
                self.group.clone().into(),
                Some(self.group.clone()),
                groups,
                false,
                t,
                cx,
            )))
    }

    /// Who to sign in as, and how.
    fn sign_in_card(&self, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let methods = [
            (SignIn::Key, "SSH key", "sign-in-key"),
            (SignIn::Password, "Password", "sign-in-password"),
            (SignIn::Agent, "SSH agent", "sign-in-agent"),
        ];
        card(t)
            .child(self.field("Username", &self.user, Some(Field::User), true, t))
            .child(
                div()
                    .flex()
                    .gap(px(4.))
                    .p(px(4.))
                    .rounded(px(14.))
                    .bg(t.card2)
                    .children(methods.into_iter().map(|(method, label, id)| {
                        let on = self.sign_in == method;
                        div()
                            .id(id)
                            .debug_selector(move || id.into())
                            .flex_1()
                            .h(px(36.))
                            .flex()
                            .items_center()
                            .justify_center()
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
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.set_sign_in(method, cx)),
                            )
                    })),
            )
            .when(self.sign_in == SignIn::Key, |d| {
                d.child(self.key_row(t, cx))
            })
            .when(self.sign_in == SignIn::Password, |d| {
                d.child(self.field("Password", &self.password, None, true, t))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .min_h(px(36.))
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Save in the system keychain"),
                            )
                            .child(
                                switch("save-password", self.save_password, t)
                                    .debug_selector(|| "save-password".into())
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_password = !this.save_password;
                                        cx.notify();
                                    })),
                            ),
                    )
            })
    }

    fn key_row(&self, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let bad = self.has_problem(Field::Key);
        let info = self.key.as_ref().and_then(|(_, info)| info.as_ref());
        let name = self.key.as_ref().map(|(path, _)| file_name(path));
        let kind = self.key.as_ref().map(|(_, info)| match info {
            Some(info) => info.kind.clone(),
            None => "unreadable".into(),
        });
        div()
            .flex()
            .items_center()
            .gap(px(12.))
            .py(px(10.))
            .pl(px(12.))
            .pr(px(10.))
            .rounded(px(16.))
            .border_1()
            .border_color(if bad { t.danger } else { t.line })
            .child(art_tile(info.map(|info| info.art.clone()), 38., 12., t))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .when_some(name, |d, name| {
                        d.child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .truncate()
                                .child(name),
                        )
                    })
                    .when_some(kind, |d, kind| {
                        d.child(
                            div()
                                .font_family(MONO)
                                .text_size(px(11.))
                                .text_color(t.text2)
                                .child(kind),
                        )
                    }),
            )
            .child(
                button("choose-key", t)
                    .child(if self.key.is_some() {
                        "Change"
                    } else {
                        "Choose"
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.choose_key(window, cx))),
            )
    }

    /// Going through a jump host, and the route it makes.
    fn jump_card(&self, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let on = self.jump.is_some();
        let chosen = self.jump.as_deref().and_then(|name| self.vault.find(name));
        let value: SharedString = chosen
            .map(|via| format!("{} · {}", via.name, via.target()).into())
            .unwrap_or_default();
        // A host can't go through one that already goes through it.
        let choices = self
            .vault
            .hosts()
            .filter(|host| {
                self.replacing
                    .as_deref()
                    .is_none_or(|this| !self.vault.goes_through(host, this))
            })
            .map(|host| (host.name.clone(), Some(host.target())))
            .collect();
        // The whole route: the chosen host's own jump hosts come first.
        let hops = on.then(|| {
            chosen
                .map(|via| {
                    let mut hops: Vec<String> = self
                        .vault
                        .jumps(via)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|hop| hop.name.clone())
                        .collect();
                    hops.push(via.name.clone());
                    hops
                })
                .unwrap_or_default()
        });
        // The proxy dials the first hop: this host's own, or the first jump
        // host's.
        let proxy: Option<Option<String>> = match chosen {
            Some(via) => self
                .vault
                .first_hop_host(via)
                .proxy
                .as_ref()
                .map(|proxy| Some(proxy.host.clone())),
            None => self.proxy_on.then(|| {
                Proxy::parse(&self.proxy.read(cx).value())
                    .ok()
                    .map(|proxy| proxy.host)
            }),
        };
        card(t)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Jump host"),
                    )
                    .child(
                        switch("jump-switch", on, t)
                            .debug_selector(|| "jump-switch".into())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.jump = match this.jump {
                                    Some(_) => None,
                                    None => Some(String::new()),
                                };
                                this.recheck(cx);
                                cx.notify();
                            })),
                    ),
            )
            .when(on, |d| {
                d.child(self.picker(
                    "jump-picker",
                    Menu::Jump,
                    value,
                    self.jump.clone(),
                    choices,
                    self.has_problem(Field::Jump),
                    t,
                    cx,
                ))
            })
            // A host reached through a jump host doesn't dial out itself, so
            // its own proxy only matters without one.
            .when(!on, |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(
                            div()
                                .flex_1()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Proxy"),
                        )
                        .child(
                            switch("proxy-switch", self.proxy_on, t)
                                .debug_selector(|| "proxy-switch".into())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.proxy_on = !this.proxy_on;
                                    this.recheck(cx);
                                    cx.notify();
                                })),
                        ),
                )
                .when(self.proxy_on, |d| {
                    d.child(div().child(self.field_body(&self.proxy, Some(Field::Proxy), true, t)))
                })
            })
            .when(on || proxy.is_some(), |d| {
                d.child(self.route(proxy, hops, t, cx))
            })
    }

    /// This computer → the proxy → each jump host → this host. A blank
    /// stop stands for a jump host or proxy not given yet.
    fn route(
        &self,
        proxy: Option<Option<String>>,
        hops: Option<Vec<String>>,
        t: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let node = |glyph: IconName, label: Option<SharedString>, target: bool| {
            div()
                .w(px(86.))
                .flex_shrink(1.)
                .min_w_0()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(6.))
                .child(
                    div()
                        .size(px(42.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .shadow(t.shadow_sm())
                        .bg(if target { t.text } else { t.card })
                        .child(icon(glyph, 18., if target { t.card } else { t.text2 })),
                )
                .when_some(label, |d, label| {
                    d.child(
                        div()
                            .max_w_full()
                            .text_size(px(12.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(label),
                    )
                })
        };
        // Stops narrow on a long route, but the lines between them stay.
        let link = || {
            div()
                .flex_1()
                .min_w(px(14.))
                .h(px(2.))
                .mt(px(20.))
                .bg(t.track)
        };
        let mut stops: Vec<(IconName, Option<String>)> = Vec::new();
        if let Some(proxy) = proxy {
            stops.push((IconName::Globe, proxy));
        }
        match hops {
            Some(hops) if hops.is_empty() => stops.push((IconName::Shield, None)),
            Some(hops) => stops.extend(hops.into_iter().map(|hop| (IconName::Shield, Some(hop)))),
            None => {}
        }
        let typed = self.name.read(cx).value();
        let this_host: SharedString = if typed.trim().is_empty() {
            "New host".into()
        } else {
            typed
        };
        div()
            .flex()
            .items_start()
            .pt(px(14.))
            .px(px(10.))
            .pb(px(12.))
            .rounded(px(16.))
            .bg(t.card2)
            .child(node(IconName::Laptop, None, false))
            .children(stops.into_iter().flat_map(|(glyph, label)| {
                [
                    link().into_any_element(),
                    node(glyph, label.map(Into::into), false).into_any_element(),
                ]
            }))
            .child(link())
            .child(node(IconName::Server, Some(this_host), true))
    }

    fn tags_card(&self, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let suggestions = self.vault.tag_suggestions(&self.tags, 3);
        card(t)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(px(6.))
                    .min_h(px(44.))
                    .p(px(6.))
                    .rounded(px(12.))
                    .bg(t.card2)
                    .children(self.tags.iter().map(|tag| {
                        let gone = tag.clone();
                        div()
                            .h(px(32.))
                            .pl(px(12.))
                            .pr(px(4.))
                            .flex()
                            .items_center()
                            .gap(px(2.))
                            .rounded(px(10.))
                            .bg(t.card)
                            .shadow(t.shadow_sm())
                            .font_family(MONO)
                            .text_size(px(12.))
                            .child(tag.clone())
                            .child(
                                div()
                                    .id(SharedString::from(format!("remove-tag-{tag}")))
                                    .size(px(26.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(8.))
                                    .cursor_pointer()
                                    .child(icon(IconName::Close, 11., t.text3))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.tags.retain(|have| *have != gone);
                                        cx.notify();
                                    })),
                            )
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(100.))
                            .px(px(6.))
                            .font_family(MONO)
                            .text_size(px(13.))
                            .child(Input::new(&self.tag).appearance(false)),
                    ),
            )
            .when(!suggestions.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(6.))
                        .children(suggestions.into_iter().map(|tag| {
                            let id = format!("suggest-{tag}");
                            let label = format!("+ {tag}");
                            div()
                                .id(SharedString::from(id.clone()))
                                .debug_selector(move || id)
                                .h(px(30.))
                                .px(px(10.))
                                .flex()
                                .items_center()
                                .rounded(px(9.))
                                .border_1()
                                .border_dashed()
                                .border_color(t.text3.opacity(0.5))
                                .text_color(t.text2)
                                .font_family(MONO)
                                .text_size(px(12.))
                                .cursor_pointer()
                                .child(label)
                                .on_click(cx.listener(move |this, _, _, cx| this.add_tag(&tag, cx)))
                        })),
                )
            })
    }

    fn options_card(&self, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        card(t)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .min_h(px(36.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Keep connection alive"),
                    )
                    .child(
                        switch("keep-alive-switch", self.keep_alive, t)
                            .debug_selector(|| "keep-alive-switch".into())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.keep_alive = !this.keep_alive;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .min_h(px(36.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Forward the SSH agent"),
                    )
                    .child(
                        switch("agent-switch", self.agent_forwarding, t)
                            .debug_selector(|| "agent-switch".into())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.agent_forwarding = !this.agent_forwarding;
                                cx.notify();
                            })),
                    ),
            )
            .child(self.field("Run after login", &self.on_login, None, true, t))
            .child(self.field("Environment", &self.env, Some(Field::Env), true, t))
    }

    fn footer(&self, ui: &Ui, cx: &Context<Self>) -> impl IntoElement {
        let t = ui.theme;
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .child(
                button("test-connection", t)
                    .debug_selector(|| "test-connection".into())
                    .h(px(44.))
                    .px(px(16.))
                    .rounded(px(14.))
                    .text_size(px(14.))
                    .child(icon(IconName::Zap, 16., t.text))
                    .child("Test connection")
                    .on_click(cx.listener(|this, _, _, cx| this.test_connection(cx))),
            )
            .child(self.test_result(ui))
            .child(div().flex_1())
            .when(self.replacing.is_some(), |d| {
                d.child(
                    div()
                        .id("delete-host")
                        .debug_selector(|| "delete-host".into())
                        .h(px(44.))
                        .px(px(14.))
                        .flex()
                        .items_center()
                        .text_color(t.danger)
                        .font_weight(FontWeight::SEMIBOLD)
                        .cursor_pointer()
                        .child("Delete host")
                        .on_click(cx.listener(|this, _, _, cx| this.delete(cx))),
                )
            })
            .child(
                button("cancel-editor", t)
                    .h(px(44.))
                    .px(px(18.))
                    .rounded(px(14.))
                    .text_size(px(14.))
                    .child("Cancel")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(EditorEvent::Close))),
            )
            .child(
                ink_button("save-host", t)
                    .debug_selector(|| "save-host".into())
                    .h(px(44.))
                    .px(px(22.))
                    .rounded(px(14.))
                    .text_size(px(14.))
                    .child("Save host")
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
            )
    }

    fn test_result(&self, ui: &Ui) -> AnyElement {
        let t = ui.theme;
        match &self.test {
            Test::Idle => div().into_any_element(),
            Test::Running => icon(IconName::LoaderCircle, 14., t.text2).into_any_element(),
            Test::Reached {
                hop, latency_ms, ..
            } => div()
                .flex()
                .items_center()
                .gap(px(10.))
                .when_some(hop.clone(), |d, hop| {
                    d.child(div().text_size(px(13.)).text_color(t.text2).child(hop))
                })
                .child(
                    ui.trace(
                        "editor-test",
                        Trace::single(*latency_ms),
                        marks::seed("editor-test"),
                        t.text,
                        t.text3,
                    )
                    .w(px(72.))
                    .h(px(18.)),
                )
                .child(
                    div()
                        .font_family(MONO)
                        .text_size(px(12.))
                        .child(format!("{latency_ms} ms")),
                )
                .into_any_element(),
            Test::Failed(reason) => div()
                .text_size(px(13.))
                .text_color(t.danger)
                .child(reason.clone())
                .into_any_element(),
        }
    }

    fn has_problem(&self, field: Field) -> bool {
        self.problems.iter().any(|problem| problem.field == field)
    }

    /// A labelled text field, outlined in red while it has a problem.
    fn field(
        &self,
        label: &'static str,
        input: &Entity<InputState>,
        field: Option<Field>,
        mono: bool,
        t: &Theme,
    ) -> Div {
        labelled(label, t).child(self.field_body(input, field, mono, t))
    }

    /// A text field's box, outlined in red while it has a problem.
    fn field_body(
        &self,
        input: &Entity<InputState>,
        field: Option<Field>,
        mono: bool,
        t: &Theme,
    ) -> Div {
        let bad = field.is_some_and(|field| self.has_problem(field));
        div()
            .h(px(44.))
            .px(px(10.))
            .flex()
            .items_center()
            .rounded(px(12.))
            .bg(t.card2)
            .border_1()
            .border_color(if bad { t.danger } else { t.card2 })
            .when(mono, |d| d.font_family(MONO))
            .child(Input::new(input).appearance(false))
    }

    /// A field that opens a list to choose from. `shown` is the field's text;
    /// options are a value and an optional detail shown beside it.
    #[allow(clippy::too_many_arguments)]
    fn picker(
        &self,
        id: &'static str,
        menu: Menu,
        shown: SharedString,
        chosen: Option<String>,
        options: Vec<(String, Option<String>)>,
        bad: bool,
        t: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let open = self.menu == Some(menu);
        let value = shown;
        div()
            .child(
                div()
                    .id(id)
                    .debug_selector(move || id.into())
                    .h(px(44.))
                    .px(px(14.))
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.))
                    .rounded(px(12.))
                    .bg(t.card2)
                    .border_1()
                    .border_color(if bad { t.danger } else { t.card2 })
                    .cursor_pointer()
                    .child(div().flex_1().min_w_0().truncate().child(value))
                    .child(icon(IconName::ChevronDown, 16., t.text3))
                    // Opens only: a click outside the list (this one too)
                    // closes it first.
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.menu = Some(menu);
                        cx.notify();
                    })),
            )
            .when(open, |d| {
                let hover = t.card2;
                d.child(deferred(
                    anchored().snap_to_window_with_margin(px(8.)).child(
                        div()
                            .id("picker-menu")
                            .occlude()
                            .mt(px(6.))
                            .min_w(px(280.))
                            .p(px(6.))
                            .flex()
                            .flex_col()
                            .rounded(px(14.))
                            .bg(t.card)
                            .border_1()
                            .border_color(t.line)
                            .shadow(t.shadow())
                            .text_color(t.text)
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.menu = None;
                                cx.notify();
                            }))
                            .children(options.into_iter().map(|(option, detail)| {
                                let chosen = chosen.as_deref() == Some(option.as_str());
                                let selector = format!("{id}-{option}");
                                let pick = option.clone();
                                div()
                                    .id(SharedString::from(selector.clone()))
                                    .debug_selector(move || selector)
                                    .h(px(36.))
                                    .px(px(10.))
                                    .flex()
                                    .items_center()
                                    .gap(px(10.))
                                    .rounded(px(10.))
                                    .cursor_pointer()
                                    .hover(move |s| s.bg(hover))
                                    .child(div().flex_1().truncate().child(option))
                                    .when_some(detail, |d, detail| {
                                        d.child(
                                            div()
                                                .font_family(MONO)
                                                .text_size(px(12.))
                                                .text_color(t.text3)
                                                .child(detail),
                                        )
                                    })
                                    .child(div().w(px(14.)).when(chosen, |d| {
                                        d.child(icon(IconName::Check, 14., t.text))
                                    }))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.pick(menu, pick.clone(), cx)
                                    }))
                            })),
                    ),
                ))
            })
    }
}

impl Focusable for HostEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for HostEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme: Rc<Theme> = theme::current(cx);
        let t = &theme;
        let ui = Ui {
            theme: &theme,
            live: &self.live,
        };
        let message = self.notice.clone().or_else(|| {
            self.problems
                .first()
                .map(|problem| problem.message.clone().into())
        });
        div()
            .id("host-editor")
            .key_context("HostEditor")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    if this.menu.take().is_some() {
                        cx.notify();
                    } else {
                        cx.emit(EditorEvent::Close);
                    }
                    cx.stop_propagation();
                }
            }))
            .w(px(860.))
            .max_w_full()
            .max_h_full()
            .flex()
            .flex_col()
            .gap(px(16.))
            .py(px(20.))
            .px(px(24.))
            .rounded(px(28.))
            .bg(t.win)
            .border_1()
            .border_color(t.line)
            .shadow(t.shadow_lg())
            .text_color(t.text)
            .text_size(px(14.))
            .child(self.header(t, cx))
            .child(
                div()
                    .id("host-editor-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(14.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(14.))
                                    .child(self.place_card(t, cx))
                                    .child(self.sign_in_card(t, cx)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(14.))
                                    .child(self.jump_card(t, cx))
                                    .child(self.tags_card(t, cx))
                                    .child(self.options_card(t, cx)),
                            ),
                    ),
            )
            .when_some(message, |d, message| {
                d.child(
                    div()
                        .text_size(px(13.))
                        .text_color(t.danger)
                        .truncate()
                        .child(message),
                )
            })
            .child(self.footer(&ui, cx))
    }
}

fn text_input(
    value: &str,
    placeholder: &'static str,
    window: &mut Window,
    cx: &mut Context<HostEditor>,
) -> Entity<InputState> {
    let value = value.to_string();
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(value)
    })
}

fn card(t: &Theme) -> Div {
    div()
        .p(px(14.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .rounded(px(22.))
        .bg(t.card)
        .shadow(t.shadow())
}

fn labelled(label: &'static str, t: &Theme) -> Div {
    div().flex().flex_col().gap(px(6.)).child(
        div()
            .text_size(px(12.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(t.text2)
            .child(label),
    )
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The key OpenSSH would use first, if there is one.
fn default_key() -> Option<PathBuf> {
    let ssh = std::env::home_dir()?.join(".ssh");
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .into_iter()
        .map(|name| ssh.join(name))
        .find(|path| path.exists())
}
