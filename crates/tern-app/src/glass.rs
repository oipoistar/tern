//! Voxel glass: animated voxels seen through a milky layer with a liquid-glass
//! rim. Used for accents and statuses only, never as a fill for ordinary
//! surfaces.
//!
//! The static UI is a cached view, so animating inside it would force the
//! whole screen to re-render every frame (GPUI dirties every ancestor of a
//! notified view). Instead the static UI holds invisible placeholders that
//! record where each glass surface sits, and [`glass_layer`] paints all of
//! them on top, once per frame, from that record.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::*;

use crate::bench;
use crate::theme::{Milk, Theme, Tone};

/// Width of one voxel column.
const CELL: f32 = 10.;
/// Visible height of a voxel's top face; also the row pitch.
const TOP: f32 = 7.;
/// Visible height of a voxel's front face, seen from slightly above.
const FRONT: f32 = 3.;

/// How a glass surface should look.
#[derive(Clone, Copy)]
pub struct Glass {
    pub tone: Tone,
    pub milk: Milk,
    pub radius: Pixels,
    /// Colour directly behind the surface, used to mask the voxels into the
    /// rounded shape (GPUI clips to rectangles only).
    pub underlay: Hsla,
    /// Phase offset so neighbouring surfaces don't ripple in lockstep.
    pub seed: f32,
}

/// Where the static UI placed one glass surface during its last prepaint.
struct Slot {
    bounds: Bounds<Pixels>,
    mask: ContentMask<Pixels>,
    glass: Glass,
    theme: Rc<Theme>,
    content: Rc<dyn Fn() -> AnyElement>,
}

/// Glass surfaces recorded by the static UI, keyed by a stable id so a repeated
/// prepaint replaces its entry instead of duplicating it.
#[derive(Clone, Default)]
pub struct GlassSlots(Rc<RefCell<Vec<(SharedString, Slot)>>>);

impl GlassSlots {
    /// Forget every surface; call at the start of the static UI's render.
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    fn record(&self, id: SharedString, slot: Slot) {
        let mut slots = self.0.borrow_mut();
        match slots.iter_mut().find(|(existing, _)| *existing == id) {
            Some(entry) => entry.1 = slot,
            None => slots.push((id, slot)),
        }
    }
}

/// A placeholder for a glass surface in the static UI. It takes the size of
/// `content` without painting it and records its bounds for [`glass_layer`],
/// which draws the surface and `content` on top. Put click handlers on the
/// placeholder (or an ancestor); the layer above has no hitboxes.
pub fn glass<E: IntoElement>(
    slots: &GlassSlots,
    id: impl Into<SharedString>,
    glass: Glass,
    theme: &Rc<Theme>,
    content: impl Fn() -> E + 'static,
) -> Div {
    let content: Rc<dyn Fn() -> AnyElement> = Rc::new(move || content().into_any_element());
    let (slots, id, theme) = (slots.clone(), id.into(), theme.clone());
    let placeholder = div().invisible().child(content());
    div().relative().child(placeholder).child(
        canvas(
            move |bounds, window, _| {
                let mask = window.content_mask();
                slots.record(
                    id,
                    Slot {
                        bounds,
                        mask,
                        glass,
                        theme,
                        content,
                    },
                );
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full(),
    )
}

/// Paints every recorded glass surface at `time` seconds into the animation.
/// Render it after (on top of) the cached static UI, filling the window.
pub fn glass_layer(slots: &GlassSlots, time: f32) -> impl IntoElement {
    let slots = slots.clone();
    canvas(
        move |_, window, cx| {
            slots
                .0
                .borrow()
                .iter()
                .map(|(_, slot)| {
                    let mut surface = surface(slot, time).into_any_element();
                    window.with_content_mask(Some(slot.mask), |window| {
                        surface.prepaint_as_root(
                            slot.bounds.origin,
                            slot.bounds.size.into(),
                            window,
                            cx,
                        );
                    });
                    (slot.mask, surface)
                })
                .collect::<Vec<_>>()
        },
        |_, mut surfaces, window, cx| {
            for (mask, surface) in &mut surfaces {
                window.with_content_mask(Some(*mask), |window| surface.paint(window, cx));
            }
            bench::frame_end();
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

fn surface(slot: &Slot, time: f32) -> Div {
    let Glass {
        tone,
        milk,
        radius,
        underlay,
        seed,
    } = slot.glass;
    let theme = &slot.theme;
    let dark = theme.dark;
    div()
        .w(slot.bounds.size.width)
        .h(slot.bounds.size.height)
        .relative()
        .rounded(radius)
        .shadow(theme.glass_shadow())
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    window.with_content_mask(Some(ContentMask { bounds }), |window| {
                        paint_voxels(bounds, tone, dark, seed, time, window);
                        mask_corners(bounds, radius, underlay, window);
                    });
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .rounded(radius)
                .bg(theme.milk(milk))
                .shadow(theme.glass_edge()),
        )
        .child(
            div()
                .relative()
                .text_color(theme.glass_text)
                .child((slot.content)()),
        )
}

/// Voxels are plain quads (a lit top face over a shaded front face), which
/// GPUI instances on the GPU; paths would need CPU tessellation every frame.
fn paint_voxels(
    bounds: Bounds<Pixels>,
    tone: Tone,
    dark: bool,
    seed: f32,
    time: f32,
    window: &mut Window,
) {
    let (hue, sat) = tone.hue_sat();
    // Lightness of the backdrop, front faces and top faces. Faces sit edge to
    // edge; only the shaded front faces separate one voxel from the next.
    let base = if dark {
        [0.30, 0.34, 0.46]
    } else {
        [0.58, 0.62, 0.74]
    };
    window.paint_quad(fill(bounds, hsla(hue / 360., sat, base[0], 1.)));

    let cols = (bounds.size.width.as_f32() / CELL).ceil() as i32 + 1;
    let rows = (bounds.size.height.as_f32() / TOP).ceil() as i32 + 2;
    let origin = bounds.origin;
    let rect = |x: f32, y: f32, w: f32, h: f32| {
        Bounds::new(
            point(origin.x + px(x), origin.y + px(y)),
            size(px(w), px(h)),
        )
    };

    // Rows go back to front, so nearer rows cover the front faces behind them.
    for r in 0..rows {
        for c in 0..cols {
            let wave = (c as f32 * 0.55 + time * 1.4 + seed).sin()
                + (r as f32 * 0.45 - time * 1.1 + seed * 0.5).cos();
            let k = (wave + 2.) / 4.;
            let x = c as f32 * CELL;
            let y = r as f32 * TOP - FRONT - wave * 2.5;
            let face_hue = (hue + (k - 0.5) * 26.) / 360.;
            window.paint_quad(fill(
                rect(x, y, CELL, TOP),
                hsla(face_hue, sat, base[2] + k * 0.10, 1.),
            ));
            window.paint_quad(fill(
                rect(x, y + TOP, CELL, FRONT),
                hsla(face_hue, sat, base[1] + k * 0.06, 1.),
            ));
        }
    }
}

/// Paints the underlay colour outside the rounded rectangle. A quad whose
/// border's inner edge is exactly the surface's outline does the job.
fn mask_corners(bounds: Bounds<Pixels>, radius: Pixels, underlay: Hsla, window: &mut Window) {
    let band = px(24.);
    window.paint_quad(quad(
        bounds.dilate(band),
        radius + band,
        transparent_black(),
        band,
        underlay,
        BorderStyle::Solid,
    ));
}
