# Tern

A free, open-source SSH client for macOS, Windows and Linux: a modern
alternative to PuTTY and Termius. Desktop only, no accounts, no Tern servers;
settings will sync through storage you already own (Google Drive, Dropbox,
OneDrive, iCloud, WebDAV or any folder).

Early development. Native Rust with [GPUI](https://gpui.rs) and
[GPUI Kit](https://gpui-kit.com); SSH via [russh](https://github.com/Eugeny/russh);
terminal emulation via [alacritty_terminal](https://crates.io/crates/alacritty_terminal).
What's built and what comes next: [ROADMAP.md](ROADMAP.md).

## Layout

| Crate | What it is |
| --- | --- |
| `crates/tern-core` | UI-free core: hosts and the vault, SSH sessions (direct or through jump hosts), host-key checks |
| `crates/tern-app` | The desktop app (`tern`) |
| `crates/tern-devserver` | Local SSH server with a real shell, for development |

## Run

```sh
cargo run -p tern-app                      # the app
cargo run -p tern-app -- --connect "ssh -p 2222 user@host"   # open a session at start
cargo run -p tern-app -- --connect prod-db-01                 # or one to a saved host
cargo run -p tern-app -- --local                              # open a local terminal at start
cargo run -p tern-app -- --edit prod-db-01                    # open the host editor (a new host without a name)
cargo run -p tern-app -- --settings sync                      # open Settings at a page
cargo run -p tern-app -- --show snippets                      # open the vault at a screen
cargo run -p tern-app -- --palette prod                       # open the command palette
```

The quick-connect field in the vault accepts the same `ssh …` commands
(`-p`, `-l` and `-i` are understood). Hosts are added and changed in the host
editor (New host, or the pencil on the selected host) and saved to
`vault.json` in Tern's data folder. The file holds no secrets: names,
addresses, users and key-file paths, like `~/.ssh/config`. If it can't be
read, Tern moves it aside as `vault.json.unreadable` and starts empty.

When PuTTY has saved SSH sessions the vault lacks (in the registry on
Windows, `~/.putty/sessions` elsewhere; only read), the vault offers "Import
from PuTTY" and adds them to a PuTTY group. Sessions with a PuTTY `.ppk` key
sign in through Pageant.

Settings (the sliders in the header) can sync hosts, snippets, port
forwards, settings and chosen private keys through a folder a sync client
already keeps in step (Google Drive, Dropbox, OneDrive, iCloud Drive,
Nextcloud…). Each device writes its own file there, encrypted with your
passphrase; the passphrase stays in the system's credential store.

Tern checks host keys against `~/.ssh/known_hosts` (read only) and its own
known hosts in its data folder (`%APPDATA%\Tern`, `~/Library/Application
Support/Tern` or `~/.local/share/tern`; `TERN_DATA_DIR` overrides it, which
also keeps a test run away from your real vault).

## Develop

```sh
cargo run -p tern-devserver                # SSH server on 127.0.0.1:2222
```

It creates a test key and password in `dev/test-ssh/` (git-ignored) on first
run, and serves SFTP over your home folder for the Files panel. When that
key exists and there is no `vault.json` yet, the app starts with a "Local
dev server" group listing it once directly and once as
`devserver-via-jump`, which goes through the dev server itself as a jump
host (it forwards only to addresses on this computer).

```sh
cargo test --workspace                     # includes an end-to-end SSH test
cargo run -p tern-core --example probe -- "ssh -p 2222 -i dev/test-ssh/id_ed25519 tern@127.0.0.1" "echo hi"
cargo run -p tern-core --example probe -- "ssh -p 2222 -i dev/test-ssh/id_ed25519 tern@127.0.0.1" "echo hi" --jump "ssh -p 2222 -i dev/test-ssh/id_ed25519 tern@127.0.0.1"
cargo run -p tern-app --release -- --bench 10                    # frame cost of the vault's latency traces
cargo run -p tern-app --release -- --bench 10 --bench-uncached   # same, re-rendering everything
```

`TERN_THEME=light|dark` forces an appearance.

### SSH lab

`dev/lab` runs real OpenSSH servers in Docker to test Tern against: one for
each way of signing in (a password, keys, a password then a one-time code, a
key then a code, user and host certificates), servers with only today's or
with older algorithms, a bastion → hop2 → deep chain into networks this
computer can't reach, SOCKS5 (Dante) and HTTP (Tinyproxy) proxies, and a web
server to forward to; every server has tmux and a stand-in `docker` with one
container. `entrypoint.sh` says what each offers. The lab's
tests are ignored unless asked for, and need `docker` on the `PATH`:

```sh
dev/lab/setup.sh                                              # make its keys, start it
cargo run -p tern-core --example lab_hosts                    # add its servers to Tern's vault (quit Tern first)
cargo test -p tern-core --test lab -- --ignored --nocapture   # Tern's SSH against every server
cargo test -p tern-app lab -- --ignored --nocapture           # the app's panes, keyboard only
docker compose -f dev/lab/compose.yaml down                   # stop it
```
