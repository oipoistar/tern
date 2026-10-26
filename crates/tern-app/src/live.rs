//! Live marks: small drawings that change every frame (the latency traces),
//! painted by one layer over the cached screens.
//!
//! Screens are cached views, so animating inside them would force the whole
//! screen to re-render every frame (GPUI dirties every ancestor of a notified
//! view). Instead a screen holds placeholders that record where each mark
//! sits and how to draw it, and [`live_layer`] paints them all on top, once
//! per frame, from that record.

use std::cell::RefCell;
use std::collections::HashMap;
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
pub struct LiveSlots(Rc<RefCell<Recorded>>);

#[derive(Default)]
struct Recorded {
    slots: Vec<Slot>,
    /// Where each id's slot is in `slots`.
    at: HashMap<SharedString, usize>,
}

impl LiveSlots {
    /// Forget every mark; call at the start of the screen's render.
    pub fn clear(&self) {
        let mut recorded = self.0.borrow_mut();
        recorded.slots.clear();
        recorded.at.clear();
    }

    /// Whether anything on screen needs animating.
    pub fn is_empty(&self) -> bool {
        self.0.borrow().slots.is_empty()
    }

    fn record(&self, id: SharedString, slot: Slot) {
        let recorded = &mut *self.0.borrow_mut();
        match recorded.at.get(&id) {
            Some(&at) => recorded.slots[at] = slot,
            None => {
                recorded.at.insert(id, recorded.slots.len());
                recorded.slots.push(slot);
            }
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
/// after (on top of) the cached screen, filling the window. A mark its
/// screen clips away entirely (scrolled out of view) isn't drawn at all,
/// so the work follows the marks on screen, not all there are.
pub fn live_layer(slots: &LiveSlots, time: f32) -> impl IntoElement {
    let slots = slots.clone();
    canvas(
        |_, _, _| (),
        move |_, _, window, cx| {
            for slot in &slots.0.borrow().slots {
                if !slot.bounds.intersects(&slot.mask.bounds) {
                    continue;
                }
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

#[cfg(test)]
mod tests {
    // Explicit imports: `gpui_kit::*` would shadow Rust's `#[test]`.
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::TestAppContext;
    use gpui_kit::{
        Bounds, ContentMask, Context, IntoElement, ParentElement as _, Render, Styled as _, Window,
        div, point, px, size,
    };

    use super::{LiveSlots, Slot, live_layer};

    struct Layer(LiveSlots);

    impl Render for Layer {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(live_layer(&self.0, 0.))
        }
    }

    #[gpui::test]
    fn paints_only_marks_their_screen_shows(cx: &mut TestAppContext) {
        let painted = Rc::new(Cell::new(0));
        let slots = LiveSlots::default();
        let view = Bounds::new(point(px(0.), px(0.)), size(px(200.), px(100.)));
        let slot = |y: f32| {
            let painted = painted.clone();
            Slot {
                bounds: Bounds::new(point(px(10.), px(y)), size(px(80.), px(20.))),
                mask: ContentMask { bounds: view },
                paint: Rc::new(move |_, _, _, _| painted.set(painted.get() + 1)),
            }
        };
        slots.record("shown".into(), slot(10.));
        // Scrolled out of its screen's view.
        slots.record("below".into(), slot(400.));
        // Prepainted again: replaced, not added.
        slots.record("shown".into(), slot(20.));
        assert_eq!(slots.0.borrow().slots.len(), 2);

        let layer = slots.clone();
        let (_, cx) = cx.add_window_view(move |_, _| Layer(layer));
        cx.run_until_parked();
        assert_eq!(painted.get(), 1);
    }
}
