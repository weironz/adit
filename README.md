# Adit

<img src="assets/icon.png" alt="Adit icon" width="96" align="right" />

Adit is a native, Rust-only desktop SSH terminal client inspired by SecureCRT and Xshell. Windows is the first-class target, with macOS support kept in the architecture.

It is built on `iced` (GUI), `russh` (pure-Rust SSH), and an Adit-owned `vte`-based terminal core — no web view, no JavaScript. The product is the Cargo workspace under [`crates/`](crates/).

> The original Tauri + TypeScript/xterm.js prototype has been removed now that the native client supersedes it; it remains available in git history.

The app icon lives in [`assets/`](assets/) and is reproducible with `python assets/make_icon.py` (requires Pillow), which regenerates the master PNG plus the build inputs: the Windows exe-resource `.ico` ([crates/adit-app/assets/](crates/adit-app/assets/)) embedded via `build.rs`, and the raw RGBA window icon ([crates/adit-ui/assets/](crates/adit-ui/assets/)) loaded at runtime.

## Architecture

A mostly-pure-Rust desktop SSH client based on `iced + russh + vte`. See [docs/native-rust-architecture.md](docs/native-rust-architecture.md) for the design and [docs/feature-roadmap.md](docs/feature-roadmap.md) for the prioritized feature plan.

The workspace crates:

- `adit-app` — binary entrypoint (iced runtime, embedded window/exe icon)
- `adit-ui` — iced screens, terminal widget, theme, input handling
- `adit-session` — session manager and per-session actor lifecycle
- `adit-ssh` — `russh` wrapper: auth, host-key verification, PTY shell, keepalive
- `adit-terminal` — `vte`-driven VT/ANSI grid and render-ready snapshots
- `adit-storage` — profiles, settings, OS credential vault, log directory
- `adit-domain` — shared ids, errors, profile/auth models
- `adit-installer` — Windows packaging (embeds `adit-app.exe`, Start-menu shortcut)

## Features

- Connection profiles with folders/groups: create, edit, delete, drag-reorder, sort, rename group, filter.
- Authentication: password, keyboard-interactive (MFA-aware), public key (`~/.ssh` defaults or an explicit identity file), and SSH agent (Windows OpenSSH pipe / Pageant, Unix `SSH_AUTH_SOCK`).
- Host-key security: interactive first-use confirmation showing the SHA256 fingerprint, with a changed-key (MITM) warning; verified keys are stored in a per-app `known_hosts`.
- Secrets: optional password persistence in the OS credential vault (never in profile JSON).
- Real ANSI/VT terminal: 16/256/truecolor + attributes, cursor motion, erase/scroll regions, alternate screen, scrollback, wide (CJK) glyphs.
- Terminal UX: raw keyboard routing (Ctrl/Alt/function/navigation keys), mouse selection + copy/paste, scrollback navigation, automatic PTY resize.
- Multi-tab workspace, keepalive, and auto-reconnect with exponential backoff on unexpected drops.
- SFTP dual-pane file manager (SecureFX-style): browse local and remote side by side; transfer via double-click, multi-select batch, pane-to-pane drag, or drag-from-Explorer (plus a native file picker); rename/delete on both panes; clickable column sorting; and a detailed transfer queue (destination, size, progress, speed) over a second SSH connection reusing the session credential.
- Port forwarding: local (`-L`), dynamic SOCKS5 (`-D`), and remote (`-R`) tunnels, created and managed from a tunnels panel, optionally saved per profile to auto-start on connect, with live status and active-connection counts.
- Session output (transcript) logging to a file, on demand.
- Dark/light theme and full settings persistence (theme, folded groups, window size, auto-reconnect).

## Development

Run the app:

```powershell
cargo run -p adit-app
```

Select or create a profile, optionally enter an SSH password, and connect. With an empty password Adit still tries the SSH agent and default keys under `~/.ssh`. Profiles persist to a JSON file under the platform app config directory (shown in the status bar); passwords are never written there.

Check, lint, and test the workspace:

```powershell
cargo check --workspace
cargo clippy --workspace --all-targets
cargo test --workspace
```

Build the Windows installer:

```powershell
cargo build -p adit-app --release
$env:ADIT_APP_EXE = "$PWD\target\release\adit-app.exe"
cargo build -p adit-installer --release
```

The installer binary is `target\release\adit-installer.exe`.

## Roadmap

Done: native workspace, `russh` auth chain, `vte` terminal core, raw keyboard input, interactive host-key verification, keepalive + auto-reconnect, session logging, theme + settings persistence, app icon and Windows installer.

Next: SFTP file transfer, scrollback search, fonts/color schemes, port forwarding, jump hosts, import/export, and macOS packaging. See [docs/feature-roadmap.md](docs/feature-roadmap.md) for the full phased plan.
