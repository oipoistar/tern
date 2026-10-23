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
      is read only), key / agent (OpenSSH agent, Pageant) / password sign-in;
      OpenSSH certificates: host certificates vouched for by known_hosts'
      `@cert-authority` lines, user certificates beside a key file
      (`id_ed25519-cert.pub`) or held by the agent
- [x] Host editor: add, edit, delete, groups, tags, test connection
- [x] Jump hosts (`ssh -J` style, chains of any length: a jump host can
      go through another), keep-alive, command after login
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
- [x] History (item 7): every connection attempt that ends, with when, how
      long and how it ended, in `history.json` (last 200); Reconnect uses
      the saved host's current settings
- [x] Port forwarding (item 8): per-host `-L`, `-R` and `-D` (SOCKS5) rules
      in the vault file (version 3), started with a session when switched
      on and switched live while one is open; the screen shows a dot while
      a rule runs and the reason when it can't
- [x] Split panes and broadcast (item 9): one to four panes per tab (single,
      side by side, main plus stack, grid), each with its host, Maximize
      and Close; an ink ring on the focused pane, on all of them while
      Broadcast types (and runs snippets) into every pane; a toolbar with
      the focused host and its latency trace
- [x] SFTP panel (item 10): Files beside a session's panes, over
      `russh-sftp`: the host's folders or this computer's, a path bar with
      Up, Upload and Refresh, names with sizes or item counts; clicking a
      file copies it to the other side, dropped files upload, transfers
      show an ink progress bar and a check, and never replace a file
- [x] Vault side panel (item 11): for the selected host, a live preview of
      its open session (painted by the live layer, so the vault doesn't
      re-render), Open terminal (shows the open session if there is one),
      SFTP, and its port forwarding switches
- [x] Settings (item 12): `settings.json` in the data folder; the header's
      sliders open General (the data folder), Terminal (font, size, cursor,
      scrollback, visual bell, copy on select) and Appearance (system, light
      or dark, also set by the header's toggle)
- [x] Vault list view and filters (item 13): a cards / list switch; Filter
      by group, tag and status with the count of chosen values on the
      button; arrows move the selection, Enter connects, E edits
- [x] Sync through any folder (item 14): each device writes its own file,
      encrypted with a passphrase (Argon2id, XChaCha20-Poly1305), into a
      folder a sync client keeps in step; the newest change of each host or
      snippet wins, deletions included; switches for hosts and groups,
      snippets, port forwards and settings, private keys one by one; the
      devices and "Synced 2 min ago" in Settings, the real state in the
      header; the passphrase in the system's credential store
- [x] Save passwords (item 16): a password field and "Save in the system
      keychain" in the host editor; sessions try a saved password before
      asking; never in `vault.json`, forgotten with the host
- [x] Agent forwarding (item 15): "Forward the SSH agent" in the host
      editor; the server's agent channels are piped to the local agent
      (`SSH_AUTH_SOCK`, OpenSSH's pipe or Pageant) only when it's on
- [x] Terminal niceties (item 17): Ctrl+Shift+F / Cmd+F finds in the
      scrollback (plain text, case-sensitive only with capitals; the match
      selected, the others on screen lit faintly; Enter older, Shift+Enter
      newer); Ctrl+click / Cmd+click opens a link (underlined while the key
      is held); Ctrl+= / Ctrl+- / Ctrl+0 zoom the font; programs that ask
      get the mouse (clicks, drags, motion, wheel; SGR and UTF-8), Shift
      keeps it for selecting, and the wheel sends arrows to full-screen
      programs that don't take it; the visual bell came with Settings
- [x] More SSH (item 18): jump hosts that go through jump hosts, OpenSSH
      host and user certificates, and reconnecting by itself after a
      dropped connection (told apart from a shell that ended): a second,
      doubling to half a minute, eight tries; a refused sign-in or a key
      problem stops it for the user; the new shell starts on a clear
      screen with the old one in the scrollback
- [x] Environment variables per host: an Environment field in the host
      editor takes OpenSSH's SetEnv form (NAME=value, quotes around
      spaces); the shell is asked to start with them, and the server's
      AcceptEnv decides
- [x] Proxies per host: a Proxy switch and URL (socks5://host:port or
      http://host:port) in the host editor, drawn as a stop on the
      route; the first hop is dialled through it, reachability checks
      too, and the proxy's refusal is the error shown. Proxies that need
      a password aren't supported, since the vault file keeps no secrets

## Known gaps in what's built

- Only tested on Windows so far (item 23).

## Next, in order

### 5b. Keychain, later
- Identities (a user plus a key, reused by hosts), with a Keys / Identities
  switch in the Keychain toolbar.
- The "Include in encrypted sync" switch in a key's details comes with sync
  (item 14).

### 14b. More sync providers
- WebDAV / Nextcloud directly, without a desktop client: the same
  per-device files over HTTP. Then provider-specific choices in the Sync
  page (Google Drive, Dropbox, OneDrive, iCloud Drive) where a folder isn't
  enough.
- No Tern servers and no accounts, ever.

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
