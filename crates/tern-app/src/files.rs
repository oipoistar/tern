//! The Files panel beside a session's panes: SFTP on the host, or this
//! computer's folders, with a path bar (Up, Upload, Refresh), a list of
//! names with sizes or item counts, and transfers with an ink progress bar
//! and a check when done. Clicking a file copies it to the other side's
//! folder; files dropped on the panel upload to the host's folder.

use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::assets::IconName;
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

struct Transfer {
    name: String,
    progress: Progress,
    /// `None` while running.
    outcome: Option<Result<(), String>>,
}

pub struct FilesPanel {
    sftp: Option<Sftp>,
    /// Why the host's side can't be shown, in one line.
    problem: Option<SharedString>,
    side: Side,
    remote_dir: String,
    local_dir: PathBuf,
    remote: Vec<Entry>,
    local: Vec<Entry>,
    transfers: Vec<Transfer>,
    _opening: Task<()>,
    _ticker: Option<Task<()>>,
}

impl FilesPanel {
    /// A panel for the SFTP session `opening` will give, starting in its
    /// home folder there and in `local_dir` here.
    pub fn new(
        opening: impl Future<Output = Result<Sftp, String>> + 'static,
        local_dir: PathBuf,
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
            problem: None,
            side: Side::Remote,
            remote_dir: "/".into(),
            local_dir,
            remote: Vec::new(),
            local,
            transfers: Vec::new(),
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

    fn refresh(&mut self, cx: &mut Context<Self>) {
        match self.side {
            Side::Remote => self.go_remote(self.remote_dir.clone(), cx),
            Side::Local => self.go_local(self.local_dir.clone(), cx),
        }
    }

    fn up(&mut self, cx: &mut Context<Self>) {
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
        div()
            .id(SharedString::from(id.clone()))
            .debug_selector(move || id)
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
            .on_click(cx.listener(move |this, _, _, cx| this.open(&clicked, cx)))
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
        let sides = [
            (Side::Remote, "Remote", "files-remote"),
            (Side::Local, "This computer", "files-local"),
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
                                this.side = side;
                                cx.notify();
                            }))
                    })),
            )
            .child(self.path_bar(t, cx))
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
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
