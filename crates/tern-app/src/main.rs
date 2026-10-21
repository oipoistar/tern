mod assets;
mod glass;
mod theme;
mod vault;

use gpui_kit::*;

fn main() {
    gpui_kit::application()
        .with_assets(assets::TernAssets)
        .run(|cx| {
            gpui_kit::init(cx);

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size(px(1280.), px(820.)), cx)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Tern".into()),
                    ..Default::default()
                }),
                window_min_size: Some(size(px(720.), px(480.))),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| vault::Vault::new(window, cx))
            })
            .expect("failed to open the Tern window");

            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
}
