//! The Files panel beside a session's panes: SFTP on the host, or this
//! computer's folders, with a path bar (Up, New folder, Upload, Refresh), a
//! list of names with sizes or item counts, and transfers with an ink
//! progress bar and a check when done. Clicking a file copies it to the
//! other side's folder; files dropped on the panel upload to the host's
//! folder. Hovering a row offers rename, permissions (on the host) and
//! delete, which a line under the path bar takes or confirms.

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::sftp::{self, Entry, Progress, Sftp};

use crate::theme::{self, MONO, Theme};
use crate::ui::icon;

/// How often transfers redraw their progress while running.
const PROGRESS_TICK: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Remote,
    Local,
}

/// What the line under the path bar is doing.
#[derive(Clone)]
enum Change {
    Rename(Entry),
    NewFolder,
    Mode(Entry),
    Delete(Entry),
}

/// The line under the path bar: a name or mode to type, or a delete to
/// confirm.
struct Form {
    change: Change,
    /// Where it happens: the side and its folder when the form opened.
    side: Side,
    local_dir: PathBuf,
    remote_dir: String,
    /// `None` for a delete.
    input: Option<Entity<InputState>>,
    problem: Option<SharedString>,
    _enter: Option<Subscription>,
}

struct Transfer {
    name: String,
    progress: Progress,
    /// `None` while running.
    outcome: Option<Result<(), String>>,
}

pub struct FilesPanel {
    sftp: Option<Sftp>,
    /// The session's host, naming its side.
    host: SharedString,
    /// Why the host's side can't be shown, in one line.
    problem: Option<SharedString>,
    side: Side,
    remote_dir: String,
    local_dir: PathBuf,
    remote: Vec<Entry>,
    local: Vec<Entry>,
    transfers: Vec<Transfer>,
    form: Option<Form>,
    _opening: Task<()>,
    _ticker: Option<Task<()>>,
}

impl FilesPanel {
    /// A panel for the SFTP session on `host` that `opening` will give,
    /// starting in its home folder there and in `local_dir` here.
    pub fn new(
        opening: impl Future<Output = Result<Sftp, String>> + 'static,
        local_dir: PathBuf,
        host: SharedString,
        cx: &mut Context<Self>,
    ) -> Self {
        let task = cx.spawn(async move |this, cx| {
            let opened = match opening.await {
                Ok(sftp) => sftp.home().await.map(|home| (sftp, home)),
                Err(problem) => Err(problem),
            };
            let _ = this.update(cx, |this, cx| {
                match opened {
                    Ok((sftp, home)) => {
                        this.sftp = Some(sftp);
                        this.go_remote(home, cx);
                    }
                    Err(problem) => this.problem = Some(problem.into()),
                }
                cx.notify();
            });
        });
        let local = sftp::list_local(&local_dir).unwrap_or_default();
        Self {
            sftp: None,
            host,
            problem: None,
            side: Side::Remote,
            remote_dir: "/".into(),
            local_dir,
            remote: Vec::new(),
            local,
            transfers: Vec::new(),
            form: None,
            _opening: task,
            _ticker: None,
        }
    }

    /// The folder this computer's side starts in: Downloads, or home.
    pub fn default_local_dir() -> PathBuf {
        let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let downloads = home.join("Downloads");
        if downloads.is_dir() { downloads } else { home }
    }

    fn go_remote(&mut self, dir: String, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp.clone() else {
            return;
        };
        let listing = sftp.list(&dir);
        cx.spawn(async move |this, cx| {
            let listed = listing.await;
            let _ = this.update(cx, |this, cx| {
                match listed {
                    Ok(entries) => {
                        this.remote_dir = dir;
                        this.remote = entries;
                        this.problem = None;
                    }
                    Err(problem) => this.problem = Some(problem.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn go_local(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        match sftp::list_local(&dir) {
            Ok(entries) => {
                self.local_dir = dir;
                self.local = entries;
                self.problem = None;
            }
            Err(problem) => self.problem = Some(problem.into()),
        }
        cx.notify();
    }

    /// Opens the line under the path bar for `change`, with the field
    /// focused and filled with what's there now.
    fn begin(&mut self, change: Change, window: &mut Window, cx: &mut Context<Self>) {
        let value = match &change {
            Change::Rename(entry) => Some(entry.name.clone()),
            Change::NewFolder => Some(String::new()),
            Change::Mode(entry) => Some(format!("{:o}", entry.mode.unwrap_or(0o644))),
            Change::Delete(_) => None,
        };
        let input = value.map(|value| {
            let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
            input.update(cx, |state, cx| state.focus(window, cx));
            input
        });
        let enter = input.as_ref().map(|input| {
            cx.subscribe_in(input, window, |this, _, event: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.apply(cx);
                }
            })
        });
        self.form = Some(Form {
            change,
            side: self.side,
            local_dir: self.local_dir.clone(),
            remote_dir: self.remote_dir.clone(),
            input,
            problem: None,
            _enter: enter,
        });
        cx.notify();
    }

    /// Does what the line under the path bar says; a problem keeps it open.
    fn apply(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        let typed = form
            .input
            .as_ref()
            .map(|input| input.read(cx).value().to_string())
            .unwrap_or_default();
        let change = form.change.clone();
        let (side, local_dir, remote_dir) =
            (form.side, form.local_dir.clone(), form.remote_dir.clone());
        let name = match &change {
            Change::Rename(_) | Change::NewFolder => match sftp::check_name(&typed) {
                Ok(name) => name.to_string(),
                Err(problem) => return self.refuse(problem, cx),
            },
            Change::Mode(entry) | Change::Delete(entry) => entry.name.clone(),
        };
        match side {
            Side::Local => {
                let path = |name: &str| local_dir.join(name);
                let done = match &change {
                    Change::Rename(entry) => sftp::rename_local(&path(&entry.name), &path(&name)),
                    Change::NewFolder => sftp::mkdir_local(&path(&name)),
                    Change::Delete(entry) => sftp::remove_local(&path(&entry.name), entry.dir),
                    Change::Mode(_) => Ok(()),
                };
                self.finish(done, cx);
            }
            Side::Remote => {
                let Some(sftp) = self.sftp.clone() else {
                    return;
                };
                let path = |name: &str| sftp::join(&remote_dir, name);
                let work: std::pin::Pin<Box<dyn Future<Output = Result<(), String>>>> =
                    match &change {
                        Change::Rename(entry) => {
                            Box::pin(sftp.rename(&path(&entry.name), &path(&name)))
                        }
                        Change::NewFolder => Box::pin(sftp.mkdir(&path(&name))),
                        Change::Delete(entry) => {
                            Box::pin(sftp.remove(&path(&entry.name), entry.dir))
                        }
                        Change::Mode(entry) => match sftp::parse_mode(&typed) {
                            Ok(mode) => Box::pin(sftp.set_mode(&path(&entry.name), mode)),
                            Err(problem) => return self.refuse(problem, cx),
                        },
                    };
                cx.spawn(async move |this, cx| {
                    let done = work.await;
                    let _ = this.update(cx, |this, cx| this.finish(done, cx));
                })
                .detach();
            }
        }
    }

    fn refuse(&mut self, problem: String, cx: &mut Context<Self>) {
        if let Some(form) = &mut self.form {
            form.problem = Some(problem.into());
        }
        cx.notify();
    }

    /// After a change: closed and listed again, or the reason it failed.
    fn finish(&mut self, done: Result<(), String>, cx: &mut Context<Self>) {
        match done {
            Ok(()) => {
                self.form = None;
                self.refresh(cx);
            }
            Err(problem) => self.refuse(problem, cx),
        }
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        match self.side {
            Side::Remote => self.go_remote(self.remote_dir.clone(), cx),
            Side::Local => self.go_local(self.local_dir.clone(), cx),
        }
    }

    fn up(&mut self, cx: &mut Context<Self>) {
        self.form = None;
        match self.side {
            Side::Remote => self.go_remote(sftp::parent(&self.remote_dir), cx),
            Side::Local => {
                if let Some(parent) = self.local_dir.parent() {
                    self.go_local(parent.to_path_buf(), cx);
                }
            }
        }
    }

    /// A folder opens; a file copies to the other side's folder.
    fn open(&mut self, entry: &Entry, cx: &mut Context<Self>) {
        if entry.dir {
            self.form = None;
        }
        match (self.side, entry.dir) {
            (Side::Remote, true) => self.go_remote(sftp::join(&self.remote_dir, &entry.name), cx),
            (Side::Local, true) => self.go_local(self.local_dir.join(&entry.name), cx),
            (Side::Remote, false) => {
                let remote = sftp::join(&self.remote_dir, &entry.name);
                self.download(remote, entry.name.clone(), cx);
            }
            (Side::Local, false) => self.upload(self.local_dir.join(&entry.name), cx),
        }
    }

    fn upload(&mut self, local: PathBuf, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp.clone() else {
            return;
        };
        let name = local
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let progress = Progress::default();
        let copy = sftp.upload(local, &self.remote_dir, progress.clone());
        self.track(name, progress, async move { copy.await.map(|_| ()) }, cx);
    }

    fn download(&mut self, remote: String, name: String, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp.clone() else {
            return;
        };
        let progress = Progress::default();
        let copy = sftp.download(&remote, self.local_dir.clone(), progress.clone());
        self.track(name, progress, async move { copy.await.map(|_| ()) }, cx);
    }

    /// Shows a transfer until it ends, then refreshes the side it went to.
    fn track(
        &mut self,
        name: String,
        progress: Progress,
        copy: impl Future<Output = Result<(), String>> + 'static,
        cx: &mut Context<Self>,
    ) {
        let index = self.transfers.len();
        self.transfers.push(Transfer {
            name,
            progress,
            outcome: None,
        });
        cx.spawn(async move |this, cx| {
            let outcome = copy.await;
            let _ = this.update(cx, |this, cx| {
                this.transfers[index].outcome = Some(outcome);
                // Both sides may have changed.
                this.go_remote(this.remote_dir.clone(), cx);
                this.go_local(this.local_dir.clone(), cx);
            });
        })
        .detach();
        if self._ticker.is_none() {
            self._ticker = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(PROGRESS_TICK).await;
                    let running = this.update(cx, |this, cx| {
                        cx.notify();
                        let running = this.transfers.iter().any(|t| t.outcome.is_none());
                        if !running {
                            this._ticker = None;
                        }
                        running
                    });
                    if !matches!(running, Ok(true)) {
                        break;
                    }
                }
            }));
        }
        cx.notify();
    }

    /// Picks files to upload into the host's folder.
    fn choose_uploads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Upload".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = chosen.await {
                let _ = this.update(cx, |this, cx| {
                    for path in paths {
                        this.upload(path, cx);
                    }
                });
            }
        })
        .detach();
    }

    fn path_bar(&self, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let path: SharedString = match self.side {
            Side::Remote => self.remote_dir.clone().into(),
            Side::Local => self.local_dir.display().to_string().into(),
        };
        let small = |id: &'static str, glyph: IconName| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .size(px(30.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(9.))
                .cursor_pointer()
                .child(icon(glyph, 15., t.text2))
        };
        div()
            .h(px(38.))
            .pl(px(4.))
            .pr(px(4.))
            .flex()
            .items_center()
            .gap(px(4.))
            .rounded(px(12.))
            .bg(t.card2)
            .child(
                small("files-up", IconName::ArrowUp)
                    .on_click(cx.listener(|this, _, _, cx| this.up(cx))),
            )
            .child(
                // The end of a path says where you are.
                div()
                    .flex_1()
                    .min_w_0()
                    .font_family(MONO)
                    .text_size(px(12.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_start()
                    .child(path),
            )
            .when(self.side == Side::Local || self.sftp.is_some(), |d| {
                d.child(small("files-new-folder", IconName::FolderPlus).on_click(
                    cx.listener(|this, _, window, cx| this.begin(Change::NewFolder, window, cx)),
                ))
            })
            .when(self.side == Side::Remote && self.sftp.is_some(), |d| {
                d.child(
                    small("files-upload", IconName::Upload).on_click(
                        cx.listener(|this, _, window, cx| this.choose_uploads(window, cx)),
                    ),
                )
            })
            .child(
                small("files-refresh", IconName::RefreshCw)
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
    }

    fn row(&self, index: usize, entry: &Entry, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let id = format!("file-{index}");
        let detail = match (entry.dir, entry.items) {
            (true, Some(1)) => "1 item".to_string(),
            (true, Some(items)) => format!("{items} items"),
            (true, None) => String::new(),
            (false, _) => sftp::size(entry.size),
        };
        let clicked = entry.clone();
        let hover = t.card2;
        let group: SharedString = format!("file-row-{index}").into();
        let action = |name: &'static str, glyph: IconName, change: Change| {
            let id = format!("file-{name}-{index}");
            div()
                .id(SharedString::from(id.clone()))
                .debug_selector(move || id)
                .size(px(24.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(7.))
                .opacity(0.)
                .group_hover(group.clone(), |s| s.opacity(1.))
                .hover(|s| s.bg(t.card))
                .child(icon(glyph, 13., t.text2))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.begin(change.clone(), window, cx);
                }))
        };
        let permissions = (self.side == Side::Remote && entry.mode.is_some())
            .then(|| action("mode", IconName::Lock, Change::Mode(entry.clone())));
        div()
            .id(SharedString::from(id.clone()))
            .debug_selector(move || id)
            .group(group.clone())
            .h(px(34.))
            .px(px(8.))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.))
            .rounded(px(9.))
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .child(icon(
                if entry.dir {
                    IconName::Folder
                } else {
                    IconName::File
                },
                15.,
                if entry.dir { t.text } else { t.text2 },
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(13.))
                    .truncate()
                    .child(entry.name.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .font_family(MONO)
                    .text_size(px(11.5))
                    .text_color(t.text3)
                    .child(detail),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .child(action(
                        "rename",
                        IconName::Pencil,
                        Change::Rename(entry.clone()),
                    ))
                    .children(permissions)
                    .child(action(
                        "delete",
                        IconName::Trash,
                        Change::Delete(entry.clone()),
                    )),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.open(&clicked, cx)))
    }

    /// The line under the path bar: a field with Save, or a delete to
    /// confirm; its problem in a line below.
    fn form(&self, form: &Form, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let button = |id: &'static str, label: &'static str, color: Hsla| {
            div()
                .id(id)
                .debug_selector(move || id.into())
                .h(px(30.))
                .px(px(10.))
                .flex_none()
                .flex()
                .items_center()
                .rounded(px(9.))
                .text_size(px(12.5))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .cursor_pointer()
                .hover(|s| s.bg(t.card))
                .child(label)
        };
        let action = match &form.change {
            Change::Delete(_) => ("files-confirm", "Delete", t.danger),
            _ => ("files-confirm", "Save", t.text),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(
                div()
                    .h(px(38.))
                    .pl(px(10.))
                    .pr(px(4.))
                    .flex()
                    .items_center()
                    .gap(px(4.))
                    .rounded(px(12.))
                    .bg(t.card2)
                    .border_1()
                    .border_color(if form.problem.is_some() {
                        t.danger
                    } else {
                        t.card2
                    })
                    // Escape leaves things as they were.
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "escape" {
                            this.form = None;
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .map(|d| match (&form.input, &form.change) {
                        (Some(input), change) => d.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(13.))
                                .when(matches!(change, Change::Mode(_)), |d| d.font_family(MONO))
                                .child(Input::new(input).appearance(false)),
                        ),
                        (None, Change::Delete(entry)) => d.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(13.))
                                .truncate()
                                .child(format!("Delete {}?", entry.name)),
                        ),
                        (None, _) => d,
                    })
                    .child(
                        button(action.0, action.1, action.2)
                            .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
                    )
                    .child(
                        button("files-cancel", "Cancel", t.text2).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.form = None;
                                cx.notify();
                            },
                        )),
                    ),
            )
            .when_some(form.problem.clone(), |d, problem| {
                d.child(
                    div()
                        .px(px(4.))
                        .text_size(px(12.))
                        .text_color(t.danger)
                        .truncate()
                        .child(problem),
                )
            })
    }

    fn transfer(&self, transfer: &Transfer, t: &Theme) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(5.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(12.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(transfer.name.clone()),
                    )
                    .map(|d| match &transfer.outcome {
                        Some(Ok(())) => d.child(icon(IconName::Check, 13., t.text)),
                        Some(Err(_)) => d,
                        None => d.child(
                            div()
                                .font_family(MONO)
                                .text_color(t.text3)
                                .child(sftp::size(transfer.progress.done())),
                        ),
                    }),
            )
            .map(|d| match &transfer.outcome {
                Some(Err(reason)) => d.child(
                    div()
                        .text_size(px(12.))
                        .text_color(t.danger)
                        .truncate()
                        .child(reason.clone()),
                ),
                _ => d.child(
                    div().h(px(4.)).rounded_full().bg(t.track).child(
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(t.text)
                            .w(relative(transfer.progress.fraction())),
                    ),
                ),
            })
    }
}

impl Render for FilesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let t = &theme;
        let sides: [(Side, SharedString, &str); 2] = [
            (Side::Remote, self.host.clone(), "files-remote"),
            (Side::Local, "This computer".into(), "files-local"),
        ];
        let entries = match self.side {
            Side::Remote => &self.remote,
            Side::Local => &self.local,
        };
        div()
            .id("files")
            .debug_selector(|| "files".into())
            .w(px(340.))
            .flex_none()
            .h_full()
            .p(px(12.))
            .flex()
            .flex_col()
            .gap(px(10.))
            .rounded(px(20.))
            .bg(t.card)
            .shadow(t.shadow())
            .border_1()
            .border_color(t.card)
            // Files dropped here go up to the host's folder.
            .drag_over::<ExternalPaths>(|style, _, _, cx| {
                style.border_color(theme::current(cx).text)
            })
            .on_drop(cx.listener(|this, dropped: &ExternalPaths, _, cx| {
                for path in dropped.paths() {
                    if path.is_file() {
                        this.upload(path.clone(), cx);
                    }
                }
            }))
            .child(
                div()
                    .flex()
                    .gap(px(4.))
                    .p(px(4.))
                    .rounded(px(13.))
                    .bg(t.card2)
                    .children(sides.into_iter().map(|(side, label, id)| {
                        let on = self.side == side;
                        div()
                            .id(id)
                            .debug_selector(move || id.into())
                            .flex_1()
                            .h(px(32.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(10.))
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
                                if this.side != side {
                                    this.side = side;
                                    this.form = None;
                                }
                                cx.notify();
                            }))
                    })),
            )
            .child(self.path_bar(t, cx))
            .when_some(self.form.as_ref(), |d, form| {
                d.child(self.form(form, t, cx))
            })
            .when_some(self.problem.clone(), |d, problem| {
                d.child(
                    div()
                        .px(px(4.))
                        .text_size(px(12.))
                        .text_color(t.danger)
                        .child(problem),
                )
            })
            // Still opening SFTP on the host.
            .when(
                self.side == Side::Remote && self.sftp.is_none() && self.problem.is_none(),
                |d| {
                    d.child(div().flex().justify_center().py(px(12.)).child(icon(
                        IconName::LoaderCircle,
                        16.,
                        t.text2,
                    )))
                },
            )
            .child(
                div()
                    .id("files-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(
                        entries
                            .iter()
                            .enumerate()
                            .map(|(index, entry)| self.row(index, entry, t, cx)),
                    ),
            )
            .when(!self.transfers.is_empty(), |d| {
                d.child(div().h(px(1.)).bg(t.line)).child(
                    div()
                        .max_h(px(180.))
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .children(
                            self.transfers
                                .iter()
                                .rev()
                                .take(5)
                                .map(|transfer| self.transfer(transfer, t)),
                        ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports: `gpui_kit::*` would shadow Rust's `#[test]`.
    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    use super::FilesPanel;

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing on screen is tagged {selector}"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn browses_this_computer_and_says_why_the_host_cant(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("tern-ui-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("photos")).unwrap();
        std::fs::write(dir.join("photos").join("cat.jpg"), "meow").unwrap();
        std::fs::write(dir.join("notes.txt"), "hi").unwrap();

        cx.update(gpui_kit::init);
        let start = dir.clone();
        let (panel, cx) = cx.add_window_view(move |_, cx| {
            FilesPanel::new(
                async { Err("the host doesn't offer SFTP".to_string()) },
                start,
                "web-01".into(),
                cx,
            )
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(
                panel.problem.as_deref(),
                Some("the host doesn't offer SFTP")
            );
        });

        // This computer's side: folders first; a folder opens, Up returns.
        click(cx, "files-local");
        click(cx, "file-0");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.local_dir, dir.join("photos"));
            assert_eq!(panel.local[0].name, "cat.jpg");
        });
        click(cx, "files-up");
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.local_dir, dir);
            assert_eq!(panel.local[0].items, Some(1));
        });

        // A new folder, named in the line under the path bar.
        click(cx, "files-new-folder");
        cx.simulate_input("box");
        cx.simulate_keystrokes("enter");
        assert!(dir.join("box").is_dir());
        let names = |cx: &mut VisualTestContext| {
            panel.read_with(cx, |panel, _| {
                panel
                    .local
                    .iter()
                    .map(|entry| entry.name.clone())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(names(cx), ["box", "photos", "notes.txt"]);

        // Rename: a name with a slash is refused in one line, then fixed.
        click(cx, "file-rename-2");
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        });
        cx.simulate_input("a/b");
        click(cx, "files-confirm");
        panel.read_with(cx, |panel, _| {
            assert!(panel.form.as_ref().unwrap().problem.is_some())
        });
        cx.simulate_keystrokes(if cfg!(target_os = "macos") {
            "cmd-a"
        } else {
            "ctrl-a"
        });
        cx.simulate_input("memo.txt");
        cx.simulate_keystrokes("enter");
        assert_eq!(names(cx), ["box", "photos", "memo.txt"]);

        // A change begun here doesn't follow into another folder or side.
        click(cx, "file-delete-1");
        click(cx, "file-0");
        assert!(panel.read_with(cx, |panel, _| panel.form.is_none()));
        click(cx, "files-up");
        click(cx, "file-delete-1");
        click(cx, "files-remote");
        assert!(panel.read_with(cx, |panel, _| panel.form.is_none()));
        click(cx, "files-local");

        // Delete asks first; Cancel leaves it.
        click(cx, "file-delete-1");
        click(cx, "files-cancel");
        assert!(dir.join("photos").exists());
        click(cx, "file-delete-1");
        click(cx, "files-confirm");
        assert!(!dir.join("photos").exists());
        assert_eq!(names(cx), ["box", "memo.txt"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
