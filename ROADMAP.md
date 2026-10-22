# Roadmap

Everything the mockups and the product plan promise that Tern doesn't do yet,
in the order it should be built. Foundations come first because later items
depend on them. Tick an item in the same commit that ships it, and keep the
rules in [AGENTS.md](AGENTS.md) (monochrome UI, no helper text, logic in
tern-core with tests, a UI test for interactive behaviour).

The mockups (light and dark, every screen) live in the Claude artifact
<https://claude.ai/artifact/9g9cbb9TUigbcUePoYaxzL> (private to the owner's
claude.ai account). Each item below names the artboard it comes from.

## Built

- [x] Vault of host cards: search, quick connect from a typed `ssh` command,
      host-key art, latency traces, inverted selection
- [x] SSH sessions in tabs: host-key trust (Tern's own known_hosts; OpenSSH's
      is read only), key / agent (OpenSSH agent, Pageant) / password sign-in
- [x] Host editor: add, edit, delete, groups, tags, test connection
- [x] Jump hosts (one hop, `ssh -J` style), keep-alive, command after login
- [x] Import from PuTTY (registry on Windows, `~/.putty/sessions` elsewhere)
- [x] Light and dark themes; `tern --connect`, `tern --edit`, `--bench`
- [x] `tern-devserver`: local SSH server with a real shell that also works as
      a jump host towards this computer
- [x] Saved vault (item 1): `vault.json` in the data folder, versioned, no
      secrets, written through a temp file; an unreadable file is moved
      aside, a newer one left alone; empty on a fresh install
- [x] Terminal select and copy (item 2): drag, double-click for a word,
      triple-click for a line, Shift+click extends; painted ink on paper and
      kept in place while output scrolls; Ctrl+Shift+C / Cmd+C copies,
      Ctrl+Shift+A / Cmd+A selects everything
- [x] Real reachability (item 3): saved hosts are checked in the background
      (TCP plus SSH greeting) every 30 s while the vault shows in the
      active window, four at a time, a host behind a jump host at the jump
      host; cards draw the last 40 measured samples
- [x] Known hosts screen (item 4): every trusted key with its art, hosts,
      type and fingerprint, searchable; hashed OpenSSH names show as hashed;
      Tern's keys can be forgotten, OpenSSH's are read only
- [x] Keychain screen (item 5): key cards from `~/.ssh` (read only) and
      Tern's own folder, weak and hardware keys marked; details with the
      public key and Copy, fingerprint and the hosts that use it; Generate
      (Ed25519, optional passphrase), Import, Export, Delete (Tern's keys,
      two clicks) and Install on a host (`ssh-copy-id` in a session)
- [x] Snippets (item 6): saved commands in the vault file (version 2), a
      screen to add, edit and delete them, and a bar under each session
      whose chips type a command and Enter, with a dashed New snippet

## Known gaps in what's built

- The header's "Synced" is static text (item 14).
- Only tested on Windows so far (item 23).

## Next, in order

### 5b. Keychain, later
- Identities (a user plus a key, reused by hosts), with a Keys / Identities
  switch in the Keychain toolbar.
- The "Include in encrypted sync" switch in a key's details comes with sync
  (item 14).

### 7. History (sidebar "History")
- Recent connections: host, when, how long, how it ended. Reconnect in one
  click.
- Stored in the data folder, capped (for example the last 200).

### 8. Port forwarding (sidebar "Port forwarding"; Main artboard panel)
- Per-host rules: local (`-L`), remote (`-R`), dynamic SOCKS (`-D`).
- Switch them on and off while connected. The vault's side panel shows the
  selected host's rules as rows with ink switches (mockup).
- Core: `-L` is a local listener plus `channel_open_direct_tcpip`; `-R` is
  `tcpip_forward`. End-to-end tests against the in-process server.

### 9. Split panes and broadcast (Terminal artboard)
- Layouts: single, side by side, main plus stack, grid of four.
- Focused pane: 2 px ink ring. Each pane header shows the host plus Maximize
  and Close.
- Broadcast button (ink while on) types into every pane; then every pane gets
  the ring. Snippet chips go to every pane too while broadcasting.
- The toolbar shows the focused host with its latency trace.

### 10. SFTP panel (Terminal artboard)
- Uses the `russh-sftp` crate. A Remote / This computer switch; a path bar
  with upload and refresh; a list with name and size (or item count).
- Transfers with an ink progress bar and a check when done. Drag and drop to
  upload.

### 11. Vault side panel (Main artboard right column)
- For the selected host: a live preview of its open session (if any), Open
  terminal and SFTP buttons, and its port-forward switches (item 8).

### 12. Settings (Sync artboard tabs: General, Terminal, Appearance, Sync)
- Appearance: light, dark or system.
- Terminal: font family and size, cursor style, scrollback length, bell,
  copy on select.
- `settings.json` in the data folder.
- The header's sliders button opens Settings (mockup).

### 13. Vault list view and filters (Main artboard toolbar)
- A grid / list switch.
- Filter by group, tag and status, with the active filter count as a value on
  the Filter button.
- Keyboard: arrows move the selection, Enter connects, E edits.

### 14. Sync through your own storage (Sync artboard)
- Providers: Google Drive, Dropbox, OneDrive, iCloud Drive, WebDAV /
  Nextcloud, Any folder. Start with "Any folder": a desktop sync client then
  covers most providers with no APIs or accounts. Add WebDAV directly next.
- The vault is encrypted on the device with a passphrase before it leaves.
  For example, Argon2id to derive a key, then XChaCha20-Poly1305.
- "What syncs" switches: hosts and groups, snippets, port forwards, private
  keys (off by default, per key in Keychain), themes and settings.
- Shows the devices that sync, "Synced 2 min ago", and Sync now. The header's
  "Synced" check reflects the real state.
- Conflicts: last writer wins per item, using per-item timestamps.
- No Tern servers and no accounts, ever.

### 15. Agent forwarding (host editor option, left out for now)
- A switch in the host editor's options card.
- Core: request agent forwarding on the session channel, then answer
  server-opened `auth-agent@openssh.com` channels by piping them to the local
  agent (`SSH_AUTH_SOCK`, the OpenSSH agent pipe, or Pageant).
- An end-to-end test.

### 16. Save passwords (host editor, Password sign-in)
- A password field plus a "Save in the system keychain" switch (mockup).
- Uses the `keyring` crate. Passwords are never written to `vault.json`.

### 17. Terminal niceties
- Find in scrollback, clickable URLs, font zoom (Ctrl +/-), visual bell.
- Mouse reporting for programs that want it (vim, htop).

### 18. More SSH
- Jump chains (more than one hop; the editor refuses them today).
- OpenSSH host and user certificates (refused today with "host certificates
  aren't supported yet").
- Reconnect automatically after a dropped connection.

### 19. macOS and Linux
- Built and tested on Windows only so far. Check:
  - Cmd shortcuts (copy, paste, select all);
  - the agent via `SSH_AUTH_SOCK`;
  - PuTTY's `~/.putty/sessions`;
  - fonts (`theme::MONO`) and the data folder paths.

### 20. Installers and releases
- Windows: MSI or winget. macOS: a signed, notarised universal `.dmg`.
  Linux: AppImage, Flatpak or `.deb`.
- Published as GitHub Releases. Any update check stays serverless (the GitHub
  releases API).

### 21. Licence and going public
- Choose a licence (MIT, Apache-2.0 or GPL-3.0), add `LICENSE`, then make the
  repository public.

## Not planned

- Telnet, serial and raw connections (the PuTTY import skips them).
- Tern accounts or any Tern-hosted service.
