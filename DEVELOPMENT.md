# Developing Tern

How to build, run and check Tern from source. The rules for changing it
(checks before a commit, UI rules, gotchas) are in [AGENTS.md](AGENTS.md);
what comes next is in [ROADMAP.md](ROADMAP.md).

## Layout

| Crate | What it is |
| --- | --- |
| `crates/tern-core` | UI-free core: hosts and the vault, SSH sessions (direct or through jump hosts), host-key checks |
| `crates/tern-app` | The desktop app (`tern`), on [GPUI](https://gpui.rs) and [GPUI Kit](https://gpui-kit.com) |
| `crates/tern-devserver` | Local SSH server with a real shell, for development |

SSH is [russh](https://github.com/Eugeny/russh) (with the `ring` backend);
terminal emulation is [alacritty_terminal](https://crates.io/crates/alacritty_terminal).

## Run

Clone the repository and, with [Rust](https://rustup.rs) installed (and on
Linux the libraries below):

```sh
cargo run -p tern-app --release            # the app, as it's released
cargo run -p tern-app                      # the app
cargo run -p tern-app -- --connect "ssh -p 2222 user@host"   # open a session at start
cargo run -p tern-app -- --connect prod-db-01                 # or one to a saved host
cargo run -p tern-app -- --local                              # open a local terminal at start
cargo run -p tern-app -- --play session.cast                  # play a recording (asciicast v2)
cargo run -p tern-app -- --edit prod-db-01                    # open the host editor (a new host without a name)
cargo run -p tern-app -- --settings sync                      # open Settings at a page
cargo run -p tern-app -- --show snippets                      # open the vault at a screen (or `vaults`, the list of vaults)
cargo run -p tern-app -- --palette prod                       # open the command palette
```

On Linux, GPUI needs these libraries (Debian and Ubuntu names):

```sh
sudo apt-get install libx11-dev libx11-xcb-dev libxcb1-dev libxcb-randr0-dev libxcb-xfixes0-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libfontconfig-dev libfreetype-dev libvulkan1
```

Tern keeps its files in `%APPDATA%\Tern`, `~/Library/Application
Support/Tern` or `~/.local/share/tern`. `TERN_DATA_DIR` points it elsewhere,
which also keeps a test run away from your real vault; on Linux and macOS,
where Tern writes keys to `~/.ssh` and trusted hosts to
`~/.ssh/known_hosts`, `TERN_SSH_DIR` and `TERN_SYSTEM_KNOWN_HOSTS` move those
too. `TERN_THEME=light|dark` forces an appearance.

`tern --mcp` is the MCP server an agent starts: it speaks MCP on its
standard input and output and hands each tool call to the Tern running with
the same data folder (starting one if none is), once Settings has the MCP
server on. Point an agent at a build from source the same way:

```sh
claude mcp add tern-dev -- C:\path\to\tern\target\debug\tern.exe --mcp
```

On Windows Tern is a windowed program, with no console of its own, and
carries Tern's icon and version (`crates/tern-app/build.rs`;
`resources/tern.ico` is `docs/images/mark.svg` at each icon size). Started
from a terminal it still prints there, as `--bench` needs.

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
cargo run -p tern-app --release -- --bench-start                 # time from launch to the first frame, with the data folder's vault
```

## SSH lab

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

## Release

`.github/workflows/release.yml` builds and publishes a release when a
version tag is pushed. Set `version` in the root `Cargo.toml`, commit, then
tag that commit and push the tag:

```sh
git tag v0.2.0
git push origin v0.2.0
```

The workflow checks the tag matches the version, builds Tern for Windows and
publishes the release with `Tern-windows-x64.exe`, the file the README's
download link always fetches from the latest release.
