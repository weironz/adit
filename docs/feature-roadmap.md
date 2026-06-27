# Adit Feature Roadmap

Date: 2026-06-27
Status: Living plan. Tracks the gap between the current native prototype and a
serious SecureCRT/Xshell-class SSH client.

This complements [native-rust-architecture.md](native-rust-architecture.md),
which covers the migration phases and target architecture. This document is a
prioritized **feature** backlog.

## Current state (implemented)

- Connection profiles with folders/groups: create, edit, delete, drag-reorder,
  sort, rename group.
- Authentication chain: password, keyboard-interactive (MFA-aware), public key
  (`~/.ssh` defaults + explicit identity file), and SSH agent
  (Windows OpenSSH pipe / Pageant, Unix `SSH_AUTH_SOCK`).
- Host-key handling: **interactive first-use confirmation** — the handshake
  pauses and shows the server's SHA256 fingerprint for accept/reject before the
  key is recorded to a per-app `known_hosts`; a changed key shows a MITM warning
  and can be reviewed/updated. (The one-shot probe keeps non-interactive TOFU.)
- Secrets: optional password persistence in the OS credential vault (keyring).
- Terminal core: real `vte`-driven VT/ANSI grid — 16/256/truecolor + attributes,
  cursor motion, erase/scroll regions, alternate screen, scrollback, wide (CJK)
  glyphs, DSR/DA replies.
- Terminal UX: raw keyboard routing (Ctrl/Alt/function/navigation keys), mouse
  selection + copy/paste, scrollback navigation, auto PTY resize.
- App: dark/light theme toggle, full settings persistence (theme, folded groups,
  window size), Windows installer, app icon.
- **Session output logging**: toggle raw PTY transcript recording per session to
  a file under the app `logs/` dir, with a REC indicator.
- **Keepalive + auto-reconnect**: SSH keepalive (30s, drop after 3 missed) keeps
  idle sessions alive — also fixes a prior 20s inactivity-timeout that dropped
  idle sessions; established sessions auto-reconnect on an unexpected drop with
  exponential backoff (1→30s, 10 attempts), reusing the session credential. A
  persistent toggle lives under the Session menu.

## Phase A — Security & connection robustness (highest priority)

A "serious" client should not silently trust new hosts or drop on idle.

- **A1. First-use host-key prompt.** ✅ Done. The handshake pauses on an unknown
  host, emits a `HostKeyPrompt`, and only records + continues on user approval
  (rejection aborts with `HostKeyRejected`).
- **A2. Host-key mismatch UX.** ✅ Done (basic). A changed key shows a MITM
  warning with the previous and new fingerprints; accepting replaces the stored
  key, rejecting aborts. Future: per-host "always reject" memory.
- **A3. Reconnect + keepalive.** ✅ Done (global). SSH keepalive (30s / max 3)
  survives idle NAT/firewall timeouts; established sessions auto-reconnect on an
  unexpected drop with exponential backoff (1→30s, ≤10 attempts), reusing the
  in-memory credential. Skips bad-password/unreachable loops (only sessions that
  reached "connected" reconnect) and stops on user disconnect or clean exit.
  Future: per-profile policy + configurable interval/attempts.
- **A4. Key passphrase prompt + key picker.** Distinct passphrase entry for
  encrypted keys (today the password field doubles as passphrase); a file picker
  for the identity file in the profile editor.

## Phase B — Daily-driver terminal UX

- **B1. Scrollback search.** Find-in-history with match highlight and next/prev.
- **B2. Fonts & color schemes.** Configurable font family/size and terminal color
  palette (global + per-profile), persisted via settings.
- **B3. Hyperlinks.** URL detection and OSC-8 hyperlink support, click-to-open
  (with the link-safety confirmation already standard in this project).
- **B4. Mouse reporting passthrough.** Forward mouse events to the remote when an
  app enables mouse mode (vim/tmux/htop). Parser already tracks the modes.
- **B5. Paste safety.** Bracketed paste and a confirmation for multi-line pastes.
- **B6. Quick connect & broadcast.** Command history on the quick-connect bar and
  "send input to all sessions" for fan-out administration.

## Phase C — File transfer & tunnels

- **C1. SFTP panel.** `russh-sftp`-backed file browser: list, upload, download,
  rename, delete, mkdir, with transfer progress. (Currently a stub.)
- **C2. Port forwarding.** Local (`-L`), remote (`-R`), and dynamic SOCKS (`-D`)
  tunnels, created and managed from the UI, tied to a session.
- **C3. Jump host / proxy.** `ProxyJump` (bastion chaining) and `ProxyCommand`.

## Phase D — Workflow & power features

- **D1. Import/export.** Import OpenSSH `~/.ssh/config` and PuTTY sessions; export
  Adit profile sets.
- **D2. Per-profile session options.** Startup command, environment, terminal
  type, and character encoding.
- **D3. Tabs & splits.** Split panes, tab rename, detach/reattach.
- **D4. Triggers & snippets.** Regex-triggered actions; reusable command snippets
  / macros.
- **D5. Logging enhancements.** Timestamped filenames, per-profile auto-log,
  ANSI-stripped plain-text option, and an optional input (keystroke) log.

## Phase E — Packaging & platform

- **E1. macOS build.** Packaging, signing/notarization (Windows installer exists).
- **E2. High-DPI & accessibility** polish.
- **E3. Auto-update.**

## Non-goals (per architecture)

Telnet, serial, RDP, local-shell tabs, and a full plugin system are out of scope
for the first native milestones.

## Suggested order

~~A1–A2 (security baseline)~~ ✅ → ~~A3 (reconnect + keepalive)~~ ✅ →
**C1 (SFTP, biggest user-visible win)** → B1/B2 (search, fonts) → A4 (key
passphrase) → the rest as demand dictates.
