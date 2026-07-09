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

The Windows installer is built with **Inno Setup** from [`installer/adit.iss`](installer/adit.iss) (a proper setup wizard), not a workspace crate.

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
- Protocols beyond SSH: local shell (ConPTY), serial (COM/tty), and RDP (launches the system client).
- Split panes (2–4 tiled sessions), input broadcast to all sessions, per-profile startup command, per-profile `TERM`, and a configurable connect timeout.
- Terminal power features: scrollback search (Ctrl+Shift+F) with highlight and next/prev, mouse-reporting passthrough for TUIs (vim/tmux/htop), bracketed paste + multi-line paste confirmation, optional copy-on-select / right-click-paste, and italic/dim rendering.
- Configurable fonts + color schemes, configurable scrollback size, and a configuration folder (relocatable via `ADIT_CONFIG_DIR`).
- Session logging with a configurable folder, filename pattern, auto-log-on-connect, and an optional ANSI-stripped plaintext format.
- Import hosts from `~/.ssh/config`, and in-app check-for-updates with one-click update.
- Dark/light theme and full settings persistence.

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

Build the Windows installer (requires [Inno Setup 6](https://jrsoftware.org/isdl.php)):

```powershell
cargo build -p adit-app --release
& "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe" /DAppVersion=<version> installer\adit.iss
```

This produces `target\release\adit-installer-v<version>.exe` — a setup wizard that installs to `C:\Program Files\Adit` (all users, or per-user), creates shortcuts, registers an uninstaller, and closes a running instance before updating.

## Roadmap

Most of the phased plan is implemented — SSH/SFTP/tunnels, four protocols, split panes, broadcast, fonts/schemes, scrollback search, mouse passthrough, bracketed paste, `~/.ssh/config` import, in-app updates, and the Inno Setup installer.

Still open: jump host / `ProxyJump`, command snippets, tab rename, code signing, CI, and macOS packaging. See [docs/feature-roadmap.md](docs/feature-roadmap.md) for the full status.
