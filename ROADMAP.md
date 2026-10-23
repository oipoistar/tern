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
- [x] Import from OpenSSH: hosts in ~/.ssh/config the vault lacks are
      offered beside PuTTY's; each alias gets what ssh would use (first
      value wins, Host * included, Include followed, Match skipped):
      HostName, User, Port, IdentityFile, ProxyJump chains and the older
      ProxyCommand ssh -W, nc proxies, Local/Remote/DynamicForward,
      SetEnv, ForwardAgent and ServerAliveInterval
- [x] Keyboard-interactive sign-in: after keys and the password, the
      server's own questions; a password question is answered from the
      password Tern has, anything else (a one-time code) is asked in the
      pane one question at a time, pasting allowed; jump hosts are asked
      too, and may now sign in with a password (their saved one, or
      asked)
- [x] Local terminal tabs: this computer's shell (PowerShell 7 or
      Windows PowerShell, $SHELL elsewhere) on a pseudo-terminal, from
      the button at the end of the tab strip, Ctrl+Shift+T (Cmd+T) or
      tern --local; panes split as local shells too, there's no Files or
      History for them, and Restart brings an exited shell back.
      Ctrl+Tab and Ctrl+Shift+Tab go round the tabs, the vault included
- [x] File management in the Files panel: New folder in the path bar;
      hovering a row offers rename, permissions on the host (octal or
      rwx) and delete (folders with what they hold), each taken or
      confirmed in one line under the path bar, on the host and on this
      computer; nothing is ever replaced
- [x] Terminal colour schemes: Settings, Terminal, Colours shows each
      scheme as a swatch of itself (Tern's own, which follows the app's
      look, then Solarized Dark and Light, Dracula, Nord, Gruvbox Dark,
      One Dark, Tokyo Night, Monokai and GitHub Light); terminals, the
      vault's previews and key art repaint at once, and the choice syncs
      with the other settings
- [x] Run a snippet on several hosts: a play button on each snippet
      opens a list of hosts to pick; Run opens them side by side, four
      panes to a tab, each typing the snippet once its shell starts
      (after the host's own command). tern --show <screen> opens the
      vault at a screen
- [x] Port forwarding without a terminal: a play button beside each rule
      runs it now in a session without a shell (one per host, shared by
      its rules, closed with the last); stop ends it. Such a session
      can't ask for anything, so an untrusted key, a refused sign-in or
      a server's question shows as the rule's one-line reason.
      Connection errors now read as a few words everywhere
- [x] Identities (item 5b): a Keys / Identities switch on the Keychain
      screen; an identity is a name, a username and a sign-in (agent,
      key file or password, the password in the system keychain), and a
      host using one (an Identity picker in the editor) signs in as it,
      jump hosts too; editing an identity changes every host that uses
      it, it can't be deleted while one does, and identities sync with
      hosts. Vault file version 8
- [x] Open all: a group's heading opens the hosts shown in it side by
      side, four panes to a tab, ready for Broadcast
- [x] Copy for OpenSSH: the side panel copies the selected host as an
      ssh command line (port, key, -J chain, -A, proxy, rules switched
      on; a chain whose first hop has a proxy becomes nested
      ProxyCommands) or as ~/.ssh/config blocks, one per host on the way,
      that the importer reads back as the same hosts
- [x] Host notes: a Notes field in the host editor, shown under the
      selected host in the side panel and matched by search. Vault file
      version 9
- [x] Wake-on-LAN: a host can keep its network card's MAC address (typed
      any common way); the side panel's Wake broadcasts the magic packet
      on the local network (ports 9 and 7) and says Sent. Vault file
      version 10
- [x] Paste protection: pasting several lines into a shell that would
      run each as it arrives (no bracketed paste) shows them first, with
      Paste (Enter) and Cancel (Escape); Settings, Terminal, Confirm
      pastes turns it off
- [x] Tab markers: a tab not on screen shows a dot once its sessions
      print something new, and a bell if one rang; looking at the tab
      clears them
- [x] Snippet variables: {{name}} in a snippet's command asks for a
      value before it's typed, in a row above the snippet bar (Enter
      moves on, the last Enter types it, Escape doesn't) and in the Run
      on hosts chooser
- [x] Terminal menu: right-click (Shift+right-click in programs that
      take the mouse) offers Copy, Paste, Select all, Find, Clear
      scrollback and Save output, which writes the scrollback and screen
      to a text file
- [x] Command palette: Ctrl+Shift+P (Cmd+Shift+P on macOS), or the
      search button in the header, finds saved hosts, open tabs,
      snippets and Tern's own screens and actions by a few of their
      letters; Up and Down pick, Enter runs, Escape gives the keyboard
      back
- [x] Reopen tabs: a plain start opens the tabs open when Tern last
      quit, panes and the tab on screen included, from tabs.json in the
      data folder (saved hosts by id, typed connections by what was
      typed, no passwords; hosts deleted since are left out); Settings,
      General, Reopen tabs turns it off
- [x] Sync across versions: hosts, snippets and identities, with their
      proxies and rules, and terminal settings keep fields this Tern
      doesn't know, in the vault, settings and sync files, and edits
      keep them too; sync files carry a format number, and a device that
      meets a newer one stops syncing, with a one-line reason, until
      it's updated
- [x] Encrypted key files: a key file that needs a passphrase is offered
      to the server first, and its passphrase is asked in the pane
      (three tries) only once the server says it would take the key, as
      OpenSSH does; a jump host's asks the same way
- [x] Guarded hosts: Guard against stray input in the host editor; the
      host's panes get a double hairline inside the ring, Broadcast
      leaves them out unless a pane's header lets it in (for as long as
      Broadcast lasts), and pastes (even one line), snippets and
      commands from Run on hosts wait in the pane for Paste or Run
      (Enter) and Cancel (Escape). tern --connect also takes a saved
      host's name
- [x] Kept shells: Keep shells running in the host editor runs the
      host's shells in tmux (screen without it, a plain shell without
      either) under a name for each pane slot, so a dropped connection
      or Tern quitting doesn't end them; the pane reconnects to the same
      shell without typing its login command again, a later pane to the
      host finds a shell left running, and closing a pane by hand ends
      its shell on the host
- [x] Edit host files here: a host's file in the Files panel has an
      edit button that copies it into a folder of its own and opens it
      in this computer's app for it; each save is written back over the
      host's file (its permissions kept, shown with the transfers), and
      if the host's file changed meanwhile, one line asks first
      (Replace or Cancel). The button stays inked while the file is
      being edited
- [x] Ports the host opens: a live session to a Linux host reads its
      /proc over SFTP every few seconds (nothing runs in the shell), and
      the toolbar offers each port that started listening since the
      pane's first session as an outlined :port; a click forwards it to
      the same port here (inked; then a click opens it in the browser,
      and its stop ends forwarding), a failure draws the danger outline,
      and a port that closes goes with its forward
- [x] Host vitals: the same reading of /proc draws the host's load
      (against its CPUs), memory and disk in use as small traces in the
      toolbar, each with its icon and latest value (shares rounded up,
      as df does); a vital the host doesn't report isn't shown
- [x] Containers on the host: once a session is live, its host's
      running containers are listed beside the terminal (Docker, else
      Podman); a host that has them gets a Containers button whose list
      (listed again as it opens) gives each by name and image, and picks
      open a pane beside it with a shell inside the container (bash, else
      sh), in a tab of its own when the tab is full
- [x] Groups that fill themselves: Group from a command (or a sourced
      group's pencil) takes a group's name and a command run here (cmd
      on Windows, sh elsewhere) that prints hosts in OpenSSH's config
      form (Tailscale, a cloud's CLI, an inventory, a script); Fill and
      the group's Refresh keep the group to what it prints, by name:
      hosts keep their ids and what was added to them here, new ones
      come in ("name (group)" when another group has the name), gone
      ones go unless another host goes through them, and output with no
      hosts changes nothing. The commands stay in sources.json in the
      data folder and never sync
- [x] Record and replay sessions: Record… in the terminal's menu asks
      where, then writes what the pane shows (never what's typed) as an
      asciicast v2 file, across reconnects too, until Stop recording or
      the ink record mark in the toolbar; Play a recording in the palette,
      a .cast file dropped on the window or tern --play opens it in a tab
      of its own, at its recorded size, under a bar with play or pause,
      an ink track to click and the time (Space plays or pauses, the
      arrows move five seconds). A file that can't be read says why there

## Known gaps in what's built

- Only tested on Windows so far (item 19); CI builds and tests on macOS and
  Linux too, but the app itself hasn't been run there.
- Proxies take no username or password: the vault keeps no secrets, and
  the credential store doesn't hold proxy logins yet.
- Forwarding rules run without a terminal can't ask for anything: they
  sign in with a key, the agent or a saved password, and a server that
  asks a question (a one-time code) fails them.
- A jump host that signs in with a password asks for it only through the
  server's own questions (keyboard-interactive); one that has those turned
  off needs the password saved.
- A device still on a Tern built before sync kept unknown fields (October
  2026) drops the host fields it doesn't know (identity, proxy,
  environment, notes, MAC address) and syncs the stripped hosts back, so
  every device needs updating once.

## Next, in order

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
