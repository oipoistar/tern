//! The History screen: recent connections, newest first, each with when it
//! was, how long it lasted, how it ended, and a button to go back.

use gpui_kit::assets::IconName;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use tern_core::history::{self, Visit};

use crate::theme::{self, MONO, Theme};
use crate::ui::icon;

pub enum HistoryEvent {
    /// Connect again as the visit at this index did.
    Reconnect(usize),
}

impl EventEmitter<HistoryEvent> for HistoryView {}

pub struct HistoryView {
    /// Newest first, as last recorded.
    visits: Vec<Visit>,
}

impl HistoryView {
    pub fn new(visits: Vec<Visit>) -> Self {
        Self { visits }
    }

    pub fn refresh(&mut self, visits: Vec<Visit>, cx: &mut Context<Self>) {
        self.visits = visits;
        cx.notify();
    }

    fn row(
        &self,
        index: usize,
        visit: &Visit,
        now: u64,
        t: &Theme,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let when = match visit.lasted {
            Some(seconds) => format!(
                "{} · {}",
                history::ago(visit.started, now),
                history::span(seconds)
            ),
            None => history::ago(visit.started, now),
        };
        let id = format!("reconnect-{index}");
        div()
            .flex()
            .items_center()
            .gap(px(14.))
            .py(px(12.))
            .pl(px(14.))
            .pr(px(12.))
            .rounded(px(18.))
            .bg(t.card)
            .shadow(t.shadow())
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
                    .child(icon(IconName::SquareTerminal, 16., t.text2)),
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
                            .child(visit.label()),
                    )
                    .when(visit.host.is_some(), |d| {
                        d.child(
                            div()
                                .font_family(MONO)
                                .text_size(px(12.))
                                .text_color(t.text2)
                                .truncate()
                                .child(visit.target()),
                        )
                    }),
            )
            .child(
                div()
                    .w(px(280.))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .text_size(px(13.))
                    .child(div().text_color(t.text2).child(when))
                    .child(
                        div()
                            .text_color(t.text3)
                            .truncate()
                            .child(visit.ended.clone()),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(id.clone()))
                    .debug_selector(move || id)
                    .size(px(36.))
                    .flex_none()
                    .rounded_full()
                    .border_1()
                    .border_color(t.line)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .child(icon(IconName::RotateCw, 15., t.text))
                    .on_click(
                        cx.listener(move |_, _, _, cx| cx.emit(HistoryEvent::Reconnect(index))),
                    ),
            )
    }
}

impl Render for HistoryView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let now = history::now();
        div()
            .id("history")
            .debug_selector(|| "history".into())
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(10.))
            .children(
                self.visits
                    .iter()
                    .enumerate()
                    .map(|(index, visit)| self.row(index, visit, now, &theme, cx)),
            )
    }
}
