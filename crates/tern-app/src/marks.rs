//! Tern's drawn marks, in ink on whatever surface they sit on: the host-key
//! art (static) and the latency trace (animated, painted by the live layer).

use std::rc::Rc;
use std::time::Instant;

use gpui_kit::*;
use tern_core::HostStatus;
use tern_core::randomart::{Randomart, SIZE};
use tern_core::reach::CHECK_EVERY;

/// Dot radius by how often the bishop landed on a square (0, 1, 2, … 5+),
/// for the vault card's 3.6 px pitch; the largest still leaves a gap.
/// Smaller tiles scale them down.
const ART_RADIUS: [f32; 6] = [0.55, 0.9, 1.15, 1.35, 1.5, 1.6];
const ART_PITCH: f32 = 3.6;
/// Gap between the art's outer dots and the tile edge, as a share of its
/// width.
const ART_PAD: f32 = 0.14;

/// Width of one latency sample on the trace.
const SAMPLE_PITCH: f32 = 7.;
/// The least spread of latencies that fills the trace's height, so a
/// steady host draws a nearly flat line instead of magnified noise.
const STEADY_MS: f32 = 10.;
/// Space kept above and below the line for the head dot.
const TRACE_PAD: f32 = 3.;

/// Host-key art: the drunken-bishop field as dots, with a ring where the
/// bishop stopped. Without a known key the field is empty, so an untrusted
/// host reads as blank rather than as a different host.
pub fn paint_art(
    bounds: Bounds<Pixels>,
    art: Option<&Randomart>,
    ink: Hsla,
    faint: Hsla,
    window: &mut Window,
) {
    let width = bounds.size.width.as_f32();
    let pad = width * ART_PAD;
    let pitch = (width - 2. * pad) / (SIZE - 1) as f32;
    let scale = (pitch / ART_PITCH).min(1.);
    let at = |x: usize, y: usize| offset(bounds, pad + x as f32 * pitch, pad + y as f32 * pitch);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let visits = art.map_or(0, |art| art.visits(x, y)) as usize;
            let color = if visits > 0 { ink } else { faint.opacity(0.35) };
            dot(at(x, y), ART_RADIUS[visits.min(5)] * scale, color, window);
        }
    }
    if let Some(art) = art {
        let (x, y) = art.end();
        ring(at(x, y), 3.2 * scale, ink, window);
    }
}

/// What a latency trace shows: a host's status and its recent checks.
#[derive(Clone)]
pub struct Trace {
    pub status: HostStatus,
    /// Round trips in milliseconds, oldest first; `None` for a check that
    /// failed.
    pub samples: Rc<[Option<u32>]>,
    /// When the newest sample was taken. The line slides left from then
    /// on, by one sample's width over a check interval, so it moves with
    /// time and doesn't jump when the next sample lands. `None` draws it
    /// fully slid.
    pub sampled_at: Option<Instant>,
}

impl Trace {
    /// One check's result, like the host editor's Test connection.
    pub fn single(latency_ms: u32) -> Self {
        Self {
            status: HostStatus::online(latency_ms),
            samples: Rc::new([Some(latency_ms)]),
            sampled_at: None,
        }
    }
}

/// The latency trace, with now at the right edge: a line through the
/// checks that answered (higher when slower) ending in a dot, dots where a
/// check failed, a faint hairline where nothing was checked yet, and a dot
/// sweeping a dashed line while the first check runs. `seed` keeps
/// neighbouring sweeps apart.
pub fn paint_trace(
    bounds: Bounds<Pixels>,
    trace: &Trace,
    seed: u32,
    time: f32,
    ink: Hsla,
    faint: Hsla,
    window: &mut Window,
) {
    let (w, h) = (bounds.size.width.as_f32(), bounds.size.height.as_f32());
    let mid = h / 2.;
    if trace.samples.is_empty() {
        if trace.status == HostStatus::Connecting {
            let mut x = 2.;
            while x + 2. <= w - 2. {
                window.paint_quad(fill(rect(bounds, x, mid - 0.5, 2., 1.), faint));
                x += 5.;
            }
            let phase = (time * 0.45 + (seed % 1000) as f32 / 1000.).fract();
            let head = 3. + phase * (w - 6.);
            let tail = (head - 24.).max(3.);
            window.paint_quad(fill(rect(bounds, tail, mid - 0.6, head - tail, 1.2), ink));
            dot(offset(bounds, head, mid), 2., ink, window);
        } else {
            hairline(bounds, 2., w - 2., faint, window);
        }
        return;
    }

    let slide = trace.sampled_at.map_or(1., |at| {
        (at.elapsed().as_secs_f32() / CHECK_EVERY.as_secs_f32()).min(1.)
    });
    let points = sample_points(&trace.samples, w, h, slide);
    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        // Before the first check.
        if let Some(&(x, _)) = points.first() {
            hairline(bounds, 2., x, faint, window);
        }
        // The answered stretch being drawn: the path and its segment count.
        let mut line: Option<(PathBuilder, usize)> = None;
        let finish = |line: &mut Option<(PathBuilder, usize)>, window: &mut Window| {
            if let Some((path, segments)) = line.take()
                && segments > 0
                && let Ok(path) = path.build()
            {
                window.paint_path(path, ink);
            }
        };
        for (i, &(x, y)) in points.iter().enumerate() {
            // Each sample holds until the next one, or until now.
            let until = points.get(i + 1).map_or(w - TRACE_PAD, |next| next.0);
            match y {
                Some(y) => {
                    let point = offset(bounds, x, y);
                    match &mut line {
                        Some((path, segments)) => {
                            path.line_to(point);
                            *segments += 1;
                        }
                        None => {
                            let mut path = PathBuilder::stroke(px(1.25));
                            path.move_to(point);
                            line = Some((path, 0));
                        }
                    }
                    if i + 1 == points.len()
                        && until > x
                        && let Some((path, segments)) = &mut line
                    {
                        path.line_to(offset(bounds, until, y));
                        *segments += 1;
                    }
                }
                None => {
                    finish(&mut line, window);
                    let mut dx = x;
                    while dx < until {
                        dot(offset(bounds, dx, mid), 0.7, faint, window);
                        dx += 4.;
                    }
                }
            }
        }
        finish(&mut line, window);
    });
    if let Some(&(_, Some(y))) = points.last() {
        dot(offset(bounds, w - TRACE_PAD, y), 2., ink, window);
    }
}

/// Where each sample sits on a `width` × `height` trace: the newest
/// `slide` samples' width left of the right edge, older ones a pitch apart
/// before it. A failed check has no height. Heights are centred on the
/// spread of answered checks, which fills the trace once it passes
/// [`STEADY_MS`].
fn sample_points(
    samples: &[Option<u32>],
    width: f32,
    height: f32,
    slide: f32,
) -> Vec<(f32, Option<f32>)> {
    let answered = samples.iter().flatten().map(|&ms| ms as f32);
    let (low, high) = answered.fold((f32::MAX, f32::MIN), |(low, high), ms| {
        (low.min(ms), high.max(ms))
    });
    let spread = (high - low).max(STEADY_MS);
    let centre = (low + high) / 2.;
    let usable = height - 2. * TRACE_PAD;
    let newest = width - TRACE_PAD - slide * SAMPLE_PITCH;
    let last = samples.len().saturating_sub(1);
    samples
        .iter()
        .enumerate()
        .map(|(i, sample)| {
            let x = newest - (last - i) as f32 * SAMPLE_PITCH;
            let y = sample.map(|ms| height / 2. - (ms as f32 - centre) / spread * usable);
            (x, y)
        })
        .collect()
}

/// A stable per-host seed, so neighbouring marks don't move in step.
pub fn seed(name: &str) -> u32 {
    name.bytes().fold(2_166_136_261u32, |h, b| {
        (h ^ b as u32).wrapping_mul(16_777_619)
    })
}

/// A faint level line from `from` to `to` across the middle of `bounds`.
fn hairline(bounds: Bounds<Pixels>, from: f32, to: f32, faint: Hsla, window: &mut Window) {
    if to > from {
        let mid = bounds.size.height.as_f32() / 2.;
        window.paint_quad(fill(
            rect(bounds, from, mid - 0.5, to - from, 1.),
            faint.opacity(0.6),
        ));
    }
}

fn offset(bounds: Bounds<Pixels>, x: f32, y: f32) -> Point<Pixels> {
    point(bounds.origin.x + px(x), bounds.origin.y + px(y))
}

fn rect(bounds: Bounds<Pixels>, x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
    Bounds::new(offset(bounds, x, y), size(px(w), px(h)))
}

/// Dots are rounded quads, which GPUI instances on the GPU; paths would need
/// tessellating on the CPU.
pub fn dot(center: Point<Pixels>, radius: f32, color: Hsla, window: &mut Window) {
    window.paint_quad(quad(
        Bounds::new(
            point(center.x - px(radius), center.y - px(radius)),
            size(px(2. * radius), px(2. * radius)),
        ),
        px(radius),
        color,
        px(0.),
        transparent_black(),
        BorderStyle::Solid,
    ));
}

pub fn ring(center: Point<Pixels>, radius: f32, color: Hsla, window: &mut Window) {
    window.paint_quad(quad(
        Bounds::new(
            point(center.x - px(radius), center.y - px(radius)),
            size(px(2. * radius), px(2. * radius)),
        ),
        px(radius),
        transparent_black(),
        px(1.),
        color,
        BorderStyle::Solid,
    ));
}

#[cfg(test)]
mod tests {
    use super::{SAMPLE_PITCH, TRACE_PAD, sample_points};

    #[test]
    fn samples_run_up_to_now_at_the_right_edge() {
        let points = sample_points(&[Some(10), None, Some(30)], 100., 18., 0.5);
        let xs: Vec<f32> = points.iter().map(|point| point.0).collect();
        let newest = 100. - TRACE_PAD - 0.5 * SAMPLE_PITCH;
        assert_eq!(
            xs,
            [newest - 2. * SAMPLE_PITCH, newest - SAMPLE_PITCH, newest]
        );
        // A failed check has no height; slower is higher, inside the pad.
        let (slow, fast) = (points[2].1.unwrap(), points[0].1.unwrap());
        assert!(points[1].1.is_none());
        assert!(slow < fast);
        assert_eq!((slow, fast), (TRACE_PAD, 18. - TRACE_PAD));
    }

    #[test]
    fn a_steady_host_draws_a_nearly_flat_line() {
        let points = sample_points(&[Some(20), Some(21), Some(20)], 100., 18., 0.);
        for (_, y) in points {
            assert!((y.unwrap() - 9.).abs() < 1., "{y:?}");
        }
        let single = sample_points(&[Some(500)], 100., 18., 0.);
        assert_eq!(single, [(100. - TRACE_PAD, Some(9.))]);
    }
}
