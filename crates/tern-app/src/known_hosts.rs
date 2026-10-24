//! The Known hosts screen: every host key Tern trusts, as key art, hosts,
//! type and fingerprint. Keys in a file Tern trusts keys in can be
//! forgotten: its own, and on Linux and macOS `~/.ssh/known_hosts` too. On
//! Windows that file is only read, and its entries carry a lock.

use std::path::PathBuf;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::known_hosts::{self, KnownHost};

use crate::theme::{self, MONO, Theme};
use crate::ui::{art_tile, button, group_heading, icon, search_field};

pub struct KnownHostsView {
    /// The files to read, each with whether Tern writes it.
    files: Vec<(PathBuf, bool)>,
    entries: Vec<KnownHost>,
    search: Entity<InputState>,
    query: SharedString,
    /// Why the last Forget didn't work.
    notice: Option<SharedString>,
    _subscription: Subscription,
}

impl KnownHostsView {
    pub fn new(files: Vec<(PathBuf, bool)>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let subscription = cx.subscribe(&search, |this: &mut Self, search, event, cx| {
            if let InputEvent::Change = event {
                this.query = search.read(cx).value();
                cx.notify();
            }
        });
        Self {
            entries: known_hosts::list_in(&files),
            files,
            search,
            query: SharedString::default(),
            notice: None,
            _subscription: subscription,
        }
    }

    /// Reads the files again, for keys trusted or forgotten elsewhere.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.entries = known_hosts::list_in(&self.files);
        self.notice = None;
        cx.notify();
    }

    fn forget(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(index) else {
            return;
        };
        match known_hosts::forget(entry) {
            Ok(()) => self.refresh(cx),
            Err(error) => {
                self.notice = Some(format!("Can't forget this key: {error}").into());
                cx.notify();
            }
        }
    }

    fn row(&self, index: usize, entry: &KnownHost, t: &Theme, cx: &Context<Self>) -> Stateful<Div> {
        let id = format!("known-{index}");
        div()
            .id(SharedString::from(id.clone()))
            .debug_selector(move || id)
            .flex()
            .items_center()
            .gap(px(14.))
            .py(px(12.))
            .pl(px(12.))
            .pr(px(14.))
            .rounded(px(18.))
            .bg(t.card)
            .shadow(t.shadow())
            .child(art_tile(Some(entry.art.clone()), 44., 13., t))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(match &entry.hosts {
                        Some(hosts) => div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .font_family(MONO)
                            .truncate()
                            .child(hosts.clone()),
                        // OpenSSH keeps only a hash of the names.
                        None => div()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(t.text3)
                            .child("Hashed"),
                    })
                    .child(
                        div()
                            .flex()
                            .gap(px(10.))
                            .font_family(MONO)
                            .text_size(px(12.))
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(t.text2)
                                    .child(entry.kind.clone()),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .text_color(t.text3)
                                    .truncate()
                                    .child(entry.fingerprint.clone()),
                            ),
                    ),
            )
            .child(if entry.ours {
                let id = format!("forget-{index}");
                button(SharedString::from(id.clone()), t)
                    .debug_selector(move || id)
                    .child(icon(IconName::Trash, 14., t.text))
                    .child("Forget")
                    .on_click(cx.listener(move |this, _, _, cx| this.forget(index, cx)))
                    .into_any_element()
            } else {
                div()
                    .size(px(36.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon(IconName::Lock, 15., t.text3))
                    .into_any_element()
            })
    }
}

impl Render for KnownHostsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let t = &theme;
        // Tern's own keys first: they're the ones that can be changed.
        let groups = [("Tern", true), ("OpenSSH", false)].map(|(name, ours)| {
            let rows: Vec<Stateful<Div>> = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.ours == ours && entry.matches(&self.query))
                .map(|(index, entry)| self.row(index, entry, t, cx))
                .collect();
            (!rows.is_empty()).then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .child(group_heading(name, t))
                    .children(rows)
            })
        });
        div()
            .id("known-hosts")
            .debug_selector(|| "known-hosts".into())
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(22.))
            .child(
                div()
                    .flex()
                    .child(search_field("known-search", &self.search, t)),
            )
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
            .children(groups.into_iter().flatten())
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports: `gpui_kit::*` would shadow Rust's `#[test]`.
    use gpui::{Modifiers, TestAppContext};

    use super::KnownHostsView;

    /// A known_hosts line trusting the same made-up key for `host`.
    fn line(host: &str) -> String {
        const KEY: &str =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJJQcfBOGr0NzINylvUGAqKGNRYlc0YQcFzh42nXCoFQ";
        format!("{host} {KEY}\n")
    }

    #[gpui::test]
    fn forgets_terns_keys_and_leaves_openssh_alone(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join(format!("tern-ui-known-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (system, ours) = (dir.join("system"), dir.join("ours"));
        let system_text = line("github.com");
        std::fs::write(&system, &system_text).unwrap();
        std::fs::write(&ours, line("[127.0.0.1]:2222") + &line("db.lan")).unwrap();
        let files = vec![(system.clone(), false), (ours.clone(), true)];

        cx.update(gpui_kit::init);
        let (view, cx) = cx.add_window_view(|window, cx| KnownHostsView::new(files, window, cx));
        cx.run_until_parked();
        for row in ["known-0", "known-1", "known-2"] {
            assert!(cx.debug_bounds(row).is_some(), "{row} is listed");
        }
        assert!(
            cx.debug_bounds("forget-0").is_none(),
            "OpenSSH's is read only"
        );

        let forget = cx
            .debug_bounds("forget-1")
            .expect("Tern's keys can be forgotten");
        cx.simulate_click(forget.center(), Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let hosts: Vec<Option<&str>> = view
                .entries
                .iter()
                .map(|entry| entry.hosts.as_deref())
                .collect();
            assert_eq!(hosts, [Some("github.com"), Some("db.lan")]);
        });
        assert_eq!(std::fs::read_to_string(&system).unwrap(), system_text);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
