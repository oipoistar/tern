// On Windows Tern is a windowed program, so no console opens behind it;
// started from a terminal it writes there (`attach_console`). Tests keep
// their console.
#![cfg_attr(not(test), windows_subsystem = "windows")]

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
mod mcp;
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
    #[cfg(windows)]
    load_libraries_from_system32();
    // `tern --mcp`: the MCP server an agent starts, speaking MCP on standard
    // input and output and handing tool calls to the Tern running here. No
    // window, and nothing else on standard output.
    if std::env::args().nth(1).as_deref() == Some("--mcp") {
        if let Err(error) = tern_core::mcp::serve_stdio() {
            eprintln!("tern --mcp: {error}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(windows)]
    attach_console();
    bench::note_start();
    gpui_kit::application()
        .with_assets(assets::TernAssets)
        .run(|cx| {
            gpui_kit::init(cx);
            terminal::bind_keys(cx);
            settings::init(cx);
            vaults::init(cx);
            cx.set_global(sync::SecretsStore(std::sync::Arc::new(
                tern_core::secrets::SystemSecrets,
            )));

            let benching = bench::enabled() || bench::launch();
            let options = workspace::window_options(
                if benching {
                    bench::window_bounds()
                } else {
                    WindowBounds::centered(size(px(1280.), px(820.)), cx)
                },
                !benching,
            );
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

/// Libraries load from System32 only, never from Tern's own folder: a
/// Downloads folder, perhaps, where a file named like one could be waiting
/// (portable-pty would load a `conpty.dll` there into every local
/// terminal). The ones tern.exe imports are looked for there too
/// (`/DEPENDENTLOADFLAG`, in build.rs).
#[cfg(windows)]
fn load_libraries_from_system32() {
    unsafe extern "system" {
        fn SetDefaultDllDirectories(flags: u32) -> i32;
    }
    const LOAD_LIBRARY_SEARCH_SYSTEM32: u32 = 0x800;
    unsafe {
        SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32);
    }
}

/// Started from a console (`--bench` prints its results there), the
/// windowed program writes to that console.
#[cfg(windows)]
fn attach_console() {
    unsafe extern "system" {
        fn AttachConsole(process: u32) -> i32;
    }
    // ATTACH_PARENT_PROCESS; started from Explorer, there's none to join.
    unsafe {
        AttachConsole(u32::MAX);
    }
}
