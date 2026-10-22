# Working on Tern

Guidance for anyone changing this repo, people and coding agents alike. The
README covers what Tern is and how to run it. What to build next is in
[ROADMAP.md](ROADMAP.md), in order; tick an item in the commit that ships it.

## Checks before every commit

```sh
cargo fmt --all
cargo clippy --workspace --all-targets   # must be warning-free
cargo test --workspace                   # includes an end-to-end SSH test
```

Visual changes: run the app and look (`TERN_THEME=light|dark` to check both).
Performance-sensitive changes: compare `--bench 10` with and without them
(see README).

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
(`host.rs`, `vault.rs`), the bookkeeping of background reachability checks
(`reach.rs`), the host editor's checks (`draft.rs`), SSH sessions (`ssh.rs`,
russh on its own Tokio runtime, talking to callers through channels; a
`Connection` resolves a saved host into what to dial, jump hosts included),
host-key art (`randomart.rs`), both known_hosts files (`known_hosts.rs`),
OpenSSH certificates (`certs.rs`),
key pairs (`keys.rs`), snippets (`snippet.rs`), recent connections
(`history.rs`), the vault's filter (`filter.rs`), links in terminal
text (`links.rs`), port forwarding
(`forward.rs`), agent forwarding (`agent.rs`), SFTP (`sftp.rs`), settings
(`settings.rs`), sync (`sync.rs`), the system's credential store
(`secrets.rs`), reading
PuTTY's saved sessions (`putty.rs`) and paths and safe writes
(`paths.rs`). `tern-app` is GPUI: `workspace.rs` is the window
root (header, tabs, a session tab's panes, toolbar and snippet bar, live
layer), `files.rs` is a session tab's SFTP panel, `settings.rs` holds the
settings in effect and their dialog, `sync.rs` sync's state and runs,
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
- **Host keys:** `~/.ssh/known_hosts` is read only. Trusted keys go to Tern's
  own file in its data folder. `TERN_DATA_DIR` and `TERN_SYSTEM_KNOWN_HOSTS`
  redirect both for tests, and `TERN_SSH_DIR` the `~/.ssh` the Keychain
  lists, so a check never shows your own keys.
- **OS CPU-time counters are useless for frame costs** (Windows counts in
  15.6 ms ticks); `--bench` times frames with the high-resolution clock.
