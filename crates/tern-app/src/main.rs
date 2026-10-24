mod assets;
mod bench;
mod files;
mod forwarding;
mod history;
mod host_editor;
mod identities;
mod keychain;
mod known_hosts;
#[cfg(test)]
mod lab;
mod live;
mod marks;
mod settings;
mod snippets;
mod sync;
mod term_input;
mod terminal;
mod theme;
mod ui;
mod vault;
mod vaults;
mod workspace;

use gpui_kit::*;

fn main() {
    bench::note_start();
    gpui_kit::application()
        .with_assets(assets::TernAssets)
        .run(|cx| {
            gpui_kit::init(cx);
            settings::init(cx);
            vaults::init(cx);
            cx.set_global(sync::SecretsStore(std::sync::Arc::new(
                tern_core::secrets::SystemSecrets,
            )));

            let benching = bench::enabled() || bench::launch();
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
                settings::apply_appearance(window, cx);
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
