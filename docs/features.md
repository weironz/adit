# Feature reference

What Adit actually does today, verified against the source. For the shape of the code
see [architecture.md](architecture.md); for the reasoning, [decisions.md](decisions.md).
Planned-but-unbuilt work lives in [feature-roadmap.md](feature-roadmap.md) and
[phase2-plan.md](phase2-plan.md) — this file is only what exists.

Known gaps are listed honestly at the [end](#known-gaps).

## Protocols

| Protocol | Transport | Notes |
|---|---|---|
| **SSH** | `russh` | The main path: PTY shell, SFTP, tunnels, jump hosts |
| **Local shell** | `portable-pty` (ConPTY on Windows) | Any program; defaults to the system shell |
| **Serial** | `serialport` | Port in the host field, baud in the identity field |
| **RDP** | IronRDP, out-of-process helper | Graphical surface, not a VT terminal |

All three terminal protocols share one event protocol, so the session layer treats them
uniformly. RDP is the exception and carries a framebuffer instead.

## Sessions

- **Profiles** with group/folder organisation, manual ordering, and drag-reorder.
  Sessions and folders share one ordering scale, so a session can sit above, between, or
  inside folders.
- **Tabs** — click to activate, drag to reorder live, right-click for rename / disconnect
  / reconnect / clone / close. A status dot and an optional environment badge (e.g.
  `PROD`, tinted by the profile's accent) ride on each tab.
- **Auto-titling**: repeated sessions to one profile become `name`, `name (1)`, `name (2)`.
  Hand-renamed tabs are never renumbered, and closing one re-tidies the rest.
- **Auto-reconnect** with exponential backoff (1→30 s, 10 attempts). Arms only if the
  session actually connected once and the user didn't disconnect deliberately, so a bad
  password or an intentional `exit` never loops.
- **Enter reconnects** a dropped session; SSH reconnects in place with stored
  credentials, other protocols reopen the dialog.
- **Split panes / tiling** — up to 6 sessions tiled as columns, rows, or a grid. Only the
  focused pane shows a cursor and drives scrolling. RDP cannot be split.
- **Broadcast input** fans keystrokes to every connected session, with a persistent
  warning in the status bar showing the reach count.

## Authentication

Methods are tried in a strict order, short-circuiting on success:

1. **Password**, then **keyboard-interactive** if the password is rejected (capped at 16 rounds)
2. **Explicit identity file**
3. **SSH agent** — on Windows the OpenSSH named pipe, then Pageant; on Unix `SSH_AUTH_SOCK`
4. **Default keys** — `~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa`, `id_dsa`

A separate key **passphrase** field is preferred, falling back to the login password so
older profiles keep working. Key loading uses `russh`/`ssh-key`, which covers OpenSSH
natively and routes PuTTY `.ppk` to its PPK parser.

**Interactive MFA** is supported: fields the client can safely answer (non-echoed
password/passphrase prompts) are auto-filled, while anything that looks like a second
factor — OTP, verification code, token, authenticator, 2FA, Duo, PIN, YubiKey, SecurID —
or a password *change* prompt is surfaced to the user as a dialog. Dismissing it cancels
the connect rather than silently falling through to other methods.

**Host keys** are verified against `known_hosts` — including **hashed** (`|1|…`) entries
and wildcard/negated host patterns — and classified as trusted / unknown / changed. An unknown key can be auto-accepted (default) or prompted; a **changed** key
always prompts, shows the previous fingerprint, and warns about MITM. Known hosts can be
listed and removed from the UI.

## Terminal

A `vte`-driven parser over an Adit-owned grid.

- **SGR**: bold, dim, italic, underline, strikethrough, reverse, hidden; 16 ANSI,
  256-colour, and 24-bit truecolor (both `;` and `:` sub-parameter forms).
- **Screen control**: scroll regions (DECSTBM), alternate screen (47/1047/1049), insert
  and delete lines/characters, all erase modes, save/restore cursor.
- **Scrollback** with a configurable limit (default 5000 lines), correctly *not* written
  while the alternate screen is active.
- **Wide characters**: CJK/full-width glyphs occupy two columns and cannot straddle the
  right margin.
- **OSC 8 hyperlinks** — only `http(s)` render as links, the click target is armed only
  while Ctrl/Cmd is held (so a plain click still selects), and clicking asks for
  confirmation showing the real URL before opening a browser.
- **Mouse reporting** (1000/1002/1003, SGR and legacy X10 encodings) so `vim`, `tmux`,
  and `htop` receive mouse events instead of the terminal selecting text.
- **Bracketed paste**, with a multi-line paste confirmation that is skipped when the
  remote app is already in bracketed-paste mode.
- **Device reports**: DSR cursor position and DA (identifies as VT102-class); replies are
  written straight back to the PTY.

**Selection** is anchored in absolute scrollback rows: single-click drag, double-click
word, triple-click line. It survives scrolling, and dragging past the pane edge
auto-scrolls while extending. Copy-on-select and right-click-paste are both optional.

**Scrollback search** (Ctrl+Shift+F) highlights all matches, steps with wraparound,
auto-scrolls to the current hit, and shows an `n/total` counter.

**Appearance**: 6 monospace font presets, font size 9–28 px (Ctrl+wheel zooms), and 7
colour schemes (Default, Dracula, One Dark, Nord, Gruvbox Dark, Solarized Dark/Light).
A reverse-video block cursor blinks at 530 ms on the focused pane and holds solid while
typing.

## SFTP

Two independent surfaces, both on their own SFTP connection:

**Dual-pane file manager** (SecureFX-style) — local and remote panes with path bars,
sortable columns, multi-select, double-click transfer, pane-to-pane drag with a drag
ghost, right-click menus (download/upload, rename, delete, open), mkdir, inline rename,
delete confirmation, and a transfer queue with progress, speed, and history. Dropping OS
files onto the window uploads them to the remote cwd. **Directory trees transfer
recursively** in both directions.

**Command-line `sftp>` tab** (Alt+P, SecureCRT-style) — `ls`/`dir`, `cd`, `pwd`, `lls`,
`lcd`, `lpwd`, `get`, `put`, `mkdir`, `rmdir`, `rm`/`del`, `rename`/`mv`, `clear`,
`help`, `exit`. Paths accept `~` and `..`; quoted paths keep spaces. Dropping a file onto
the tab uploads it to the current remote directory. Line editing supports Backspace,
Ctrl+C, and Ctrl+U. Because SFTP has no remote echo and no `chdir`, the shell owns its
own echo and prompt, and `cd` is implemented as a listing whose success commits the
directory — so an unreadable directory reports an error instead of silently "working".

## Port forwarding

All three OpenSSH forms: **local** (`-L`), **remote** (`-R`), and **dynamic** SOCKS5
(`-D`, with a real handshake covering IPv4, domain, and IPv6 address types). Tunnels can
be created ad-hoc or saved to a profile and started automatically after connect. A live
list shows listening state, active/total connection counts, and errors.

## Jump hosts (ProxyJump)

An ordered chain of `user@host:port` hops, parsed OpenSSH-style with genuine IPv6
handling (bracketed `[::1]:22` and bare literals). Each hop is dialled through the
previous one. Hops authenticate with the profile's own credentials — there is no per-hop
password. An unknown hop key is recorded TOFU; a **changed** hop key is rejected
outright, and only the final target gets the interactive prompt.

## RDP

Connect with NLA/CredSSP, keyboard (PC/AT set-1 scancodes including the extended set),
mouse with correct un-letterboxing back to remote pixels, wheel, and **dynamic resize** —
the remote desktop is resized to match the viewport so it renders ~1:1 instead of being
upscaled.

**GNOME Remote Desktop system mode ("Remote Login") works end to end**, which required
implementing three things IronRDP doesn't provide: EGFX graphics compositing including
**RemoteFX Progressive decoding**, **Server Redirection**, and **RDSTLS** handover auth.
See [rdp-gnome-remote-desktop.md](rdp-gnome-remote-desktop.md).

The framebuffer is sampled per vsync frame, but only while an RDP tab is *active*, so a
background RDP session doesn't pin the app at 60 fps.

## Credentials & security

Passwords and key passphrases are stored **encrypted** in `credentials.json` in the
config directory — XChaCha20-Poly1305, Argon2id-derived key, fresh nonce per write,
atomic writes, `0600` on Unix. Secrets from the older OS-keyring store are imported once
on startup. Saving is on by default; a rejected password re-prompts automatically.

> **The security model is obfuscation, not secrecy.** The key is compiled into the
> binary and there is no master password — a deliberate trade-off so credentials sync
> between machines with zero setup. Anyone holding the file *and* the open-source key
> can recover every password. See
> [decisions.md](decisions.md#8-credentials-encrypted-in-the-config-directory-not-the-os-keyring--reversal).

The RDP password reaches the helper over **stdin, never argv or env** (argv is visible
in the process list). Passwords are never written to `profiles.json`.

## Logging

Per-session transcripts with a configurable directory and filename pattern
(`%N %H %Y %M %D %h %m %s`, with a live preview), optional auto-start on connect, and an
optional plaintext mode that strips escape sequences for a human-readable log.

## Import & configuration

- **`~/.ssh/config`** import — `Host`/`HostName`/`User`/`Port`/`IdentityFile`, multiple
  aliases per block, `~` expansion. Wildcard and `Match` blocks are skipped.
- **SecureCRT session tree** import — walks the `Sessions` folder, preserves the folder
  structure as groups, and decodes SecureCRT's hex-DWORD port fields.
- **Relocatable config directory** — point it at a synced folder (Dropbox) and profiles,
  settings, *and* credentials travel with it. Takes effect on next launch;
  `ADIT_CONFIG_DIR` overrides everything.
- **In-app updater** — checks GitHub releases, compares semver, downloads and silently
  launches the installer. Optional check on startup.

## Keyboard shortcuts

| Keys | Action |
|---|---|
| `Alt+R` | Jump to the toolbar's host box |
| `Alt+I` | Jump to the sidebar filter (reveals the sidebar if hidden) |
| `Alt+P` | Open a command-line SFTP tab for the active session |
| `Ctrl+Shift+F` | Scrollback search (`Esc` closes) |
| `Ctrl+Shift+C` | Copy selection (or the visible screen when nothing is selected) |
| `Ctrl+Shift+V` | Paste |
| `Shift+PageUp/PageDown` | Scroll one page |
| `Ctrl+Shift+Home/End` | Jump to the top / bottom of the scrollback |
| `Ctrl+wheel` | Zoom the terminal font |
| `Enter` (on a dropped session) | Reconnect |
| `Esc` | Cancel an in-place rename |

---

## Known gaps

Verified shortcomings, so nobody has to rediscover them.

### Not implemented
- **RDP**: no H.264 decoder (hosts that negotiate AVC render black), the server cursor
  shape isn't drawn, and updates always ship the **whole** framebuffer rather than dirty
  rectangles. **Clipboard is not implemented** and has no feature flag — CLIPRDR's native
  backend wants a Windows message pump, which the windowless helper process doesn't have,
  so it needs design work rather than a flag. Audio (`sound`) is implemented but off by
  default because it pulls native Opus (needs CMake).
- **Terminal**: no reflow on resize (narrowing permanently truncates), no combining /
  zero-width character support, no DCS/Sixel, no charset designation, no custom tab
  stops. `TerminalChangeSet` dirty-row tracking is a stub that always reports the whole
  screen.
- **MFA is shell-only.** SFTP and tunnel connections answer non-interactively with the
  stored password, so an MFA-gated host will fail those.
- **Jump hosts reuse the target's single credential** — no per-hop authentication.
- **SFTP shell**: no tab completion, and no history recall (the history is recorded but
  unbound).
- stderr is merged into stdout on the shell path.
- SFTP transfer progress is correlated by **name**, so concurrent transfers of
  same-named files can mis-attribute progress.
- macOS is architecturally supported but unbuilt; Windows code signing is pending.

### Cosmetic / cleanup
- `adit-ui` is a single ~12.7k-line file; navigating it is the main friction in the repo.
