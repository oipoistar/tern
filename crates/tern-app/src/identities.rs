//! The Identities half of the Keychain screen: users and sign-ins that
//! hosts share, as cards. A form at the top adds or edits one; the vault
//! saves what it sends, and keeps a password in the system's credential
//! store, never in the vault file.

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::Auth;
use tern_core::identity::{self, Identity};

use crate::theme::{self, MONO, Theme};
use crate::ui::{button, icon, ink_button, switch};

pub enum IdentityEvent {
    /// Save `identity`, already checked. `password` is one typed to keep
    /// (with `save_password`), or none to leave the saved one as it is.
    Save {
        identity: Identity,
        password: Option<String>,
        save_password: bool,
    },
    /// Delete the identity with this id.
    Delete(String),
}

impl EventEmitter<IdentityEvent> for IdentitiesView {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Method {
    Agent,
    Key,
    Password,
}

/// The add or edit form.
struct Form {
    /// The id of the identity being edited.
    replacing: Option<String>,
    name: Entity<InputState>,
    user: Entity<InputState>,
    key: Entity<InputState>,
    password: Entity<InputState>,
    method: Method,
    save_password: bool,
    problem: Option<SharedString>,
}

pub struct IdentitiesView {
    identities: Vec<Identity>,
    form: Option<Form>,
}

impl IdentitiesView {
    pub fn new(identities: Vec<Identity>) -> Self {
        Self {
            identities,
            form: None,
        }
    }

    /// Takes the vault's identities after a change and closes the form.
    pub fn refresh(&mut self, identities: Vec<Identity>, cx: &mut Context<Self>) {
        self.identities = identities;
        self.form = None;
        cx.notify();
    }

    /// A save or delete the vault refused, shown in the form.
    pub fn refuse(&mut self, problem: String, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.problem = Some(problem.into());
        }
        cx.notify();
    }

    /// The form's fields, for tests: name, user, key file, password.
    #[cfg(test)]
    pub fn inputs(&self) -> Option<[Entity<InputState>; 4]> {
        self.form.as_ref().map(|form| {
            [
                form.name.clone(),
                form.user.clone(),
                form.key.clone(),
                form.password.clone(),
            ]
        })
    }

    /// Opens the form on the identity with `id`, or a new one.
    pub fn open_form(&mut self, id: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let editing = id
            .as_deref()
            .and_then(|id| self.identities.iter().find(|identity| identity.id == id));
        let input = |value: &str,
                     placeholder: &'static str,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            let value = value.to_string();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let (method, key) = match editing.map(|identity| &identity.auth) {
            Some(Auth::Key(path)) => (Method::Key, path.display().to_string()),
            Some(Auth::Password) => (Method::Password, String::new()),
            _ => (Method::Agent, String::new()),
        };
        let name = input(
            editing.map_or("", |identity| &identity.name),
            "",
            window,
            cx,
        );
        let user = input(
            editing.map_or("", |identity| &identity.user),
            "",
            window,
            cx,
        );
        let key = input(&key, "~/.ssh/id_ed25519", window, cx);
        let password = cx.new(|cx| InputState::new(window, cx).masked(true));
        name.update(cx, |state, cx| state.focus(window, cx));
        self.form = Some(Form {
            replacing: editing.map(|identity| identity.id.clone()),
            name,
            user,
            key,
            password,
            method,
            save_password: true,
            problem: None,
        });
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        let text = |input: &Entity<InputState>| input.read(cx).value().to_string();
        let auth = match form.method {
            Method::Agent => Auth::Agent,
            Method::Password => Auth::Password,
            Method::Key => {
                let typed = text(&form.key);
                let typed = typed.trim();
                let path = match (typed.strip_prefix("~/"), std::env::home_dir()) {
                    (Some(rest), Some(home)) => home.join(rest),
                    _ => PathBuf::from(typed),
                };
                Auth::Key(path)
            }
        };
        match identity::check(
            &self.identities,
            form.replacing.as_deref(),
            &text(&form.name),
            &text(&form.user),
            auth,
        ) {
            Ok(identity) => {
                let typed = text(&form.password);
                cx.emit(IdentityEvent::Save {
                    password: (!typed.is_empty()).then_some(typed),
                    save_password: form.save_password,
                    identity,
                });
            }
            Err(problem) => self.refuse(problem, cx),
        }
    }

    fn form_card(&self, form: &Form, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let field = |label: &'static str, input: &Entity<InputState>, mono: bool| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.))
                .child(
                    div()
                        .text_size(px(12.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.text2)
                        .child(label),
                )
                .child(
                    div()
                        .h(px(44.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .rounded(px(12.))
                        .bg(t.card2)
                        .when(mono, |d| d.font_family(MONO))
                        .child(Input::new(input).appearance(false)),
                )
        };
        let methods = [
            (Method::Agent, "SSH agent", "identity-agent"),
            (Method::Key, "SSH key", "identity-key"),
            (Method::Password, "Password", "identity-password"),
        ];
        div()
            .p(px(14.))
            .flex()
            .flex_col()
            .gap(px(12.))
            .rounded(px(22.))
            .bg(t.card)
            .shadow(t.shadow())
            .child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(div().flex_1().child(field("Name", &form.name, false)))
                    .child(div().flex_1().child(field("Username", &form.user, true))),
            )
            .child(
                div()
                    .flex()
                    .gap(px(4.))
                    .p(px(4.))
                    .rounded(px(14.))
                    .bg(t.card2)
                    .children(methods.into_iter().map(|(method, label, id)| {
                        let on = form.method == method;
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
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(form) = &mut this.form {
                                    form.method = method;
                                }
                                cx.notify();
                            }))
                    })),
            )
            .when(form.method == Method::Key, |d| {
                d.child(field("Key file", &form.key, true))
            })
            .when(form.method == Method::Password, |d| {
                d.child(field("Password", &form.password, true)).child(
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
                            switch("identity-save-password", form.save_password, t)
                                .debug_selector(|| "identity-save-password".into())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(form) = &mut this.form {
                                        form.save_password = !form.save_password;
                                    }
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when_some(form.problem.clone(), |d, problem| {
                d.child(div().text_size(px(13.)).text_color(t.danger).child(problem))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        ink_button("save-identity", t)
                            .debug_selector(|| "save-identity".into())
                            .h(px(40.))
                            .px(px(18.))
                            .rounded(px(13.))
                            .text_size(px(13.))
                            .child("Save")
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    )
                    .child(
                        button("cancel-identity", t)
                            .debug_selector(|| "cancel-identity".into())
                            .h(px(40.))
                            .px(px(16.))
                            .rounded(px(13.))
                            .text_size(px(13.))
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.form = None;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .when_some(form.replacing.clone(), |d, id| {
                        d.child(
                            div()
                                .id("delete-identity")
                                .debug_selector(|| "delete-identity".into())
                                .h(px(40.))
                                .px(px(12.))
                                .flex()
                                .items_center()
                                .text_size(px(13.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(t.danger)
                                .cursor_pointer()
                                .child("Delete identity")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(IdentityEvent::Delete(id.clone()))
                                })),
                        )
                    }),
            )
    }

    fn card(
        &self,
        index: usize,
        identity: &Identity,
        t: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let id = format!("identity-{index}");
        let editing = identity.id.clone();
        let (glyph, method) = match &identity.auth {
            Auth::Agent => (IconName::Shield, "agent".to_string()),
            Auth::Key(path) => (
                IconName::KeyRound,
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            Auth::Password => (IconName::Lock, "password".to_string()),
        };
        div()
            .id(SharedString::from(id.clone()))
            .debug_selector(move || id)
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
                    .size(px(40.))
                    .flex_none()
                    .rounded(px(12.))
                    .border_1()
                    .border_color(t.line)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon(glyph, 16., t.text2)),
            )
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
                            .child(identity.name.clone()),
                    )
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(px(12.))
                            .text_color(t.text2)
                            .truncate()
                            .child(format!("{} · {method}", identity.user)),
                    ),
            )
            .child(icon(IconName::Pencil, 15., t.text3))
    }
}

impl Render for IdentitiesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let t = &theme;
        div()
            .id("identities")
            .debug_selector(|| "identities".into())
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                div().flex().justify_end().child(
                    ink_button("new-identity", t)
                        .debug_selector(|| "new-identity".into())
                        .h(px(46.))
                        .px(px(20.))
                        .rounded(px(16.))
                        .text_size(px(15.))
                        .child(icon(IconName::Plus, 16., t.card))
                        .child("New identity")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_form(None, window, cx)),
                        ),
                ),
            )
            .when_some(self.form.as_ref(), |d, form| {
                d.child(self.form_card(form, t, cx))
            })
            .children(
                self.identities
                    .iter()
                    .enumerate()
                    .map(|(index, identity)| self.card(index, identity, t, cx)),
            )
    }
}
