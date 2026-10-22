//! Small building blocks shared by Tern's views.

use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::*;

use crate::glass::{Glass, GlassSlots, glass};
use crate::theme::{Milk, Theme, Tone};

/// What every builder needs: the theme and where glass surfaces register.
pub struct Ui<'a> {
    pub theme: &'a Rc<Theme>,
    pub slots: &'a GlassSlots,
}

impl Ui<'_> {
    #[allow(clippy::too_many_arguments)]
    pub fn glass<E: IntoElement>(
        &self,
        id: impl Into<SharedString>,
        tone: Tone,
        milk: Milk,
        radius: f32,
        underlay: Hsla,
        seed: f32,
        content: impl Fn() -> E + 'static,
    ) -> Div {
        let spec = Glass {
            tone,
            milk,
            radius: px(radius),
            underlay,
            seed,
        };
        glass(self.slots, id, spec, self.theme, content)
    }
}

pub fn icon(name: IconName, size: f32, color: Hsla) -> Icon {
    Icon::new(name).w(px(size)).h(px(size)).text_color(color)
}

pub fn status_dot(theme: &Theme) -> Div {
    div()
        .size(px(7.))
        .rounded_full()
        .bg(theme.ok_glyph)
        .shadow(vec![theme.status_dot_halo()])
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
    .size(px(20.))
}
