# Working on Tern

Guidance for anyone changing this repo, people and coding agents alike. The
README covers what Tern is and how to run it.

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
- **No colour-gradient fills and no pills with tinted backgrounds.** Accents
  and statuses use voxel glass (`glass.rs`); neutral states use plain or clear
  surfaces. Status colour lives in a small glyph (dot, check, ring).
- Light and dark must both work; colours come from `theme.rs`, never inline.

## Architecture in one breath

`tern-core` has no UI: hosts, SSH sessions (`ssh.rs`, russh on its own Tokio
runtime, talking to callers through channels) and paths. `tern-app` is GPUI:
`workspace.rs` is the window root (header, tabs, glass layer), `vault.rs` and
`terminal.rs` are the screens. `tern-devserver` is a local SSH server with a
real shell for manual testing.

## Gotchas that cost time before

- **Glass is painted by one layer on top**, not inside screens. Screens are
  cached views; GPUI re-renders every ancestor of a notified view, so
  animating inside them would redraw everything. Screens register glass
  placeholders (`ui.glass(...)`) into a `GlassSlots`; each independently
  rendering part (header, screen) owns its own `GlassSlots` so one clearing
  never wipes another.
- **GPUI only clips to rectangles.** Rounded glass is masked by painting the
  underlay colour around it; pass the right `underlay`.
- **Typed text arrives through `EntityInputHandler`**, not key-down events.
  Key-down handles only keys that produce control sequences.
- **`use gpui_kit::*` shadows Rust's `#[test]`** with GPUI's macro. In test
  modules, import what you need explicitly.
- **russh uses the `ring` backend**; the default aws-lc needs CMake and NASM on
  Windows.
- **Windows ConPTY waits for a reply to its cursor-position query** before
  showing output; a real terminal (alacritty_terminal) answers it.
- **Host keys:** `~/.ssh/known_hosts` is read only. Trusted keys go to Tern's
  own file in its data folder. `TERN_DATA_DIR` and `TERN_SYSTEM_KNOWN_HOSTS`
  redirect both for tests.
- **OS CPU-time counters are useless for frame costs** (Windows counts in
  15.6 ms ticks); `--bench` times frames with the high-resolution clock.
