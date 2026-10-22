mod assets;
mod bench;
mod forwarding;
mod history;
mod host_editor;
mod keychain;
mod known_hosts;
mod live;
mod marks;
mod snippets;
mod terminal;
mod theme;
mod ui;
mod vault;
mod workspace;

use gpui_kit::*;

fn main() {
    gpui_kit::application()
        .with_assets(assets::TernAssets)
        .run(|cx| {
            gpui_kit::init(cx);

            let benching = bench::enabled();
            let options = WindowOptions {
                window_bounds: Some(if benching {
                    bench::window_bounds()
                } else {
                    WindowBounds::centered(size(px(1280.), px(820.)), cx)
                }),
                titlebar: Some(TitlebarOptions {
                    title: Some("Tern".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(720.), px(480.))),
                focus: !benching,
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                // Start in the system's appearance; `TERN_THEME=light|dark`
                // overrides it (handy for screenshots).
                let dark = match std::env::var("TERN_THEME").as_deref() {
                    Ok("dark") => true,
                    Ok("light") => false,
                    _ => matches!(
                        window.appearance(),
                        WindowAppearance::Dark | WindowAppearance::VibrantDark
                    ),
                };
                theme::set_dark(dark, cx);
                cx.new(|cx| workspace::Workspace::new(window, cx))
            })
            .expect("failed to open the Tern window");

            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            if benching {
                bench::start(cx);
            } else {
                cx.activate(true);
            }
        });
}
