//! The Files panel beside a session's panes: SFTP on the host, or this
//! computer's folders, with a path bar (Up, New folder, Upload, Refresh), a
//! list of names with sizes or item counts, and transfers with an ink
//! progress bar and a check when done. Clicking a file copies it to the
//! other side's folder; files dropped on the panel upload to the host's
//! folder. Hovering a row offers rename, permissions (on the host) and
//! delete, which a line under the path bar takes or confirms; a host's
//! file can be edited in this computer's app for it, each save going back
//! (asking first if the host's file changed meanwhile). Once its pane has
//! reconnected, Refresh or showing the panel again opens SFTP on the new
//! connection.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::edits::{self, Edit};
use tern_core::sftp::{self, Entry, Progress, Sftp, Stamp};
use tern_core::shells;

use crate::terminal::TerminalView;
use crate::theme::{self, MONO, Theme};
use crate::ui::icon;

/// How often transfers redraw their progress while running.
const PROGRESS_TICK: Duration = Duration::from_millis(100);
/// How long after a transfer ends its side is listed again: transfers that
/// end together are listed once.
const REFRESH_SETTLE: Duration = Duration::from_millis(150);
/// How often edited copies are checked for saves.
const EDIT_POLL: Duration = Duration::from_secs(1);

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
    /// Write a saved copy over the host's file (by path), which changed
    /// since: the copy as it is now.
    Replace(String, Stamp),
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

/// A host's file open in an editor here.
struct Editing {
    edit: Edit,
    /// Checking the host, writing back or asking: the next check waits.
    busy: bool,
    /// The copy as it was when it last couldn't go back: still to go, at
    /// its next save or once SFTP opens again.
    refused: Option<Stamp>,
}

struct Transfer {
    name: String,
    progress: Progress,
    /// `None` while running.
    outcome: Option<Result<(), String>>,
}

pub struct FilesPanel {
    /// The session pane whose host this shows, and which of its sessions
    /// SFTP was opened on.
    pane: WeakEntity<TerminalView>,
    session: u64,
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
    /// The listing of each side asked for last, by number: an older one
    /// that ends later doesn't replace it.
    remote_listing: u64,
    local_listing: u64,
    /// Sides a transfer changed, to list again once transfers settle.
    stale: Vec<Side>,
    refreshing: Option<Task<()>>,
    transfers: Vec<Transfer>,
    form: Option<Form>,
    edits: Vec<Editing>,
    /// The folder the pane's shell moved to before SFTP opened, to start in.
    following: Option<String>,
    /// Files opened in this computer's apps, for tests.
    #[cfg(test)]
    opened: Vec<PathBuf>,
    _opening: Task<()>,
    _ticker: Option<Task<()>>,
    _watch: Option<Task<()>>,
}

impl FilesPanel {
    /// A panel for `pane`'s host, starting in its home folder there and in
    /// `local_dir` here.
    pub fn new(pane: &Entity<TerminalView>, local_dir: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut panel = Self {
            pane: pane.downgrade(),
            session: 0,
            sftp: None,
            host: pane.read(cx).label(),
            problem: None,
            side: Side::Remote,
            remote_dir: "/".into(),
            local_dir,
            remote: Vec::new(),
            local: Vec::new(),
            remote_listing: 0,
            local_listing: 0,
            stale: Vec::new(),
            refreshing: None,
            transfers: Vec::new(),
            form: None,
            edits: Vec::new(),
            following: None,
            #[cfg(test)]
            opened: Vec::new(),
            _opening: Task::ready(()),
            _ticker: None,
            _watch: None,
        };
        panel.open_sftp(pane, None, cx);
        // This computer's side lists in the background too; a problem on
        // the host's side, from opening SFTP, stays.
        panel.list_here(panel.local_dir.clone(), false, cx);
        panel
    }

    /// Goes to the folder the pane's shell moved to; before SFTP opens,
    /// it starts there.
    pub fn follow(&mut self, folder: String, cx: &mut Context<Self>) {
        if self.sftp.is_some() {
            self.go_remote(folder, cx);
        } else {
            self.following = Some(folder);
        }
    }

    /// Moves the pane's shell into the host's folder shown; a guarded
    /// host asks first.
    pub(crate) fn cd_shell(&mut self, cx: &mut Context<Self>) {
        let Some(pane) = self.pane.upgrade() else {
            return;
        };
        let command = format!("cd -- {}\r", shells::quote(&self.remote_dir));
        pane.update(cx, |pane, cx| pane.type_command(command.into_bytes(), cx));
    }

    /// Whether this shows `pane`'s host.
    pub fn is_for(&self, pane: &Entity<TerminalView>) -> bool {
        self.pane == *pane
    }

    /// Opens SFTP on `pane`'s current session, in `dir` there, or else in
    /// the home folder.
    fn open_sftp(
        &mut self,
        pane: &Entity<TerminalView>,
        dir: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let (opening, session) = {
            let pane = pane.read(cx);
            (pane.open_sftp(), pane.session())
        };
        self.session = session;
        self.problem = None;
        self._opening = cx.spawn(async move |this, cx| {
            let opened = match opening.await {
                Ok(sftp) => match dir {
                    Some(dir) => Ok((sftp, dir)),
                    None => sftp.home().await.map(|home| (sftp, home)),
                },
                Err(problem) => Err(problem),
            };
            let _ = this.update(cx, |this, cx| {
                match opened {
                    Ok((sftp, dir)) => {
                        this.sftp = Some(sftp);
                        let dir = this.following.take().unwrap_or(dir);
                        this.go_remote(dir, cx);
                    }
                    Err(problem) => this.problem = Some(problem.into()),
                }
                cx.notify();
            });
        });
    }

    /// Opens SFTP again if the pane has started a session since (it
    /// reconnected), back in the folder shown; says whether it did.
    pub fn reopen_if_reconnected(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(pane) = self.pane.upgrade() else {
            return false;
        };
        if pane.read(cx).session() == self.session {
            return false;
        }
        // The folder is only worth going back to if SFTP had opened.
        let dir = self.sftp.take().map(|_| self.remote_dir.clone());
        self.open_sftp(&pane, dir, cx);
        // Copies that couldn't go back over the old connection try the new.
        for editing in &mut self.edits {
            editing.refused = None;
        }
        cx.notify();
        true
    }

    /// What the host's side lists, by name.
    #[cfg(test)]
    pub fn remote_names(&self) -> Vec<String> {
        self.remote.iter().map(|entry| entry.name.clone()).collect()
    }

    /// Which of its pane's sessions SFTP was opened on.
    #[cfg(test)]
    pub fn session(&self) -> u64 {
        self.session
    }

    /// The folder to start in once SFTP opens, the shell having moved.
    #[cfg(test)]
    pub fn following(&self) -> Option<&str> {
        self.following.as_deref()
    }

    /// Edits the host's file `name` in the folder shown, as its row's
    /// button does.
    #[cfg(test)]
    pub fn edit_named(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some(entry) = self.remote.iter().find(|entry| entry.name == name).cloned() {
            self.edit(&entry, cx);
        }
    }

    /// The files opened in this computer's apps.
    #[cfg(test)]
    pub fn opened(&self) -> &[PathBuf] {
        &self.opened
    }

    /// Whether the line under the path bar asks to write over a host's
    /// file that changed.
    #[cfg(test)]
    pub fn asks_to_replace(&self) -> bool {
        matches!(
            self.form,
            Some(Form {
                change: Change::Replace(..),
                ..
            })
        )
    }

    /// Says yes to what the line under the path bar asks.
    #[cfg(test)]
    pub fn confirm(&mut self, cx: &mut Context<Self>) {
        self.apply(cx);
    }

    /// The folder this computer's side starts in: Downloads, or home.
    pub fn default_local_dir() -> PathBuf {
        let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let downloads = home.join("Downloads");
        if downloads.is_dir() { downloads } else { home }
    }

    /// Lists `dir` on the host: its names as soon as they come, then its
    /// folders' items as they're counted. Only the listing asked for last
    /// shows.
    fn go_remote(&mut self, dir: String, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp.clone() else {
            return;
        };
        self.remote_listing += 1;
        let listing = self.remote_listing;
        let names = sftp.list(&dir);
        cx.spawn(async move |this, cx| {
            let listed = names.await;
            let counting = this.update(cx, |this, cx| {
                if this.remote_listing != listing {
                    return None;
                }
                cx.notify();
                match listed {
                    Ok(entries) => {
                        let counting = sftp.count(&dir, &entries);
                        this.remote_dir = dir;
                        this.remote = entries;
                        this.problem = None;
                        Some(counting)
                    }
                    Err(problem) => {
                        this.problem = Some(problem.into());
                        None
                    }
                }
            });
            if let Ok(Some(counting)) = counting {
                let counts = counting.await;
                let _ = this.update(cx, |this, cx| {
                    if this.remote_listing == listing {
                        sftp::set_counts(&mut this.remote, &counts);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    fn go_local(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        self.list_here(dir, true, cx);
    }

    /// Lists the folder `dir` on this computer, away from the UI thread,
    /// as [`FilesPanel::go_remote`] does on the host; once listed, it
    /// `clears` the problem shown, if asked to.
    fn list_here(&mut self, dir: PathBuf, clears: bool, cx: &mut Context<Self>) {
        self.local_listing += 1;
        let listing = self.local_listing;
        let folder = dir.clone();
        let names = cx.background_spawn(async move { sftp::list_local(&folder) });
        cx.spawn(async move |this, cx| {
            let listed = names.await;
            let counting = this.update(cx, |this, cx| {
                if this.local_listing != listing {
                    return None;
                }
                cx.notify();
                match listed {
                    Ok(entries) => {
                        let (folder, listed) = (dir.clone(), entries.clone());
                        this.local_dir = dir;
                        this.local = entries;
                        if clears {
                            this.problem = None;
                        }
                        Some(
                            cx.background_spawn(async move { sftp::count_local(&folder, &listed) }),
                        )
                    }
                    Err(problem) => {
                        this.problem = Some(problem.into());
                        None
                    }
                }
            });
            if let Ok(Some(counting)) = counting {
                let counts = counting.await;
                let _ = this.update(cx, |this, cx| {
                    if this.local_listing == listing {
                        sftp::set_counts(&mut this.local, &counts);
                        cx.notify();
                    }
                });
            }
        })
        .detach();
    }

    /// Lists `side` again once the transfers ending now have settled:
    /// many ending together are listed once.
    fn refresh_soon(&mut self, side: Side, cx: &mut Context<Self>) {
        if !self.stale.contains(&side) {
            self.stale.push(side);
        }
        if self.refreshing.is_some() {
            return;
        }
        self.refreshing = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_SETTLE).await;
            let _ = this.update(cx, |this, cx| {
                this.refreshing = None;
                for side in std::mem::take(&mut this.stale) {
                    match side {
                        Side::Remote => this.go_remote(this.remote_dir.clone(), cx),
                        Side::Local => this.go_local(this.local_dir.clone(), cx),
                    }
                }
            });
        }));
    }

    /// Opens the line under the path bar for `change`, with the field
    /// focused and filled with what's there now.
    fn begin(&mut self, change: Change, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss();
        let value = match &change {
            Change::Rename(entry) => Some(entry.name.clone()),
            Change::NewFolder => Some(String::new()),
            Change::Mode(entry) => Some(format!("{:o}", entry.mode.unwrap_or(0o644))),
            Change::Delete(_) | Change::Replace(..) => None,
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

    /// Closes the line under the path bar; an edited copy it asked about
    /// waits for its next save.
    fn dismiss(&mut self) {
        if let Some(Form {
            change: Change::Replace(remote, ours),
            ..
        }) = self.form.take()
            && let Some(editing) = self.edits.iter_mut().find(|e| e.edit.remote == remote)
        {
            editing.edit.ours = ours;
            editing.busy = false;
        }
    }

    /// Does what the line under the path bar says; a problem keeps it open.
    fn apply(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.form else {
            return;
        };
        if let Change::Replace(remote, ours) = &form.change {
            let (remote, ours) = (remote.clone(), *ours);
            self.form = None;
            return self.write_back(remote, ours, cx);
        }
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
            Change::Replace(..) => return,
        };
        match side {
            Side::Local => {
                let path = |name: &str| local_dir.join(name);
                let done = match &change {
                    Change::Rename(entry) => sftp::rename_local(&path(&entry.name), &path(&name)),
                    Change::NewFolder => sftp::mkdir_local(&path(&name)),
                    Change::Delete(entry) => sftp::remove_local(&path(&entry.name), entry.dir),
                    Change::Mode(_) | Change::Replace(..) => Ok(()),
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
                        Change::Replace(..) => return,
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
                self.dismiss();
                self.refresh(cx);
            }
            Err(problem) => self.refuse(problem, cx),
        }
        cx.notify();
    }

    /// Lists the side shown again; the host's opens SFTP again first if
    /// the pane reconnected.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        match self.side {
            Side::Remote => {
                if !self.reopen_if_reconnected(cx) {
                    self.go_remote(self.remote_dir.clone(), cx)
                }
            }
            Side::Local => self.go_local(self.local_dir.clone(), cx),
        }
    }

    fn up(&mut self, cx: &mut Context<Self>) {
        self.dismiss();
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
            self.dismiss();
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
        self.track(name, progress, copy, Some(Side::Remote), |_, _, _| {}, cx);
    }

    fn download(&mut self, remote: String, name: String, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp.clone() else {
            return;
        };
        let progress = Progress::default();
        let copy = sftp.download(&remote, self.local_dir.clone(), progress.clone());
        self.track(name, progress, copy, Some(Side::Local), |_, _, _| {}, cx);
    }

    /// Opens the host's file in this computer's app for it, from a copy in
    /// a folder of its own, and watches the copy: each save goes back.
    fn edit(&mut self, entry: &Entry, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp.clone() else {
            return;
        };
        let remote = sftp::join(&self.remote_dir, &entry.name);
        if let Some(editing) = self.edits.iter().find(|e| e.edit.remote == remote) {
            let local = editing.edit.local.clone();
            return self.open_here(&local, cx);
        }
        let folder = match edits::folder(&std::env::temp_dir()) {
            Ok(folder) => folder,
            Err(problem) => {
                self.problem = Some(problem.to_string().into());
                return cx.notify();
            }
        };
        let progress = Progress::default();
        let copying = progress.clone();
        let from = remote.clone();
        // The host's file as it was before the copy, so a change during it
        // counts as one.
        let copy = async move {
            let theirs = sftp.stamp(&from).await?;
            let local = sftp.download(&from, folder, copying).await?;
            Ok((local, theirs))
        };
        let name = entry.name.clone();
        let then = move |this: &mut Self,
                         copied: Result<(PathBuf, Stamp), String>,
                         cx: &mut Context<Self>| {
            let Ok((local, theirs)) = copied else {
                return;
            };
            let local = match edits::openable(local) {
                Ok(local) => local,
                Err(problem) => {
                    this.problem = Some(problem.to_string().into());
                    return;
                }
            };
            match edits::stamp(&local) {
                Ok(ours) => {
                    this.open_here(&local, cx);
                    this.edits.push(Editing {
                        edit: Edit {
                            remote,
                            local,
                            theirs,
                            ours,
                        },
                        busy: false,
                        refused: None,
                    });
                    this.watch(cx);
                }
                Err(problem) => this.problem = Some(problem.to_string().into()),
            }
        };
        // The copy lands in a folder of its own: neither side changes.
        self.track(name, progress, copy, None, then, cx);
    }

    /// Opens this computer's file in the app the system has for it.
    fn open_here(&mut self, path: &Path, cx: &mut Context<Self>) {
        #[cfg(not(test))]
        cx.open_with_system(path);
        #[cfg(test)]
        self.opened.push(path.to_path_buf());
        cx.notify();
    }

    /// Checks the edited copies every second while there are any.
    fn watch(&mut self, cx: &mut Context<Self>) {
        if self._watch.is_some() {
            return;
        }
        self._watch = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(EDIT_POLL).await;
                let going = this.update(cx, |this, cx| {
                    this.check_edits(cx);
                    let going = !this.edits.is_empty();
                    if !going {
                        this._watch = None;
                    }
                    going
                });
                if !matches!(going, Ok(true)) {
                    break;
                }
            }
        }));
    }

    /// Looks for copies saved since they last matched the host's files.
    fn check_edits(&mut self, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp.clone() else {
            return;
        };
        // A copy that's gone isn't being edited any more.
        self.edits.retain(|editing| editing.edit.local.exists());
        for editing in self.edits.iter_mut().filter(|editing| !editing.busy) {
            let Ok(ours) = edits::stamp(&editing.edit.local) else {
                continue;
            };
            if !editing.edit.changed_here(&ours) || editing.refused == Some(ours) {
                continue;
            }
            editing.busy = true;
            let remote = editing.edit.remote.clone();
            let theirs = sftp.stamp(&remote);
            cx.spawn(async move |this, cx| {
                let theirs = theirs.await;
                let _ = this.update(cx, |this, cx| this.saved(remote, ours, theirs, cx));
            })
            .detach();
        }
    }

    /// After a save here: the copy goes back, or, if the host's file
    /// changed too, the line under the path bar asks first.
    fn saved(
        &mut self,
        remote: String,
        ours: Stamp,
        theirs: Result<Stamp, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(editing) = self.edits.iter_mut().find(|e| e.edit.remote == remote) else {
            return;
        };
        match theirs {
            // Another change is being made here: ask once it's done.
            Ok(_) if self.form.is_some() => editing.busy = false,
            Ok(theirs) if editing.edit.changed_there(&theirs) => {
                self.form = Some(Form {
                    change: Change::Replace(remote, ours),
                    side: Side::Remote,
                    local_dir: self.local_dir.clone(),
                    remote_dir: self.remote_dir.clone(),
                    input: None,
                    problem: None,
                    _enter: None,
                });
            }
            Ok(_) => self.write_back(remote, ours, cx),
            Err(problem) => {
                editing.busy = false;
                self.problem = Some(problem.into());
            }
        }
        cx.notify();
    }

    /// Writes the edited copy of `remote`, as it was when `ours` was taken,
    /// over the host's file.
    fn write_back(&mut self, remote: String, ours: Stamp, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp.clone() else {
            return;
        };
        let Some(editing) = self.edits.iter_mut().find(|e| e.edit.remote == remote) else {
            return;
        };
        editing.busy = true;
        let progress = Progress::default();
        let copy = sftp.replace(editing.edit.local.clone(), &remote, progress.clone());
        let name = remote.rsplit('/').next().unwrap_or(&remote).to_string();
        let then =
            move |this: &mut Self, replaced: Result<Stamp, String>, _: &mut Context<Self>| {
                if let Some(editing) = this.edits.iter_mut().find(|e| e.edit.remote == remote) {
                    match replaced {
                        Ok(theirs) => {
                            editing.edit.ours = ours;
                            editing.edit.theirs = theirs;
                            editing.refused = None;
                        }
                        // Still to go back, so not taken for gone.
                        Err(_) => editing.refused = Some(ours),
                    }
                    editing.busy = false;
                }
            };
        self.track(name, progress, copy, Some(Side::Remote), then, cx);
    }

    /// Shows a transfer until it ends, then lists the side it `changed`
    /// again; `then` gets what the copy gave.
    fn track<T: 'static>(
        &mut self,
        name: String,
        progress: Progress,
        copy: impl Future<Output = Result<T, String>> + 'static,
        changed: Option<Side>,
        then: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let index = self.transfers.len();
        self.transfers.push(Transfer {
            name,
            progress,
            outcome: None,
        });
        cx.spawn(async move |this, cx| {
            let copied = copy.await;
            let _ = this.update(cx, |this, cx| {
                this.transfers[index].outcome =
                    Some(copied.as_ref().map(|_| ()).map_err(Clone::clone));
                then(this, copied, cx);
                if let Some(side) = changed {
                    this.refresh_soon(side, cx);
                }
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
            // The shell into this folder; inked while the shell is there.
            .when(self.side == Side::Remote && self.sftp.is_some(), |d| {
                let here = self
                    .pane
                    .upgrade()
                    .is_some_and(|pane| pane.read(cx).folder() == Some(self.remote_dir.as_str()));
                d.child(
                    div()
                        .id("files-cd")
                        .debug_selector(|| "files-cd".into())
                        .size(px(30.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(9.))
                        .cursor_pointer()
                        .when(here, |d| d.bg(t.text))
                        .child(icon(
                            IconName::SquareTerminal,
                            15.,
                            if here { t.card } else { t.text2 },
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.cd_shell(cx))),
                )
            })
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
        // A host's file can be edited here; one being edited shows it.
        let edit = (self.side == Side::Remote && !entry.dir).then(|| {
            let remote = sftp::join(&self.remote_dir, &entry.name);
            let editing = self.edits.iter().any(|e| e.edit.remote == remote);
            let id = format!("file-edit-{index}");
            let edited = entry.clone();
            div()
                .id(SharedString::from(id.clone()))
                .debug_selector(move || id)
                .size(px(24.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(7.))
                .when(!editing, |d| {
                    d.opacity(0.).group_hover(group.clone(), |s| s.opacity(1.))
                })
                .hover(|s| s.bg(t.card))
                .child(icon(
                    IconName::FilePen,
                    13.,
                    if editing { t.text } else { t.text2 },
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.edit(&edited, cx);
                }))
        });
        div()
            .id(SharedString::from(id.clone()))
            .debug_selector(move || id)
            .group(group.clone())
            .w_full()
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
                    .children(edit)
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
            Change::Replace(..) => ("files-confirm", "Replace", t.danger),
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
                            this.dismiss();
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
                        (None, Change::Replace(remote, _)) => d.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(13.))
                                .truncate()
                                .child(format!(
                                    "{} changed on the host",
                                    remote.rsplit('/').next().unwrap_or(remote)
                                )),
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
                                this.dismiss();
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
                            .min_w_0()
                            .px(px(8.))
                            .truncate()
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
                                    this.dismiss();
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
            // Rows are one height, so only those in view are built: a folder
            // of thousands costs what fits on screen.
            .child(
                uniform_list(
                    "files-list",
                    entries.len(),
                    cx.processor(|this, range: Range<usize>, _, cx| {
                        let theme = theme::current(cx);
                        let entries = match this.side {
                            Side::Remote => &this.remote,
                            Side::Local => &this.local,
                        };
                        range
                            .filter_map(|index| {
                                let entry = entries.get(index)?;
                                Some(this.row(index, entry, &theme, cx).into_any_element())
                            })
                            .collect::<Vec<_>>()
                    }),
                )
                .flex_1()
                .min_h_0(),
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
    use gpui::{AppContext as _, Modifiers, TestAppContext, VisualTestContext};

    use super::FilesPanel;
    use crate::terminal::TerminalView;

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing on screen is tagged {selector}"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn shows_the_listing_asked_for_last_and_lists_once_for_many_transfers(cx: &mut TestAppContext) {
        use super::{REFRESH_SETTLE, Side};

        let dir = std::env::temp_dir().join(format!("tern-ui-listings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a").join("one"), "1").unwrap();

        cx.update(gpui_kit::init);
        let start = dir.clone();
        let (panel, cx) = cx.add_window_view(move |_, cx| {
            let pane = cx.new(TerminalView::offline);
            FilesPanel::new(&pane, start, cx)
        });
        cx.run_until_parked();
        // Names, then the folders' items, counted after.
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.local[0].name, "a");
            assert_eq!(panel.local[0].items, Some(1));
        });

        // A listing that ends after a later one was asked for is passed over.
        panel.update(cx, |panel, cx| {
            panel.go_local(dir.join("a"), cx);
            panel.local_listing += 1;
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| assert_eq!(panel.local_dir, dir));

        // Transfers that end together list their side once, after they
        // settle.
        let before = panel.read_with(cx, |panel, _| panel.local_listing);
        panel.update(cx, |panel, cx| {
            for _ in 0..5 {
                panel.refresh_soon(Side::Local, cx);
            }
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| assert_eq!(panel.local_listing, before));
        cx.executor().advance_clock(REFRESH_SETTLE);
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| assert_eq!(panel.local_listing, before + 1));
        let _ = std::fs::remove_dir_all(&dir);
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
            let pane = cx.new(TerminalView::offline);
            FilesPanel::new(&pane, start, cx)
        });
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            assert_eq!(panel.problem.as_deref(), Some("Not connected"));
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
