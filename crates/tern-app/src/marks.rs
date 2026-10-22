//! Tern's drawn marks, in ink on whatever surface they sit on: the host-key
//! art (static) and the latency trace (animated, painted by the live layer).

use gpui_kit::*;
use tern_core::HostStatus;
use tern_core::randomart::{Randomart, SIZE};

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
/// Samples the trace scrolls by per second.
const SAMPLE_RATE: f32 = 3.;
/// Latency at which the trace reaches its full height.
const ROUGHEST_MS: f32 = 40.;

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

/// The latency trace: a live line while the host answers (rougher the slower
/// it is), a dot sweeping a dashed line while connecting, a dotted flat line
/// when it doesn't answer, and a faint hairline before it's ever been
/// checked. `seed` keeps neighbouring traces apart.
pub fn paint_trace(
    bounds: Bounds<Pixels>,
    status: HostStatus,
    seed: u32,
    time: f32,
    ink: Hsla,
    faint: Hsla,
    window: &mut Window,
) {
    let (w, h) = (bounds.size.width.as_f32(), bounds.size.height.as_f32());
    let mid = h / 2.;
    match status {
        // Never checked: a plain hairline, neither alive nor dead.
        HostStatus::Unknown => {
            window.paint_quad(fill(
                rect(bounds, 2., mid - 0.5, w - 4., 1.),
                faint.opacity(0.6),
            ));
        }
        HostStatus::Offline => {
            let mut x = 2.;
            while x <= w - 2. {
                dot(offset(bounds, x, mid), 0.7, faint, window);
                x += 4.;
            }
        }
        HostStatus::Connecting => {
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
        }
        HostStatus::Online { latency_ms } => {
            let roughness = latency_ms.unwrap_or(0) as f32 / ROUGHEST_MS;
            let amp = (mid - 3.) * (0.45 + 0.55 * roughness.min(1.));
            let n = ((w / SAMPLE_PITCH).round() as i64).max(8);
            let step = (w - 6.) / (n - 1) as f32;
            let scroll = time * SAMPLE_RATE;
            let (newest, f) = (scroll.floor() as i64, scroll.fract());
            let y = |sample: f32| mid - sample * amp;

            let mut line = PathBuilder::stroke(px(1.25));
            for j in 0..n {
                let point = offset(
                    bounds,
                    3. + (j as f32 - f) * step,
                    y(jitter(seed, newest + j - n + 1)),
                );
                if j == 0 {
                    line.move_to(point);
                } else {
                    line.line_to(point);
                }
            }
            let (now, next) = (jitter(seed, newest), jitter(seed, newest + 1));
            let head = offset(bounds, w - 3., y(now + (next - now) * f));
            line.line_to(head);
            // The oldest sample scrolls out past the left edge.
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                if let Ok(path) = line.build() {
                    window.paint_path(path, ink);
                }
            });
            dot(head, 2., ink, window);
        }
    }
}

/// A stable per-host seed, so a host's trace looks the same across frames
/// and neighbours don't move in step.
pub fn seed(name: &str) -> u32 {
    name.bytes().fold(2_166_136_261u32, |h, b| {
        (h ^ b as u32).wrapping_mul(16_777_619)
    })
}

/// One latency sample around the line's middle, in -1..1: small jitter with
/// the odd spike upward.
fn jitter(seed: u32, index: i64) -> f32 {
    let base = (unit(seed, index.wrapping_mul(7)) - 0.5) * 0.7;
    let spike = if unit(seed.wrapping_add(1), index.wrapping_mul(13)) > 0.9 {
        0.75
    } else {
        0.
    };
    base + spike
}

/// A hash of `(seed, index)` mapped to 0..1.
fn unit(seed: u32, index: i64) -> f32 {
    let mut v = (index as u32 ^ seed ^ 0x9E37_79B9).wrapping_mul(0x85EB_CA6B);
    v ^= v >> 13;
    v = v.wrapping_mul(0xC2B2_AE35);
    v ^= v >> 16;
    v as f32 / 4_294_967_296.
}

fn offset(bounds: Bounds<Pixels>, x: f32, y: f32) -> Point<Pixels> {
    point(bounds.origin.x + px(x), bounds.origin.y + px(y))
}

fn rect(bounds: Bounds<Pixels>, x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
    Bounds::new(offset(bounds, x, y), size(px(w), px(h)))
}

/// Dots are rounded quads, which GPUI instances on the GPU; paths would need
/// tessellating on the CPU.
fn dot(center: Point<Pixels>, radius: f32, color: Hsla, window: &mut Window) {
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

fn ring(center: Point<Pixels>, radius: f32, color: Hsla, window: &mut Window) {
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
    use super::{jitter, seed};

    #[test]
    fn jitter_stays_inside_the_trace() {
        for host in ["prod-db-01", "bastion", ""] {
            for index in -500..500 {
                let sample = jitter(seed(host), index);
                assert!((-0.35..=1.1).contains(&sample), "{sample}");
            }
        }
    }
}
