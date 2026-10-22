//! Live marks: small drawings that change every frame (the latency traces),
//! painted by one layer over the cached screens.
//!
//! Screens are cached views, so animating inside them would force the whole
//! screen to re-render every frame (GPUI dirties every ancestor of a notified
//! view). Instead a screen holds placeholders that record where each mark
//! sits and how to draw it, and [`live_layer`] paints them all on top, once
//! per frame, from that record.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::*;

use crate::bench;

/// Draws a mark into its bounds at `time` seconds into the animation.
type Painter = Rc<dyn Fn(Bounds<Pixels>, f32, &mut Window, &mut App)>;

/// Where a screen placed one mark during its last prepaint.
struct Slot {
    bounds: Bounds<Pixels>,
    mask: ContentMask<Pixels>,
    paint: Painter,
}

/// Marks recorded by a screen, keyed by a stable id so a repeated prepaint
/// replaces its entry instead of duplicating it.
#[derive(Clone, Default)]
pub struct LiveSlots(Rc<RefCell<Vec<(SharedString, Slot)>>>);

impl LiveSlots {
    /// Forget every mark; call at the start of the screen's render.
    pub fn clear(&self) {
        self.0.borrow_mut().clear();
    }

    /// Whether anything on screen needs animating.
    pub fn is_empty(&self) -> bool {
        self.0.borrow().is_empty()
    }

    fn record(&self, id: SharedString, slot: Slot) {
        let mut slots = self.0.borrow_mut();
        match slots.iter_mut().find(|(existing, _)| *existing == id) {
            Some(entry) => entry.1 = slot,
            None => slots.push((id, slot)),
        }
    }
}

/// A placeholder for a live mark. Size it like any element; it paints nothing
/// itself, but records its bounds so [`live_layer`] calls `paint` there every
/// frame.
pub fn live(
    slots: &LiveSlots,
    id: impl Into<SharedString>,
    paint: impl Fn(Bounds<Pixels>, f32, &mut Window, &mut App) + 'static,
) -> Canvas<()> {
    let (slots, id) = (slots.clone(), id.into());
    let paint: Painter = Rc::new(paint);
    canvas(
        move |bounds, window, _| {
            let mask = window.content_mask();
            slots.record(
                id,
                Slot {
                    bounds,
                    mask,
                    paint,
                },
            );
        },
        |_, _, _, _| {},
    )
}

/// Paints every recorded mark at `time` seconds into the animation. Render it
/// after (on top of) the cached screen, filling the window.
pub fn live_layer(slots: &LiveSlots, time: f32) -> impl IntoElement {
    let slots = slots.clone();
    canvas(
        |_, _, _| (),
        move |_, _, window, cx| {
            for (_, slot) in slots.0.borrow().iter() {
                window.with_content_mask(Some(slot.mask), |window| {
                    (slot.paint)(slot.bounds, time, window, cx)
                });
            }
            bench::frame_end();
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}
