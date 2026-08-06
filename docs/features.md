# Feature reference

What Adit actually does today, verified against the source. For the shape of the code
see [architecture.md](architecture.md); for the reasoning, [decisions.md](decisions.md).
Planned-but-unbuilt work lives in [roadmap.md](roadmap.md) — this file is only what
exists.

Known gaps are listed honestly at the [end](#known-gaps).

## Protocols

| Protocol | Transport | Notes |
|---|---|---|
| **SSH** | `russh` | The main path: PTY shell, SFTP, tunnels, jump hosts |
| **Local shell** | `portable-pty` (ConPTY on Windows) | Any program; defaults to the system shell |
| **Serial** | `serialport` | Port in the host field, baud in the identity field |
| **Telnet** | raw TCP (RFC 854) | For switches, IPMI and console servers. Unencrypted, no client credential |
| **RDP** | IronRDP, out-of-process helper | Graphical surface, not a VT terminal |

All four terminal protocols share one event protocol, so the session layer treats them
uniformly. RDP is the exception and carries a framebuffer instead.

**Telnet** negotiates options reactively — it opens with silence and answers what the
device offers, which keeps it out of the states where two implementations can negotiate
at each other forever. It accepts the server's `ECHO` and `SUPPRESS-GO-AHEAD` (that pair
is what puts the link in character-at-a-time mode), performs `TERMINAL-TYPE` (answering
`xterm-256color`, the same `TERM` the SSH path requests) and `NAWS` (reporting the window
on agreement and on every later resize), and refuses everything else explicitly rather
than ignoring it — an unanswered option leaves some servers waiting forever. `IAC IAC`
is unescaped back to a literal `0xFF` in both directions, and a bare `CR` leaves as
`CR NUL` because RFC 854 has no bare `CR`. Until the server says it will echo, input is
echoed locally, so a device that never negotiates `ECHO` doesn't look dead while you
type. There is no credential plumbing at all: the login prompt is ordinary terminal
output, so the username and password are typed into the terminal and never touch the
credential store.

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

**Resizing loses nothing.** Narrowing re-wraps every logical line instead of clipping it,
shrinking the height moves the rows it pushes off the top into scrollback, and the
primary screen stashed behind an active alternate screen takes that same path — so
leaving `vim` after shrinking the window still finds the rows underneath. The alternate
screen itself deliberately never reflows: a full-screen app redraws at the new size, and
joining its rows would splice unrelated parts of a TUI together.

**Selection** is anchored in absolute scrollback rows: single-click drag, double-click
word, triple-click line. It survives scrolling, and dragging past the pane edge
auto-scrolls while extending. Copy-on-select and right-click-paste are both optional.
A re-wrap renumbers every absolute row, so a width change re-anchors the selection and
the scroll position to **logical lines** — the pre-wrap unit, which re-wrapping preserves
— and resolves them back at the new width; both stay on the text they were on (one edge
remains, see [Known gaps](#known-gaps)).

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
recursively** in both directions. **Transfers can be stopped** — a per-row *停止* button
cancels one, *全部停止* cancels every queued/running transfer at once. A running transfer
stops within one chunk of I/O (the byte loop polls an out-of-band cancel flag, so a stop
doesn't queue behind the transfer it's meant to interrupt); the partial file is removed
(the local file for a download, the remote file for an upload). Each transfer carries a
unique id, so progress is attributed to the exact queue row even when two transfers share
a filename.

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

### Clipboard (CLIPRDR)

**Text copies both ways**, on by default. Copy in the remote desktop and it lands on the
Windows clipboard; copy locally and it pastes into the remote. **选项 → RDP 会话共享剪贴板**
turns it off; it is the one setting that hands local data to a remote machine, so it has
to be refusable. The flag is negotiated during the handshake, so it applies to the next
connection — but switching it off also stops the local poll and drops whatever the poll
had already captured, so nothing stays queued for the next remote that asks.

The design is worth knowing, because the obvious one doesn't work here. IronRDP's
`cliprdr-native` backend owns the real system clipboard and therefore needs a window and
a `WM_CLIPBOARDUPDATE` message pump — which the windowless helper process does not have.
So responsibilities are split instead: **the helper speaks CLIPRDR on the RDP wire and
nothing else, and the GUI app owns the actual Windows clipboard.** The app is a real
windowed `iced` process, so it reads and writes the clipboard natively and ships text
across the existing helper IPC (`InputEvent::ClipboardText` in, `HostMsg::ClipboardText`
out). `crates/adit-rdp/src/clipboard.rs` is then a pure protocol adapter with no OS
dependency at all — and its whole state machine is unit-tested without a live host.

Consequences of that split:

- **Text only.** `CF_UNICODETEXT` is advertised; `CF_TEXT` / `CF_OEMTEXT` are accepted
  inbound. **Images and file copy-paste are out of scope** — files need a staging
  directory, chunked `FileContents` streaming and clipboard locking, none of which the
  helper IPC carries.
- **Local → remote is delay-rendered**, like mstsc: only the format list is advertised,
  and the text crosses the wire when something on the remote actually pastes.
- **Remote → local is eager**: the moment the remote advertises text we fetch it, because
  the app can't answer Windows' synchronous "give me the clipboard" across an async IPC
  hop. Transfers are capped at 8 MiB in either direction.
- The local clipboard is **polled every 500 ms**, and only while an RDP tab is in front —
  Windows exposes no clipboard-change signal to a process that isn't listening for one.
  So a local copy reaches the remote within about half a second, not instantly.

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

## Cloud sync

Sessions, groups and settings merged across machines, behind six providers: **GitHub
Gist**, **WebDAV**, **S3-compatible** storage, **Google Drive**, **OneDrive** and
**Dropbox**. Saved passwords travel too, opt-in, and only as the sealed blob — the
passphrase never leaves the machine.

The merge runs per session, keyed by UUID and measured against the last catalog the
provider confirmed, so a deletion made on one machine propagates instead of being
resurrected by a union. Design, provider quirks and the one-time OAuth registrations are
in [cloud-sync.md](cloud-sync.md), which is worth reading before touching any of it: two
of its rules exist because breaking them destroyed a real 152-session catalog.

## Settings

One page, reached from **File → 设置…**, with a category rail: 应用 / 外观 / 终端 /
日志 / 同步与云. It replaced three separate dialogs, which between them meant three
places to look for one setting.

**Language** — Chinese or English, switched under 应用 and applied without a restart.
Lookup is keyed by the Chinese source string, so a missing translation shows the
original rather than a key (see [Known gaps](#known-gaps)).

## Keyboard shortcuts

| Keys | Action |
|---|---|
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
- **RDP**: the server cursor shape isn't drawn. Frame updates do carry a dirty rect, but
  only **one**, grown to cover everything that changed — a blinking cursor in one corner
  and a clock in another expand it to most of the screen, so what wastes bandwidth is
  the coalescing rather than the absence of tracking. (H.264 *is* decoded — AVC420 and a
  from-scratch AVC444 — so a host that negotiates AVC no longer renders black.) The clipboard is **text only** — no images, no file copy-paste (see
  [Clipboard](#clipboard-cliprdr)). Audio (`sound`) is implemented but off by default
  because it pulls native Opus (needs CMake).
- **Terminal**: no combining / zero-width character support, no DCS/Sixel, no charset
  designation, no custom tab stops. `TerminalChangeSet` dirty-row tracking is a stub
  that always reports the whole screen.
- **A reflow keeps the selection and the scroll position**, with one edge left open:
  both are mapped through the logical lines (see [Terminal](#terminal)), and a logical
  line is identified by its index from the oldest line still held. Re-wrapping narrower
  turns one row into several, so a buffer already at the scrollback limit drops its
  oldest lines and renumbers everything that survives — an anchor taken before such a
  resize then resolves to the last row instead of its own text. Only bites a full
  scrollback (default 5000 lines) being narrowed.
- **MFA does not cover the dial fallback.** SFTP and tunnels open channels on the
  shell's existing connection, so the server authenticates once and the shell's prompt
  covers everything — jump hosts included, since each hop is offered the same prompt
  channel. When there is no live session to ride on they dial their own connection
  instead, and *that* path is still non-interactive: opening SFTP against an MFA host
  after its shell has already exited fails.
- **Telnet** implements only the four options a terminal needs (`ECHO`,
  `SUPPRESS-GO-AHEAD`, `TERMINAL-TYPE`, `NAWS`); `NEW-ENVIRON`, `TSPEED`, `BINARY`,
  `LINEMODE` and the rest are refused. There is no line-at-a-time mode — input goes out
  character by character whatever the server negotiated — no `AYT`/`BRK`/`IP` controls,
  and no encryption of any kind, including none for the password.
- **Jump hosts reuse the target's single credential** — no per-hop authentication.
- **SFTP shell**: no tab completion, and no history recall (the history is recorded but
  unbound).
- stderr is merged into stdout on the shell path.
- macOS ships since v0.1.62 — CI builds and publishes both Apple Silicon and Intel dmgs
  — but they are **unsigned**, as is the Windows installer; code signing is pending on
  both platforms. The RDP helper is Windows-only, so macOS builds are SSH/SFTP only.

### Cosmetic / cleanup
- `adit-ui` is ~17k lines. It is no longer one file — `update_loop`, `dialogs`,
  `session_ops`, `workspace`, `hosts`, `sidebar`, `profiles`, `sftp`, `style` and `i18n`
  are separate modules — but `lib.rs` and `update_loop.rs` are still ~2.3k lines each,
  because iced wants a single `Message` enum and a single `update`.
- **The English translation is partial.** Menus, the settings page, the dialogs and the
  runtime notices are covered (~330 strings); anything missing falls back to the Chinese
  original rather than showing a key, so a gap is invisible until you meet it. Adding a
  translation is one row in `i18n.rs` and touches no call site.
