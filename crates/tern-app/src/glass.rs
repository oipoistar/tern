//! Voxel glass: animated isometric voxels seen through a milky layer with a
//! liquid-glass rim. Used for accents and statuses only, never as a fill for
//! ordinary surfaces.

use gpui_kit::*;

use crate::theme::{Milk, Theme, Tone};

/// Width of one voxel column.
const CELL: f32 = 10.;
/// Visible height of a voxel's top face; also the row pitch.
const TOP: f32 = 7.;
/// Visible height of a voxel's front face, seen from slightly above.
const FRONT: f32 = 3.;

/// Where an animated glass surface sits and how it should look.
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

/// Wraps `content` in a voxel-glass surface. `time` is the animation clock in
/// seconds; pass a constant to freeze it.
pub fn glass(glass: Glass, theme: &Theme, time: f32, content: impl IntoElement) -> Div {
    let Glass {
        tone,
        milk,
        radius,
        underlay,
        seed,
    } = glass;
    let dark = theme.dark;
    div()
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
            .size_full(),
        )
        .child(
            div()
                .absolute()
                .size_full()
                .rounded(radius)
                .bg(theme.milk(milk))
                .shadow(theme.glass_edge()),
        )
        .child(div().relative().text_color(theme.glass_text).child(content))
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
