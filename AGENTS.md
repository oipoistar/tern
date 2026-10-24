# Working on Tern

Guidance for anyone changing this repo, people and coding agents alike. The
README is for people using Tern (its screenshots are in `docs/images`);
[DEVELOPMENT.md](DEVELOPMENT.md) is how to build and run it from source, the
dev server and the SSH lab. What to build next is in
[ROADMAP.md](ROADMAP.md), in order; tick an item in the commit that ships it.

## Checks before every commit

```sh
cargo fmt --all
cargo clippy --workspace --all-targets   # must be warning-free
cargo test --workspace                   # includes an end-to-end SSH test
```

Visual changes: run the app and look (`TERN_THEME=light|dark` to check both).
Performance-sensitive changes: compare `--bench 10` (and `--bench-start`
for launch) with and without them (see DEVELOPMENT.md).

## UI rules

- **No taglines, helper text or secondary labels.** No subtitles, hints under
  controls, counts beside headings, captions on self-evident values,
  explanatory paragraphs or keyboard-hint badges. If a screen needs words to
  make sense, change its layout (grouping, position, icons, a control that
  shows its state) instead. Keep only text the user must read or act on: a
  fingerprint, an error reason, a value, a control's name.
- **Errors and prompts are one line.** The exception is a changed host key,
  which names the `known_hosts` line because the user needs it to respond.
- **Monochrome chrome: ink on paper.** No colour-gradient fills, no pills with
  tinted backgrounds, no coloured glass. Solid ink (the theme's `text` as a
  fill) marks only what is selected or primary: the selected card renders
  with `Theme::inverse()`, and a group's primary button is ink. Everything
  else is plain, or outlined with a `line` hairline. Colour belongs to terminal
  content (ANSI) and to the `danger` error border, nothing else.
- **Draw state instead of labelling it.** A host card shows its key as
  drunken-bishop art (`marks::paint_art`), only for a key a known_hosts file
  already trusts, and blank otherwise. Reachability is a latency trace
  (`marks::paint_trace`), not an "Online" chip.
- Light and dark must both work; colours come from `theme.rs`, never inline.

## Architecture in one breath

`tern-core` has no UI: hosts and the `Vault` with its `vault.json`
(`host.rs`, `vault.rs`), the list of vaults and where each keeps its files
(`vaults.rs`), the bookkeeping of background reachability checks
(`reach.rs`), the host editor's checks (`draft.rs`), identities (`identity.rs`), environment variables (`env.rs`), when to reconnect a
dropped session (`reconnect.rs`), the tabs to reopen at the next start (`restore.rs`), local shells (`local.rs`, the same
events as an SSH session), SSH sessions (`ssh.rs`,
russh on its own Tokio runtime, talking to callers through channels; a
`Connection` resolves a saved host into what to dial, jump hosts included),
host-key art (`randomart.rs`), both known_hosts files (`known_hosts.rs`),
OpenSSH certificates (`certs.rs`), the MCP server for agents (`mcp.rs`:
the protocol and tools, `tern --mcp`'s relay, and the loopback port the
app takes calls on),
key pairs (`keys.rs`), snippets (`snippet.rs`), recent connections
(`history.rs`), the vault's filter (`filter.rs`), terminal colour schemes (`schemes.rs`), the command palette's fuzzy matching (`fuzzy.rs`), links in terminal
text (`links.rs`), proxies (`proxy.rs`), Wake-on-LAN (`wol.rs`), port forwarding
(`forward.rs`), agent forwarding (`agent.rs`), SFTP (`sftp.rs`), settings
(`settings.rs`), sync (`sync.rs`), the system's credential store
(`secrets.rs`), reading
PuTTY's saved sessions (`putty.rs`) and OpenSSH's config (`openssh.rs`) and paths and safe writes
(`paths.rs`). `tern-app` is GPUI: `workspace.rs` is the window
root (header, tabs, a session tab's panes, toolbar and snippet bar, live
layer, and the vault open, replaced when another opens; a window of tabs
moved out of the main one is a workspace with `main` set and no vault tab), `files.rs` is a
session tab's SFTP panel, `settings.rs` holds the settings in effect and
their dialog, `vaults.rs` the list of vaults and how the open one syncs,
`sync.rs` sync's state and runs, `mcp.rs` agents' tool calls acted on
in the main window,
`vault.rs` is the
vault tab (sidebar and the hosts screen), `keychain.rs`, `snippets.rs`,
`forwarding.rs`, `known_hosts.rs` and `history.rs` are its other screens, `terminal.rs` is a
session (with `term_input.rs` encoding its keys and mouse for the remote
program), `host_editor.rs` is the dialog the vault opens over itself, and
`marks.rs` draws the art and traces.
`tern-devserver` is a local SSH server with a real shell and SFTP for
manual testing; its SFTP server (`sftp.rs`, a library module) also serves
the end-to-end tests.

Rules live in tern-core with unit tests (what a draft needs, what a saved
host connects through, which credentials a sign-in method tries); views
only gather input and show results. `tern --edit [host]` opens the editor
at startup, for looking at it without clicking.

## Gotchas that cost time before

- **Anything that moves is painted by one layer on top**, not inside screens.
  Screens are cached views; GPUI re-renders every ancestor of a notified
  view, so animating inside them would redraw everything. Screens register
  placeholders (`ui.trace(...)`, built on `live::live`) into a `LiveSlots`,
  and `live_layer` paints them each frame. The frame ticker stops while no
  placeholder is registered (a terminal tab is showing). Static drawings
  like the art paint inline.
- **GPUI redraws a window whenever an entity its last frame read changes**,
  whether or not that entity is on screen. Drawing tabs from the panes
  themselves redrew the whole window for every bit of output in a hidden
  tab; the workspace draws them from a summary (`Badge`) it updates when a
  pane notifies, and redraws only when the summary changes.
- **Keep file work off the launch path.** Host key art reads every
  known_hosts file once for all hosts, in the background
  (`known_hosts::keys_for`); per host, a big hashed file cost seconds.
  `--bench-start` times launch to the first frame.
- **Prefer quads to paths for small repeated shapes.** GPUI draws rounded
  quads on the GPU, while paths are tessellated on the CPU. Dots are quads;
  only the trace's line is a path.
- **Typed text arrives through `EntityInputHandler`**, not key-down events.
  Key-down handles only keys that produce control sequences.
- **`use gpui_kit::*` shadows Rust's `#[test]`** with GPUI's macro. In test
  modules, import what you need explicitly.
- **UI tests click like a user**: tag elements with `.debug_selector(...)`,
  find them with `cx.debug_bounds(...)` and `simulate_click` the centre (see
  `vault.rs`). Focusing an input puts the cursor at the start, so select all
  (`ctrl-a`, `cmd-a` on macOS) before typing a replacement.
- **Jump hosts tunnel SSH inside an SSH channel.** A russh server's `data`
  hook sees forwarded channels too, so the test server and `tern-devserver`
  only act on their shell's channel; echoing or writing elsewhere corrupts
  the inner connection ("Bad packet size" decodes as the stray text).
- **russh uses the `ring` backend**; the default aws-lc needs CMake and NASM on
  Windows.
- **Windows ConPTY waits for a reply to its cursor-position query** before
  showing output; a real terminal (alacritty_terminal) answers it.
- **Keys and host keys go where each system expects them.** On Linux and
  macOS, trusted host keys go to `~/.ssh/known_hosts`
  (`paths::trusted_known_hosts`) and keys Tern makes, imports or receives to
  `~/.ssh` (`keys::home`); Tern notes those keys in its own folder and
  deletes no others. On Windows both go to Tern's data folder and `~/.ssh`
  is only read. `TERN_DATA_DIR`, `TERN_SYSTEM_KNOWN_HOSTS` and `TERN_SSH_DIR`
  redirect all of it, so a check never shows or touches your own files: set
  all three before anything trusts or makes a key.
- **Synced types keep what they don't know.** `Host`, `Snippet`,
  `Identity`, `Proxy`, `Forward` and `TerminalSettings` flatten JSON fields
  they don't know into `unknown`, so an older Tern carries a newer one's
  fields through its vault, settings and sync files. Code that rebuilds one
  from a form (a draft's or a snippet's check) copies the original's
  `unknown` across. A change an older Tern would get wrong anyway (a new enum
  variant, a field inside another type, a new meaning) raises
  `sync::FORMAT`: a device that reads a newer format stops syncing, with a
  one-line reason, until it's updated.
- **`tern --mcp` owns standard output.** It speaks MCP there, so nothing
  else may print to it; it opens no window and never attaches a console.
  It reaches the app through the port and token in `mcp.json` in the data
  folder, so `TERN_DATA_DIR` picks which Tern an agent drives: point a
  check at a scratch folder with `"mcp": true` in its `settings.json`.
- **Agents read the terminal's grid, not its output.** Windows' console and
  shells that redraw lines move the cursor instead of printing newlines,
  so only the rendered text reads right. Where an agent's last read
  stopped is a mark in a cell's underline colour (as shell integration's
  command marks are), which moves with the text.
- **OS CPU-time counters are useless for frame costs** (Windows counts in
  15.6 ms ticks); `--bench` times frames with the high-resolution clock.
- **Open windows with `gpui_kit::open_window`**, not GPUI's own: it wraps
  the view in GPUI Kit's `Root`, which inputs and popovers rely on. A view
  made in one window keeps what it subscribed to there (`subscribe_in`, a
  find bar's field), so a pane moving windows drops those first.
- **Tern has no console of its own on Windows** (`windows_subsystem`, in
  every build but tests), so starting it opens no console window. Started
  from a terminal it attaches to that one (`attach_console` in `main.rs`),
  so `--bench` and `cargo run` still print; from Explorer, what it prints
  goes nowhere.
