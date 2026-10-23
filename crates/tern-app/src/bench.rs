//! `tern --bench <seconds>`: open the window off-screen and unfocused, keep the
//! latency traces moving, then print how long each frame's work took and quit.
//!
//! A frame is timed with the high-resolution clock from the start of the
//! root's render to the end of the live layer's paint, which covers render,
//! layout, prepaint and paint of the whole tree (not the GPU or present). OS
//! CPU-time counters are no use here: Windows charges them in 15.6 ms ticks.
//! Median and 90th percentile keep the odd slow frame from swinging results.

use std::cell::{Cell, RefCell};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gpui_kit::{App, Bounds, WindowBounds, point, px, size};
use tern_core::reach::{KEEP, Monitor};
use tern_core::{Host, HostGroup, Vault};

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
/// before the live layer existed, so both can be compared in one binary.
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

/// What the benchmark draws: seven hosts in two groups, the same every run
/// and never saved.
pub fn vault() -> Vault {
    let group = |name: &str, hosts: &[(&str, &str)]| HostGroup {
        name: name.into(),
        hosts: hosts
            .iter()
            .map(|(name, address)| Host::new(name, "deploy", address, 22))
            .collect(),
    };
    Vault {
        groups: vec![
            group(
                "Production",
                &[
                    ("prod-db-01", "10.0.4.21"),
                    ("prod-web-01", "10.0.4.22"),
                    ("prod-web-02", "10.0.4.23"),
                    ("bastion", "10.0.4.1"),
                ],
            ),
            group(
                "Staging",
                &[
                    ("stage-api", "10.0.8.10"),
                    ("stage-worker", "10.0.8.11"),
                    ("pi-homelab", "192.168.1.40"),
                ],
            ),
        ],
        snippets: Vec::new(),
        identities: Vec::new(),
        deleted: Vec::new(),
    }
}

/// Gives every host in `vault` a full trace of made-up checks, the odd one
/// failed, so the benchmark draws what a busy vault shows.
pub fn fill(monitor: &mut Monitor, vault: &Vault) {
    let now = Instant::now();
    for (n, host) in vault.hosts().enumerate() {
        let place = vault.first_hop(host);
        for i in 0..KEEP {
            let roll = (n * 31 + i * 17) % 23;
            let latency_ms = (roll != 0).then_some(4 + (n * 7 + roll) as u32 % 40);
            monitor.record(&place, latency_ms, now);
        }
    }
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
