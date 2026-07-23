# Architecture (as built)

How Adit is actually put together today. For *why* the shape is what it is, see
[decisions.md](decisions.md); for what it does, [features.md](features.md).

## Crate graph

```
adit-app        binary entrypoint — iced runtime, window/exe icon, /STACK link arg
 └─ adit-ui     the entire GUI: screens, terminal renderer, input, theme, dialogs
     └─ adit-session   session manager: lifecycle, event pump, SFTP/tunnel/log state
         ├─ adit-ssh       russh: auth, host keys, PTY shell, SFTP, tunnels, local/serial
         ├─ adit-storage   profiles, settings, encrypted credentials, imports
         ├─ adit-terminal  vte-driven VT grid → render-ready snapshots
         └─ adit-rdp-proto IPC wire types (serde + bincode)
adit-domain     shared ids, profile/auth models, enums  (used by everything)

adit-rdp        SEPARATE WORKSPACE → adit-rdp-host.exe   (IronRDP; talks IPC)
```

`adit-ui` is one large file (~12.7k lines) holding the iced `Message`/`update`/`view`
triple. `adit-session` and `adit-ssh` are likewise single large modules. This is
deliberate for `update`/`view` (iced's model wants one message enum) but is the main
source of navigation friction in the repo.

**`adit-rdp` is excluded from the root workspace** and has its own `Cargo.lock`, because
IronRDP's `picky` exact-pins pre-release RustCrypto crates that conflict irreconcilably
with `russh`'s. It is built with `--manifest-path crates/adit-rdp/Cargo.toml` and ships
as a helper *process*. See [decisions.md](decisions.md#4-rdp-lives-in-a-separate-workspace-and-a-separate-process).

## Threading model

| Thread / process | Owns |
|---|---|
| **UI thread** (iced) | All app state. Never blocks on disk or process spawn — both have caused "Not Responding" |
| **Per-session transport thread** | One per live SSH/local/serial session, running a tokio runtime and the shell loop |
| **SFTP / tunnel threads** | One per SFTP connection and per tunnel, same pattern |
| **`adit-profile-writer`** | Lazily spawned; serialises profile saves off the UI thread, coalescing bursts |
| **`adit-rdp-host.exe`** | Separate *process* per RDP session; two threads inside it own stdin/stdout |

Transports never touch UI state. They communicate only by channels, and the UI drains
them from one place: **`SessionManager::poll_events()`**, called on a 100 ms tick. That
single pump is what keeps the model single-threaded and race-free.

## Event protocols

Every transport speaks a command/event pair. The commands go *in* over a tokio
unbounded channel; the events come *back* over a `std::sync::mpsc`, drained with
non-blocking `try_recv`.

- **Shell** — `LiveShellCommand` (`Input`, `Resize`, `Disconnect`, `HostKeyDecision`,
  `AuthResponses`) / `LiveShellEvent` (`Status`, `Output`, `Error`, `AuthRejected`,
  `Closed`, `HostKeyPrompt`, `AuthPrompt`). Shared by SSH, local shell (ConPTY), and
  serial, so the session layer treats all three uniformly.
- **SFTP** — `SftpCommand` (`ListDir`, `Download`, `Upload`, `Mkdir`, `Rename`,
  `Remove`, `Disconnect`) / `SftpEvent` (`Status`, `Ready`, `Listing`, `Progress`,
  `Done`, `Error`, `Closed`).
- **RDP** — crosses a *process* boundary: `ClientMsg` (`Connect`, `Input`, `Close`) /
  `HostMsg` (`Connected`, `Tile`, `Resized`, `ClipboardText`, `Error`, `Closed`),
  framed as a 4-byte little-endian length prefix + bincode.

`AuthRejected` is split out from `Error` on purpose: it is the signal that drives the
UI's password re-prompt. Exactly five `SshError` variants map to it
(`AuthenticationRejected`, `EmptyPassword`, `NoAuthenticationMethod`,
`KeyPassphraseRequired`, `KeyPassphraseWrong`). `AuthenticationCancelled` is
deliberately excluded — re-opening a prompt the user just dismissed would trap them.

## Session model

A `SessionRecord` holds a `SessionSummary` (the public projection: id, profile, title,
endpoint, status) plus whichever transport it owns: `live` (shell), `rdp`, or
`sftp_shell`, and optionally a `log` and a `reconnect` state.

- **RDP sessions keep a placeholder `VtTerminal` that is never rendered** and leave
  `live`/`reconnect` as `None`, so every terminal-only code path skips them naturally.
- **Only SSH sessions carry `ReconnectState`.** RDP, local shell, serial and the SFTP
  shell therefore fall back to the connection dialog rather than reconnecting in place.
- Tab order is a separate `Vec<SessionId>` because the session store is a `HashMap`.

**Status machine** (driven entirely inside `poll_events`): status `"connected"` →
`Connected` and resets the reconnect counters; a status starting `"exit status"` →
`Disconnected` *and* marks the reconnect `manual` so a deliberate exit is never retried;
`Output` → `Connected`; `Error`/`AuthRejected` → `Error`; `Closed` → backoff or
disconnected.

**Auto-reconnect** backs off `1, 2, 4, 8, 16, 30, 30…` seconds, capped at 10 attempts.
It only arms when the session **actually reached "connected" at least once** and the user
didn't disconnect on purpose — so a wrong password or an unreachable host cannot loop.

## Data flow

**Connect.** UI collects a profile + password → `open_live_*_session` → the transport
thread connects, authenticates (prompting through `HostKeyPrompt`/`AuthPrompt` events if
needed), opens a PTY, and starts streaming `Output`.

**Remote → screen.** `Output` bytes → `VtTerminal::feed` (a `vte` parser driving an
Adit-owned grid) → the UI asks for a `TerminalSnapshot` for a `Viewport` → widgets.
The snapshot's `first_row`/`total_rows` are **absolute** indices into
`scrollback + screen`; `cursor_row` is relative to `first_row`. Selections are stored in
**absolute** rows and mapped into the viewport only at render, which is what lets a
selection survive scrolling.

**Screen → remote.** Key events → VT byte sequences (or PC/AT set-1 scancodes for RDP)
→ `Input`. Terminal-generated replies (cursor-position reports, device attributes) are
drained by `take_responses()` in `poll_events` and written straight back to the PTY.

**RDP.** The helper composites EGFX into a shared framebuffer, the session loop samples
it and emits `HostMsg::Tile`; the app uploads it as an `iced` image. Tiles are currently
always full-frame (see [decisions.md](decisions.md#9-full-frame-rdp-tiles-over-ipc-dirty-rectangles-deferred)).

## Storage layout

Everything lives in one relocatable config directory, resolved in this order:
`ADIT_CONFIG_DIR` → a pointer file (`config_location.txt`, kept in the *platform default*
dir to avoid a chicken-and-egg problem) → the platform default (`%APPDATA%\Adit` on
Windows).

| File | Contents |
|---|---|
| `profiles.json` | Profiles + group order (`version: 2`) |
| `settings.json` | UI/app preferences (`version: 1`) |
| `credentials.json` | Passwords/passphrases, **encrypted** (`version: 1`) |
| `logs/`, `downloads/` | Session transcripts, SFTP downloads |

Those three files are what "relocate the config directory" copies — pointing it at a
synced folder is how sessions *and* credentials travel between machines. Known-hosts are
deliberately **excluded**: they are machine-local trust.

Credentials use XChaCha20-Poly1305 with an Argon2id-derived key, a fresh nonce per
write, and atomic temp+rename. **The KDF's only secret input is a key compiled into the
binary — this is obfuscation, not secrecy.** See
[decisions.md](decisions.md#8-credentials-encrypted-in-the-config-directory-not-the-os-keyring--reversal).

## RDP subsystem

The helper is spawned per session (located via `ADIT_RDP_HOST`, then next to the exe,
then dev-tree fallbacks), **off the UI thread**, with `CREATE_NO_WINDOW`. The password
is written to its **stdin — never argv or env**, which would expose it in the process
list. Teardown sends `Close`, then reaps with a ~10 s grace before killing.

Inside the helper: two threads own stdin/stdout (nothing but framed `HostMsg` may touch
stdout), and the RDP session runs on a tokio runtime. Diagnostics go to
`%APPDATA%\Adit\rdp-helper.log` because the GUI discards the helper's stderr.

Connect is TCP → TLS → CredSSP/NLA, with a 30 s cap. Three pieces exist because IronRDP
doesn't provide them: **EGFX** compositing (incl. decoding RemoteFX Progressive, which
`ironrdp-egfx` does *not* do), **Server Redirection** parsing, and **RDSTLS** auth —
together these are what make GNOME Remote Desktop's system-mode handover work. The
connector is vendored with exactly three marked patches; see
[IRONRDP-PATCHES.md](../crates/adit-rdp/IRONRDP-PATCHES.md) and
[rdp-gnome-remote-desktop.md](rdp-gnome-remote-desktop.md).

## Build & release topology

Two cargo builds produce the two shipped binaries:

```
cargo build --release -p adit-app                              → adit-app.exe
cargo build --release --manifest-path crates/adit-rdp/Cargo.toml → adit-rdp-host.exe
```

The Inno Setup installer (`installer/adit.iss`) bundles both. Building only the app
leaves the helper stale — [`just dist`](../justfile) does both. Versions in the root
workspace, `crates/adit-rdp`, and `crates/adit-rdp-proto` must move together.

CI (`.github/workflows/ci.yml`) runs build + clippy (`-D warnings`) + test on Windows,
and a Docker-backed integration job on Linux that tests against a real `sshd`.

**Releases are built on CI, not locally, and triggered manually from `gh`:**

```bash
gh workflow run release.yml -f version=0.1.60
```

[`.github/workflows/release.yml`](../.github/workflows/release.yml) is a
`workflow_dispatch` that bumps the three versions, runs the gate, builds both binaries,
packages the installer, commits + tags the bump, and publishes the GitHub Release — so
the artifact is exactly what a clean, gated checkout produces. There is no `just release`.
See [decisions.md #16](decisions.md#16-releases-are-patch-only-cut-on-request-and-built-on-ci--reversal).
