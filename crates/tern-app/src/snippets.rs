//! The Snippets screen: saved commands, each a name and the command. A
//! form at the top adds or edits one; the vault saves what it sends.

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::snippet::{self, Snippet};

use crate::theme::{self, MONO, Theme};
use crate::ui::{button, icon, ink_button, search_field};

pub enum SnippetEvent {
    /// Save `snippet`, already checked; `replacing` is its name before
    /// editing.
    Save {
        replacing: Option<String>,
        snippet: Snippet,
    },
    Delete(String),
}

impl EventEmitter<SnippetEvent> for SnippetsView {}

/// The add or edit form.
struct Form {
    replacing: Option<String>,
    name: Entity<InputState>,
    command: Entity<InputState>,
    problem: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

pub struct SnippetsView {
    /// The vault's snippets, as last saved.
    snippets: Vec<Snippet>,
    search: Entity<InputState>,
    query: SharedString,
    form: Option<Form>,
    _subscription: Subscription,
}

impl SnippetsView {
    pub fn new(snippets: Vec<Snippet>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let subscription = cx.subscribe(&search, |this: &mut Self, search, event, cx| {
            if let InputEvent::Change = event {
                this.query = search.read(cx).value();
                cx.notify();
            }
        });
        Self {
            snippets,
            search,
            query: SharedString::default(),
            form: None,
            _subscription: subscription,
        }
    }

    /// Takes the vault's snippets after a change and closes the form.
    pub fn refresh(&mut self, snippets: Vec<Snippet>, cx: &mut Context<Self>) {
        self.snippets = snippets;
        self.form = None;
        cx.notify();
    }

    /// The form's name field (`first`) or command field.
    #[cfg(test)]
    pub fn input(&self, first: bool) -> Entity<InputState> {
        let form = self.form.as_ref().expect("the form is open");
        if first {
            form.name.clone()
        } else {
            form.command.clone()
        }
    }

    /// Opens the form on the snippet called `editing`, or on a new one.
    pub fn open_form(
        &mut self,
        editing: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let saved = editing
            .as_deref()
            .and_then(|name| self.snippets.iter().find(|snippet| snippet.name == name))
            .cloned();
        let (name_text, command_text) = saved
            .map(|snippet| (snippet.name, snippet.command))
            .unwrap_or_default();
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Name")
                .default_value(name_text)
        });
        let command = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Command")
                .default_value(command_text)
        });
        // Enter in either field saves.
        let subscriptions = [&name, &command]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this: &mut Self, _, event: &InputEvent, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.save(cx);
                    }
                })
            })
            .collect();
        name.update(cx, |state, cx| state.focus(window, cx));
        self.form = Some(Form {
            replacing: editing,
            name,
            command,
            problem: None,
            _subscriptions: subscriptions,
        });
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.form else {
            return;
        };
        let typed = Snippet {
            name: form.name.read(cx).value().to_string(),
            command: form.command.read(cx).value().to_string(),
        };
        match snippet::check(&self.snippets, form.replacing.as_deref(), &typed) {
            Ok(snippet) => cx.emit(SnippetEvent::Save {
                replacing: form.replacing.clone(),
                snippet,
            }),
            Err(problem) => form.problem = Some(problem.into()),
        }
        cx.notify();
    }

    fn form_card(&self, form: &Form, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let field = |input: &Entity<InputState>, mono: bool| {
            div()
                .h(px(44.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(px(12.))
                .bg(t.card2)
                .when(mono, |d| d.font_family(MONO))
                .child(Input::new(input).appearance(false))
        };
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
                    .child(field(&form.name, false).w(px(220.)).flex_none())
                    .child(field(&form.command, true).flex_1().min_w_0())
                    .when_some(form.replacing.clone(), |d, name| {
                        d.child(
                            div()
                                .id("delete-snippet")
                                .debug_selector(|| "delete-snippet".into())
                                .h(px(44.))
                                .px(px(10.))
                                .flex()
                                .items_center()
                                .text_color(t.danger)
                                .font_weight(FontWeight::SEMIBOLD)
                                .cursor_pointer()
                                .child("Delete")
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(SnippetEvent::Delete(name.clone()))
                                })),
                        )
                    })
                    .child(
                        button("cancel-snippet", t)
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
                        ink_button("save-snippet", t)
                            .debug_selector(|| "save-snippet".into())
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
        index: usize,
        snippet: &Snippet,
        t: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let id = format!("snippet-row-{index}");
        let name = snippet.name.clone();
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
                this.open_form(Some(name.clone()), window, cx)
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
                    .child(icon(IconName::CodeXml, 16., t.text2)),
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
                            .child(snippet.name.clone()),
                    )
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(px(12.))
                            .text_color(t.text2)
                            .truncate()
                            .child(snippet.command.clone()),
                    ),
            )
            .child(icon(IconName::Pencil, 15., t.text3))
    }
}

impl Render for SnippetsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let t = &theme;
        div()
            .id("snippets")
            .debug_selector(|| "snippets".into())
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(14.))
            .child(
                div()
                    .mb(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(search_field("snippets-search", &self.search, t))
                    .child(
                        ink_button("new-snippet-screen", t)
                            .debug_selector(|| "new-snippet-screen".into())
                            .h(px(46.))
                            .px(px(20.))
                            .rounded(px(16.))
                            .text_size(px(15.))
                            .child(icon(IconName::Plus, 16., t.card))
                            .child("New snippet")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_form(None, window, cx)),
                            ),
                    ),
            )
            .when_some(self.form.as_ref(), |d, form| {
                d.child(self.form_card(form, t, cx))
            })
            .children(
                self.snippets
                    .iter()
                    .enumerate()
                    .filter(|(_, snippet)| snippet.matches(&self.query))
                    .map(|(index, snippet)| self.row(index, snippet, t, cx)),
            )
    }
}
