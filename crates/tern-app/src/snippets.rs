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
    /// Run `snippet` on each of the saved `hosts`, side by side.
    Run {
        snippet: Snippet,
        hosts: Vec<String>,
    },
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

/// Choosing hosts to run a snippet on.
struct Run {
    snippet: Snippet,
    chosen: Vec<String>,
    /// A field for each of the snippet's `{{name}}` variables.
    values: Vec<(String, Entity<InputState>)>,
}

pub struct SnippetsView {
    /// The vault's snippets, as last saved.
    snippets: Vec<Snippet>,
    /// The vault's hosts by name, to run snippets on.
    hosts: Vec<String>,
    run: Option<Run>,
    search: Entity<InputState>,
    query: SharedString,
    form: Option<Form>,
    _subscription: Subscription,
}

impl SnippetsView {
    pub fn new(
        snippets: Vec<Snippet>,
        hosts: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let subscription = cx.subscribe(&search, |this: &mut Self, search, event, cx| {
            if let InputEvent::Change = event {
                this.query = search.read(cx).value();
                cx.notify();
            }
        });
        Self {
            snippets,
            hosts,
            run: None,
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

    /// Takes the vault's hosts after a change.
    pub fn set_hosts(&mut self, hosts: Vec<String>, cx: &mut Context<Self>) {
        if let Some(run) = &mut self.run {
            run.chosen.retain(|name| hosts.contains(name));
        }
        self.hosts = hosts;
        cx.notify();
    }

    /// Hosts to run `snippet` on, picked from a list; Run opens them side
    /// by side.
    fn run_card(&self, run: &Run, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let count = run.chosen.len();
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
                    .items_center()
                    .gap(px(10.))
                    .child(icon(IconName::Play, 15., t.text))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(run.snippet.name.clone()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(6.))
                    .children(self.hosts.iter().map(|host| {
                        let on = run.chosen.contains(host);
                        let id = format!("run-on-{host}");
                        let name = host.clone();
                        div()
                            .id(SharedString::from(id.clone()))
                            .debug_selector(move || id)
                            .h(px(32.))
                            .px(px(12.))
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .rounded(px(10.))
                            .border_1()
                            .border_color(if on { t.text } else { t.line })
                            .bg(if on { t.text } else { t.card })
                            .text_color(if on { t.card } else { t.text })
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .cursor_pointer()
                            .child(host.clone())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(run) = &mut this.run {
                                    match run.chosen.iter().position(|have| *have == name) {
                                        Some(at) => {
                                            run.chosen.remove(at);
                                        }
                                        None => run.chosen.push(name.clone()),
                                    }
                                }
                                cx.notify();
                            }))
                    })),
            )
            .when(!run.values.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(8.))
                        .children(run.values.iter().map(|(_, field)| {
                            div()
                                .w(px(200.))
                                .h(px(38.))
                                .px(px(10.))
                                .flex()
                                .items_center()
                                .rounded(px(11.))
                                .bg(t.card2)
                                .font_family(MONO)
                                .text_size(px(13.))
                                .child(Input::new(field).appearance(false))
                        })),
                )
            })
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(
                        ink_button("run-snippet", t)
                            .debug_selector(|| "run-snippet".into())
                            .h(px(38.))
                            .px(px(16.))
                            .rounded(px(12.))
                            .text_size(px(13.))
                            .when(count == 0, |d| d.opacity(0.4))
                            .child(icon(IconName::Play, 14., t.card))
                            .child(if count > 1 {
                                format!("Run on {count}")
                            } else {
                                "Run".to_string()
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                let Some(run) = this.run.take() else {
                                    return;
                                };
                                if run.chosen.is_empty() {
                                    this.run = Some(run);
                                    return;
                                }
                                let values: Vec<(String, String)> = run
                                    .values
                                    .iter()
                                    .map(|(name, field)| {
                                        (name.clone(), field.read(cx).value().to_string())
                                    })
                                    .collect();
                                cx.emit(SnippetEvent::Run {
                                    snippet: run.snippet.filled(&values),
                                    hosts: run.chosen,
                                });
                                cx.notify();
                            })),
                    )
                    .child(
                        button("cancel-run", t)
                            .debug_selector(|| "cancel-run".into())
                            .h(px(38.))
                            .px(px(16.))
                            .rounded(px(12.))
                            .text_size(px(13.))
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.run = None;
                                cx.notify();
                            })),
                    ),
            )
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
        let typed = Snippet::new(&form.name.read(cx).value(), &form.command.read(cx).value());
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
        let to_run = snippet.clone();
        let run_id = format!("run-snippet-{index}");
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
            .child(
                div()
                    .id(SharedString::from(run_id.clone()))
                    .debug_selector(move || run_id)
                    .size(px(34.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(10.))
                    .border_1()
                    .border_color(t.line)
                    .hover(|d| d.bg(t.card2))
                    .child(icon(IconName::Play, 14., t.text))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.form = None;
                        let values = to_run
                            .variables()
                            .into_iter()
                            .map(|name| {
                                let placeholder = name.clone();
                                let field = cx
                                    .new(|cx| InputState::new(window, cx).placeholder(placeholder));
                                (name, field)
                            })
                            .collect();
                        this.run = Some(Run {
                            snippet: to_run.clone(),
                            chosen: Vec::new(),
                            values,
                        });
                        cx.notify();
                    })),
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
            .when_some(self.run.as_ref(), |d, run| {
                d.child(self.run_card(run, t, cx))
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
