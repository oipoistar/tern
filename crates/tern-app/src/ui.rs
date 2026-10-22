//! Small building blocks shared by Tern's views.

use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::*;
use tern_core::randomart::Randomart;

use crate::live::{LiveSlots, live};
use crate::marks::{self, Trace};
use crate::theme::Theme;

/// What a screen's builders need: the theme and where live marks register.
pub struct Ui<'a> {
    pub theme: &'a Rc<Theme>,
    pub live: &'a LiveSlots,
}

impl Ui<'_> {
    /// A latency trace in `ink`, animated by the live layer. Size it like
    /// any element.
    pub fn trace(
        &self,
        id: impl Into<SharedString>,
        trace: Trace,
        seed: u32,
        ink: Hsla,
        faint: Hsla,
    ) -> Canvas<()> {
        live(self.live, id, move |bounds, time, window| {
            marks::paint_trace(bounds, &trace, seed, time, ink, faint, window)
        })
    }
}

pub fn icon(name: IconName, size: f32, color: Hsla) -> Icon {
    Icon::new(name).w(px(size)).h(px(size)).text_color(color)
}

/// Key art in a hairline tile; blank until there's a known key to draw.
pub fn art_tile(art: Option<Randomart>, size: f32, radius: f32, theme: &Theme) -> Div {
    let (ink, faint) = (theme.text, theme.text3);
    div()
        .size(px(size))
        .flex_none()
        .rounded(px(radius))
        .border_1()
        .border_color(theme.line)
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    marks::paint_art(bounds, art.as_ref(), ink, faint, window)
                },
            )
            .size_full(),
        )
}

/// A plain button: paper with a hairline edge.
pub fn button(id: impl Into<ElementId>, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(36.))
        .px(px(12.))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.))
        .rounded(px(11.))
        .border_1()
        .border_color(theme.line)
        .bg(theme.card)
        .shadow(theme.shadow_sm())
        .text_size(px(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
}

/// A screen's primary action: solid ink.
pub fn ink_button(id: impl Into<ElementId>, theme: &Theme) -> Stateful<Div> {
    button(id, theme)
        .border_color(theme.text)
        .bg(theme.text)
        .text_color(theme.card)
}

/// A live session's glyph: a solid ink dot.
pub fn status_dot(theme: &Theme) -> Div {
    div().size(px(7.)).rounded_full().bg(theme.text)
}

/// An on/off switch: an ink track with a paper knob when on.
pub fn switch(id: impl Into<ElementId>, on: bool, theme: &Theme) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(44.))
        .h(px(26.))
        .flex_none()
        .relative()
        .rounded(px(13.))
        .bg(if on { theme.text } else { theme.track })
        .cursor_pointer()
        .child(
            div()
                .absolute()
                .top(px(3.))
                .left(px(if on { 21. } else { 3. }))
                .size(px(20.))
                .rounded_full()
                .bg(if on { theme.card } else { white() })
                .shadow(vec![
                    BoxShadow::new(px(0.), px(1.), hsla(0., 0., 0., 0.25)).blur_radius(px(3.)),
                ]),
        )
}

/// The Tern mark: wings and a forked tail, stroked on a 24-unit grid.
pub fn tern_mark(color: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let scale = bounds.size.width.as_f32() / 24.;
            let at = |x: f32, y: f32| {
                point(
                    bounds.origin.x + px(x * scale),
                    bounds.origin.y + px(y * scale),
                )
            };
            let mut mark = PathBuilder::stroke(px(2.2 * scale));
            mark.move_to(at(2., 9.));
            mark.cubic_bezier_to(at(12., 11.), at(6., 6.), at(9., 7.));
            mark.cubic_bezier_to(at(22., 9.), at(15., 7.), at(18., 6.));
            mark.move_to(at(12., 11.));
            mark.line_to(at(9.5, 19.));
            mark.move_to(at(12., 11.));
            mark.line_to(at(14.5, 19.));
            if let Ok(path) = mark.build() {
                window.paint_path(path, color);
            }
        },
    )
    .size(px(22.))
}
