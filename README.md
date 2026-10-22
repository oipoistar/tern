# Tern

A free, open-source SSH client for macOS, Windows and Linux: a modern
alternative to PuTTY and Termius. Desktop only, no accounts, no Tern servers;
settings will sync through storage you already own (Google Drive, Dropbox,
OneDrive, iCloud, WebDAV or any folder).

Early development. Native Rust with [GPUI](https://gpui.rs) and
[GPUI Kit](https://gpui-kit.com); SSH via [russh](https://github.com/Eugeny/russh);
terminal emulation via [alacritty_terminal](https://crates.io/crates/alacritty_terminal).

## Layout

| Crate | What it is |
| --- | --- |
| `crates/tern-core` | UI-free core: hosts, SSH sessions, host-key checks |
| `crates/tern-app` | The desktop app (`tern`) |
| `crates/tern-devserver` | Local SSH server with a real shell, for development |

## Run

```sh
cargo run -p tern-app                      # the app
cargo run -p tern-app -- --connect "ssh -p 2222 user@host"   # open a session at start
```

The quick-connect field in the vault accepts the same `ssh …` commands
(`-p`, `-l` and `-i` are understood).

Tern checks host keys against `~/.ssh/known_hosts` (read only) and its own
known hosts in its data folder (`%APPDATA%\Tern`, `~/Library/Application
Support/Tern` or `~/.local/share/tern`; `TERN_DATA_DIR` overrides it).

## Develop

```sh
cargo run -p tern-devserver                # SSH server on 127.0.0.1:2222
```

It creates a test key and password in `dev/test-ssh/` (git-ignored) on first
run, and the app then lists it under "Local dev server".

```sh
cargo test --workspace                     # includes an end-to-end SSH test
cargo run -p tern-core --example probe -- "ssh -p 2222 -i dev/test-ssh/id_ed25519 tern@127.0.0.1" "echo hi"
cargo run -p tern-app --release -- --bench 10                    # frame cost of the vault's latency traces
cargo run -p tern-app --release -- --bench 10 --bench-uncached   # same, re-rendering everything
```

`TERN_THEME=light|dark` forces an appearance.
