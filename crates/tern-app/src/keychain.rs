//! The Keychain screen: SSH key pairs as cards (art, name, type), the
//! selected one inverted with its details beside the grid. Tern generates
//! and imports keys into `~/.ssh` on Linux and macOS and into its own folder
//! on Windows ([`keys::home`]); only the keys it added can be deleted.

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::Vault;
use tern_core::keys::{self, KeyPair};

use crate::theme::{self, MONO, TermPalette, Theme};
use crate::ui::{art_tile, button, group_heading, icon, ink_button, search_field, switch};
use crate::vaults;

/// Wide enough for a key's file name; three fit beside the details.
const CARD_WIDTH: f32 = 250.;

pub enum KeychainEvent {
    /// Add `public` to `authorized_keys` on the saved host called `host`.
    Install { host: String, public: String },
}

impl EventEmitter<KeychainEvent> for KeychainView {}

/// The Generate key form.
struct Generate {
    name: Entity<InputState>,
    passphrase: Entity<InputState>,
    problem: Option<SharedString>,
}

pub struct KeychainView {
    /// Where keys are read, each with whether it is Tern's own folder.
    dirs: Vec<(PathBuf, bool)>,
    /// A snapshot of the vault: which hosts use a key, where to install.
    vault: Vault,
    keys: Vec<KeyPair>,
    selected: Option<PathBuf>,
    search: Entity<InputState>,
    query: SharedString,
    /// One line about the last action that failed.
    notice: Option<SharedString>,
    generate: Option<Generate>,
    /// The host list for Install is open.
    picking_host: bool,
    /// Delete was clicked once; the next click deletes.
    confirm_delete: bool,
    _subscription: Subscription,
}

impl KeychainView {
    pub fn new(
        dirs: Vec<(PathBuf, bool)>,
        vault: Vault,
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
        let keys = keys::list_in(&dirs);
        Self {
            selected: keys.first().map(|key| key.path.clone()),
            keys,
            dirs,
            vault,
            search,
            query: SharedString::default(),
            notice: None,
            generate: None,
            picking_host: false,
            confirm_delete: false,
            _subscription: subscription,
        }
    }

    /// Reads the key folders again and takes a fresh look at the vault.
    pub fn refresh(&mut self, vault: Vault, cx: &mut Context<Self>) {
        self.vault = vault;
        self.reload(None);
        cx.notify();
    }

    /// Re-lists the keys, selecting `select` if given, else keeping the
    /// selection while it exists.
    fn reload(&mut self, select: Option<PathBuf>) {
        self.keys = keys::list_in(&self.dirs);
        let keep = select.or_else(|| self.selected.take());
        self.selected = keep
            .filter(|path| self.keys.iter().any(|key| key.path == *path))
            .or_else(|| self.keys.first().map(|key| key.path.clone()));
        self.confirm_delete = false;
        self.picking_host = false;
    }

    fn selected(&self) -> Option<&KeyPair> {
        let path = self.selected.as_ref()?;
        self.keys.iter().find(|key| key.path == *path)
    }

    fn select(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.selected = Some(path);
        self.confirm_delete = false;
        self.picking_host = false;
        cx.notify();
    }

    fn open_generate(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = keys::home(&self.dirs) else {
            return;
        };
        let suggested = keys::free_name(&dir, "id_ed25519");
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Name")
                .default_value(suggested)
        });
        let passphrase = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Passphrase")
                .masked(true)
        });
        name.update(cx, |state, cx| state.focus(window, cx));
        self.generate = Some(Generate {
            name,
            passphrase,
            problem: None,
        });
        cx.notify();
    }

    fn create_key(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &mut self.generate else {
            return;
        };
        let name = form.name.read(cx).value().trim().to_string();
        let passphrase = form.passphrase.read(cx).value().to_string();
        match keys::generate(&self.dirs, &name, Some(&passphrase), &comment()) {
            Ok(pair) => {
                self.generate = None;
                self.notice = None;
                self.reload(Some(pair.path));
            }
            Err(problem) => form.problem = Some(problem.into()),
        }
        cx.notify();
    }

    fn import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirs = self.dirs.clone();
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = chosen.await
                && let Some(path) = paths.into_iter().next()
            {
                let _ = this.update(cx, |this, cx| {
                    match keys::import(&dirs, &path) {
                        Ok(pair) => {
                            this.notice = None;
                            this.reload(Some(pair.path));
                        }
                        Err(error) => this.notice = Some(error.into()),
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pair) = self.selected().cloned() else {
            return;
        };
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Export here".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = chosen.await
                && let Some(folder) = paths.into_iter().next()
            {
                let _ = this.update(cx, |this, cx| {
                    this.notice = keys::export(&pair, &folder).err().map(Into::into);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// The first click arms it, the second deletes.
    fn delete(&mut self, cx: &mut Context<Self>) {
        let Some(pair) = self.selected().cloned() else {
            return;
        };
        if !self.confirm_delete {
            self.confirm_delete = true;
        } else {
            match keys::delete(&self.dirs, &pair) {
                Ok(()) => {
                    self.notice = None;
                    self.selected = None;
                    self.reload(None);
                }
                Err(error) => self.notice = Some(error.into()),
            }
        }
        cx.notify();
    }

    fn copy_public(&self, cx: &mut Context<Self>) {
        if let Some(pair) = self.selected() {
            cx.write_to_clipboard(ClipboardItem::new_string(pair.public.clone()));
        }
    }

    fn install_on(&mut self, host: String, cx: &mut Context<Self>) {
        self.picking_host = false;
        if let Some(pair) = self.selected() {
            cx.emit(KeychainEvent::Install {
                host,
                public: pair.public.clone(),
            });
        }
        cx.notify();
    }

    fn toolbar(&self, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap(px(12.))
            .child(search_field("keychain-search", &self.search, t))
            .child(
                button("import-key", t)
                    .debug_selector(|| "import-key".into())
                    .h(px(46.))
                    .px(px(16.))
                    .rounded(px(16.))
                    .border_dashed()
                    .border_color(t.text3.opacity(0.6))
                    .shadow(Vec::new())
                    .bg(transparent_black())
                    .text_size(px(14.))
                    .child(icon(IconName::Import, 16., t.text))
                    .child("Import")
                    .on_click(cx.listener(|this, _, window, cx| this.import(window, cx))),
            )
            .child(
                ink_button("generate-key", t)
                    .debug_selector(|| "generate-key".into())
                    .h(px(46.))
                    .px(px(20.))
                    .rounded(px(16.))
                    .text_size(px(15.))
                    .child(icon(IconName::Plus, 16., t.card))
                    .child("Generate key")
                    .on_click(cx.listener(|this, _, window, cx| this.open_generate(window, cx))),
            )
    }

    fn generate_form(&self, form: &Generate, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let field = |input: &Entity<InputState>, bad: bool| {
            div()
                .flex_1()
                .min_w_0()
                .h(px(44.))
                .px(px(10.))
                .flex()
                .items_center()
                .rounded(px(12.))
                .bg(t.card2)
                .border_1()
                .border_color(if bad { t.danger } else { t.card2 })
                .font_family(MONO)
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
                    .child(
                        div()
                            .size(px(44.))
                            .flex_none()
                            .rounded(px(13.))
                            .border_1()
                            .border_color(t.line)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon(IconName::KeyRound, 18., t.text)),
                    )
                    .child(field(&form.name, form.problem.is_some()))
                    .child(field(&form.passphrase, false))
                    .child(
                        button("cancel-generate", t)
                            .h(px(44.))
                            .px(px(16.))
                            .rounded(px(14.))
                            .child("Cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.generate = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        ink_button("create-key", t)
                            .debug_selector(|| "create-key".into())
                            .h(px(44.))
                            .px(px(18.))
                            .rounded(px(14.))
                            .child("Generate")
                            .on_click(cx.listener(|this, _, _, cx| this.create_key(cx))),
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

    fn card(&self, pair: &KeyPair, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let selected = self.selected.as_ref() == Some(&pair.path);
        let t = if selected { t.inverse() } else { t.clone() };
        let id = format!("key-{}", pair.name);
        let path = pair.path.clone();
        div()
            .id(SharedString::from(id.clone()))
            .debug_selector(move || id)
            .w(px(CARD_WIDTH))
            .p(px(14.))
            .flex()
            .items_center()
            .gap(px(14.))
            .rounded(px(22.))
            .bg(t.card)
            .text_color(t.text)
            .shadow(t.shadow())
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| this.select(path.clone(), cx)))
            .child(art_tile(Some(pair.art.clone()), 52., 15., &t))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(pair.name.clone()),
                    )
                    .child(
                        div()
                            .font_family(MONO)
                            .text_size(px(12.))
                            .text_color(t.text2)
                            .child(pair.kind.clone()),
                    )
                    .when(pair.weak, |d| {
                        d.child(
                            div()
                                .text_size(px(12.))
                                .text_color(t.danger)
                                .child("Weak, replace"),
                        )
                    }),
            )
            .when(pair.hardware, |d| {
                d.child(icon(IconName::Usb, 16., t.text2).flex_none())
            })
    }

    fn details(&self, pair: &KeyPair, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        // Tern's own, not the terminals' colour scheme: a key isn't
        // terminal output.
        let palette = TermPalette::for_theme(t);
        let users: Vec<String> = pair
            .users(&self.vault)
            .iter()
            .map(|host| host.name.clone())
            .collect();
        div()
            .id("key-details")
            .debug_selector(|| "key-details".into())
            .w(px(340.))
            .flex_none()
            .self_start()
            .p(px(18.))
            .flex()
            .flex_col()
            .gap(px(16.))
            .rounded(px(22.))
            .bg(t.card)
            .shadow(t.shadow())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .child(art_tile(Some(pair.art.clone()), 88., 22., t))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(3.))
                            .child(
                                div()
                                    .text_size(px(17.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(pair.name.clone()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.))
                                    .font_family(MONO)
                                    .text_size(px(12.))
                                    .text_color(t.text2)
                                    .child(pair.kind.clone())
                                    .when(pair.hardware, |d| {
                                        d.child(icon(IconName::Usb, 13., t.text2))
                                    }),
                            )
                            .when(pair.weak, |d| {
                                d.child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(t.danger)
                                        .child("Weak, replace"),
                                )
                            }),
                    ),
            )
            // The public key, in terminal ink: it's what gets pasted into a
            // server.
            .child(
                div()
                    .relative()
                    .py(px(12.))
                    .pl(px(12.))
                    .pr(px(40.))
                    .rounded(px(14.))
                    .bg(palette.background)
                    .text_color(palette.foreground)
                    .font_family(MONO)
                    .text_size(px(11.5))
                    .line_height(px(17.))
                    .child(wrap_anywhere(&pair.public, 34))
                    .child(
                        div()
                            .id("copy-public")
                            .debug_selector(|| "copy-public".into())
                            .absolute()
                            .top(px(6.))
                            .right(px(6.))
                            .size(px(28.))
                            .rounded(px(8.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .child(icon(IconName::Copy, 14., palette.foreground.opacity(0.75)))
                            .on_click(cx.listener(|this, _, _, cx| this.copy_public(cx))),
                    ),
            )
            .child(
                div()
                    .font_family(MONO)
                    .text_size(px(12.))
                    .text_color(t.text2)
                    .child(wrap_anywhere(&pair.fingerprint, 44)),
            )
            .when(!users.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .items_start()
                        .gap(px(8.))
                        .child(icon(IconName::Server, 14., t.text3).mt(px(1.)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .font_family(MONO)
                                .text_size(px(12.))
                                .child(users.join(" / ")),
                        ),
                )
            })
            // Only while sync is set up; private keys stay off it unless
            // chosen one by one.
            .when(vaults::sync_settings(cx).folder.is_some(), |d| {
                let fingerprint = pair.fingerprint.clone();
                let on = vaults::sync_settings(cx).keys.contains(&fingerprint);
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.))
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(13.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child("Include in encrypted sync"),
                        )
                        .child(
                            switch("key-sync", on, t)
                                .debug_selector(|| "key-sync".into())
                                .on_click(move |_, _, cx| {
                                    vaults::update_sync(cx, |sync| {
                                        let keys = &mut sync.keys;
                                        match keys.iter().position(|have| *have == fingerprint) {
                                            Some(index) => {
                                                keys.remove(index);
                                            }
                                            None => keys.push(fingerprint.clone()),
                                        }
                                    })
                                }),
                        ),
                )
            })
            .child(self.actions(pair, t, cx))
    }

    fn actions(&self, pair: &KeyPair, t: &Theme, cx: &Context<Self>) -> impl IntoElement {
        let hosts: Vec<(String, String)> = self
            .vault
            .hosts()
            .map(|host| (host.name.clone(), host.target()))
            .collect();
        div()
            .flex()
            .flex_wrap()
            .gap(px(8.))
            .child(
                div()
                    .child(
                        button("install-key", t)
                            .debug_selector(|| "install-key".into())
                            .child(icon(IconName::Upload, 14., t.text))
                            .child("Install on a host")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.picking_host = true;
                                cx.notify();
                            })),
                    )
                    .when(self.picking_host && !hosts.is_empty(), |d| {
                        let hover = t.card2;
                        d.child(deferred(
                            anchored().snap_to_window_with_margin(px(8.)).child(
                                div()
                                    .id("install-menu")
                                    .occlude()
                                    .mt(px(6.))
                                    .min_w(px(260.))
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
                                    .text_color(t.text)
                                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                        this.picking_host = false;
                                        cx.notify();
                                    }))
                                    .children(hosts.into_iter().map(|(name, target)| {
                                        let id = format!("install-on-{name}");
                                        div()
                                            .id(SharedString::from(id.clone()))
                                            .debug_selector(move || id)
                                            .h(px(36.))
                                            .px(px(10.))
                                            .flex()
                                            .items_center()
                                            .gap(px(10.))
                                            .rounded(px(10.))
                                            .cursor_pointer()
                                            .hover(move |s| s.bg(hover))
                                            .child(div().flex_1().truncate().child(name.clone()))
                                            .child(
                                                div()
                                                    .font_family(MONO)
                                                    .text_size(px(12.))
                                                    .text_color(t.text3)
                                                    .child(target),
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.install_on(name.clone(), cx)
                                            }))
                                    })),
                            ),
                        ))
                    }),
            )
            .child(
                button("export-key", t)
                    .debug_selector(|| "export-key".into())
                    .child(icon(IconName::Download, 14., t.text))
                    .child("Export")
                    .on_click(cx.listener(|this, _, window, cx| this.export(window, cx))),
            )
            .when(pair.ours, |d| {
                d.child(
                    div()
                        .id("delete-key")
                        .debug_selector(|| "delete-key".into())
                        .h(px(36.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.danger)
                        .cursor_pointer()
                        .child(if self.confirm_delete {
                            "Delete for good"
                        } else {
                            "Delete key"
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.delete(cx))),
                )
            })
    }
}

impl Render for KeychainView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let t = &theme;
        let groups = [("Tern", true), ("OpenSSH", false)].map(|(name, ours)| {
            let cards: Vec<_> = self
                .keys
                .iter()
                .filter(|pair| pair.ours == ours && pair.matches(&self.query))
                .map(|pair| self.card(pair, t, cx))
                .collect();
            (!cards.is_empty()).then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(14.))
                    .child(group_heading(name, t))
                    .child(div().flex().flex_wrap().gap(px(14.)).children(cards))
            })
        });
        div()
            .id("keychain")
            .debug_selector(|| "keychain".into())
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .gap(px(20.))
            .child(
                div()
                    .id("keychain-keys")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(22.))
                    .child(self.toolbar(t, cx))
                    .when_some(self.notice.clone(), |d, notice| {
                        d.child(
                            div()
                                .px(px(4.))
                                .text_size(px(13.))
                                .text_color(t.danger)
                                .truncate()
                                .child(notice),
                        )
                    })
                    .when_some(self.generate.as_ref(), |d, form| {
                        d.child(self.generate_form(form, t, cx))
                    })
                    .children(groups.into_iter().flatten()),
            )
            .when_some(self.selected(), |d, pair| {
                d.child(self.details(pair, t, cx))
            })
    }
}

/// `text` in lines of at most `width` characters, for keys and
/// fingerprints that have no spaces to wrap at.
fn wrap_anywhere(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width.max(1))
        .map(|line| line.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The comment a new key carries, as `ssh-keygen` writes it: `user@computer`.
fn comment() -> String {
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    let computer = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default();
    match (user.is_empty(), computer.is_empty()) {
        (false, false) => format!("{user}@{computer}"),
        (false, true) => user,
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports: `gpui_kit::*` would shadow Rust's `#[test]`.
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Modifiers, TestAppContext, VisualTestContext};
    use tern_core::{Auth, Host, Vault};

    use super::{KeychainEvent, KeychainView};

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("nothing on screen is tagged {selector}"));
        cx.simulate_click(bounds.center(), Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn generates_copies_installs_and_deletes_keys(cx: &mut TestAppContext) {
        let root = std::env::temp_dir().join(format!("tern-ui-keychain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (ssh, ours) = (root.join("ssh"), root.join("ours"));
        std::fs::create_dir_all(&ssh).unwrap();
        let existing =
            tern_core::keys::generate(&[(ssh.clone(), true)], "id_ed25519", None, "").unwrap();
        let mut vault = Vault::default();
        let db = Host {
            auth: Auth::Key(existing.path.clone()),
            ..Host::new("db", "deploy", "10.0.0.5", 22)
        };
        vault.save(None, db, "Hosts");
        let dirs = vec![(ssh.clone(), false), (ours.clone(), true)];

        cx.update(gpui_kit::init);
        let (view, cx) =
            cx.add_window_view(|window, cx| KeychainView::new(dirs, vault, window, cx));
        let installs: Rc<RefCell<Vec<(String, String)>>> = Rc::default();
        let recorded = installs.clone();
        cx.update(|_, cx| {
            cx.subscribe(&view, move |_, event: &KeychainEvent, _| {
                let KeychainEvent::Install { host, public } = event;
                recorded.borrow_mut().push((host.clone(), public.clone()));
            })
            .detach()
        });

        // OpenSSH's key is listed and selected, but can't be deleted.
        cx.run_until_parked();
        assert!(cx.debug_bounds("key-id_ed25519").is_some());
        assert!(cx.debug_bounds("key-details").is_some());
        assert!(cx.debug_bounds("delete-key").is_none());

        // Generate a key under the suggested name: in ~/.ssh beside the
        // other on Linux and macOS, in Tern's own folder on Windows.
        click(cx, "generate-key");
        click(cx, "create-key");
        let made = view.read_with(cx, |view, _| view.selected().cloned().unwrap());
        let (home, name) = if cfg!(unix) {
            (&ssh, "id_ed25519-2")
        } else {
            (&ours, "id_ed25519")
        };
        assert!(made.ours && made.path.starts_with(home));
        assert_eq!(made.name, name);

        click(cx, "copy-public");
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some(made.public.as_str()));

        click(cx, "install-key");
        click(cx, "install-on-db");
        assert_eq!(
            *installs.borrow(),
            [("db".to_string(), made.public.clone())]
        );

        // Deleting takes a second click.
        click(cx, "delete-key");
        assert!(made.path.exists());
        click(cx, "delete-key");
        assert!(!made.path.exists());
        view.read_with(cx, |view, _| {
            assert_eq!(view.selected().map(|pair| pair.ours), Some(false));
        });
        std::fs::remove_dir_all(&root).unwrap();
    }
}
