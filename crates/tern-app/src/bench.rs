//! `tern --bench <seconds>`: open the window off-screen and unfocused, keep the
//! glass animating, then print how long each frame's work took and quit.
//!
//! A frame is timed with the high-resolution clock from the start of the
//! vault's render to the end of the glass layer's paint, which covers render,
//! layout, prepaint and paint of the whole tree (not the GPU or present). OS
//! CPU-time counters are no use here: Windows charges them in 15.6 ms ticks.
//! Median and 90th percentile keep the odd slow frame from swinging results.

use std::cell::{Cell, RefCell};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gpui_kit::{App, Bounds, WindowBounds, point, px, size};

/// Seconds discarded before sampling, so startup work doesn't skew results.
const WARMUP: Duration = Duration::from_secs(2);

static SECONDS: OnceLock<Option<f32>> = OnceLock::new();

thread_local! {
    static FRAME_START: Cell<Option<Instant>> = const { Cell::new(None) };
    /// Duration of each frame's work, once sampling has started.
    static SAMPLES: RefCell<Option<Vec<Duration>>> = const { RefCell::new(None) };
}

/// Reads `--bench <seconds>` from the command line once.
pub fn seconds() -> Option<f32> {
    *SECONDS.get_or_init(|| {
        let mut args = std::env::args().skip_while(|arg| arg != "--bench").skip(1);
        args.next().and_then(|secs| secs.parse().ok())
    })
}

pub fn enabled() -> bool {
    seconds().is_some()
}

/// `--bench-uncached` re-renders the whole screen every frame, as Tern did
/// before the glass layer existed, so both can be compared in one binary.
pub fn uncached() -> bool {
    enabled() && std::env::args().any(|arg| arg == "--bench-uncached")
}

/// Where the benchmark window goes: off every screen, so neither the pointer
/// nor anything else on the desktop can disturb it.
pub fn window_bounds() -> WindowBounds {
    WindowBounds::Windowed(Bounds::new(
        point(px(-6000.), px(-6000.)),
        size(px(1280.), px(820.)),
    ))
}

/// Marks the start of a frame's work; call first thing in the root render.
pub fn frame_start() {
    if enabled() {
        FRAME_START.set(Some(Instant::now()));
    }
}

/// Marks the end of a frame's work; call after the last paint.
pub fn frame_end() {
    let Some(started) = FRAME_START.take() else {
        return;
    };
    SAMPLES.with_borrow_mut(|samples| {
        if let Some(samples) = samples {
            samples.push(started.elapsed());
        }
    });
}

/// Samples after the warm-up, prints the result and quits the app.
pub fn start(cx: &mut App) {
    let Some(seconds) = seconds() else { return };
    let window = Duration::from_secs_f32(seconds);
    // Runs on the UI thread, like the frame hooks, so it sees their samples.
    cx.spawn(async move |cx| {
        cx.background_executor().timer(WARMUP).await;
        SAMPLES.set(Some(Vec::new()));
        let at0 = Instant::now();
        cx.background_executor().timer(window).await;
        let wall = at0.elapsed().as_secs_f64();
        let mut ms: Vec<f64> = SAMPLES.take().unwrap_or_default().iter().map(|d| d.as_secs_f64() * 1000.).collect();
        ms.sort_by(f64::total_cmp);

        let pick = |q: f64| ms.get(((ms.len() as f64 - 1.) * q).round() as usize).copied().unwrap_or(0.);
        let busy: f64 = ms.iter().sum::<f64>() / 1000.;
        println!(
            "bench: {wall:.1} s, {} frames ({:.1} fps) | frame work: median {:.3} ms, p90 {:.3} ms | {:.2}% of the UI thread",
            ms.len(),
            ms.len() as f64 / wall,
            pick(0.5),
            pick(0.9),
            busy / wall * 100.,
        );
        cx.update(|cx| cx.quit());
    })
    .detach();
}
