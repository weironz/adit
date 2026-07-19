# Adit Feature Roadmap

> **Backlog, not a description of the product.** For what Adit actually does today,
> read [features.md](features.md) — many items below have since shipped.

Date: 2026-06-27
Status: Living plan. Tracks the gap between the current native prototype and a
serious SecureCRT/Xshell-class SSH client.

This complements [native-rust-architecture.md](native-rust-architecture.md),
which covers the migration phases and target architecture. This document is a
prioritized **feature** backlog.

> **Next stage:** the researched, best-practice-backed plan for the next batch of
> work lives in [phase2-plan.md](phase2-plan.md) — integration tests vs. a real
> `sshd`, host-key policy + known_hosts UI, ProxyJump, interactive MFA, key
> passphrase / `.ppk`, per-session appearance, Windows code signing, OSC 8
> hyperlinks, and (deferred) Zmodem.

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
- **B2. Fonts & color schemes.** ✅ Done (global). An **外观设置** dialog (View
  menu) picks the terminal **font family** (system monospace + Consolas/Cascadia
  Mono/Cascadia Code/Courier New/Lucida Console), a **font size** stepper
  (9–28 px, which rescales the whole cell grid — render, hit-testing, and
  size-fitting all derive from it), and a **color scheme** (默认/Dracula/One
  Dark/Nord/Gruvbox Dark/Solarized Dark/Solarized Light — each a full 16-color
  ANSI palette + background/foreground/selection) with a live preview. All three
  persist in `settings.json` and apply via per-frame statics like the theme
  toggle. Future: per-profile overrides, custom user schemes.
- **B3. Hyperlinks.** URL detection and OSC-8 hyperlink support, click-to-open
  (with the link-safety confirmation already standard in this project).
- **B4. Mouse reporting passthrough.** Forward mouse events to the remote when an
  app enables mouse mode (vim/tmux/htop). Parser already tracks the modes.
- **B5. Paste safety.** Bracketed paste and a confirmation for multi-line pastes.
- **B6. Quick connect & broadcast.** ✅ Broadcast done. An **输入广播** toggle
  (toolbar `⇶` button + View menu) fans terminal keystrokes and the command-bar
  line out to **every connected session** at once (fan-out administration), with
  an always-visible amber `广播 ×N` badge in the status bar showing the reach so
  it is never silently left on. Still open: command history on the quick-connect
  bar.

## Phase C — File transfer & tunnels

- **C1. SFTP panel.** ✅ Done — a **dual-pane file manager** (SecureFX-style):
  local filesystem on the left, remote (`russh-sftp` over a second SSH
  connection reusing the session credential) on the right. Browse both sides,
  upload (local → remote, plus a `rfd` native picker / typed path), download
  (remote → the current local dir), rename/delete on **both** panes (with
  confirmation), multi-select batch transfer, click-to-select + double-click
  transfer, **pane-to-pane drag** and drag-from-Explorer upload, editable path
  bars, **clickable column sorting** (name/size/modified), a **local-time**
  modified column, a drag ghost that follows the cursor, and a managed transfer
  queue (source → destination, size, progress, speed, status; live counts, a
  clear-finished button, bounded history). Future: a "download as" dialog,
  overwrite confirmation, and resumable transfers.
- **C2. Port forwarding.** ✅ Done — **local (`-L`)**, **dynamic SOCKS5 (`-D`)**,
  and **remote (`-R`)** tunnels, created/managed from a tunnels panel, each
  running its own SSH connection (reusing the active session's credential).
  Local/dynamic bind a local listener and pipe over `direct-tcpip`; remote
  requests `tcpip-forward` and pipes server-opened forwarded channels to a local
  target. Definitions can be **saved per profile and auto-start on connect**,
  with live status and an active-connection count. Future: resilient
  reconnection of tunnels when their own connection drops.
- **C3. Jump host / proxy.** `ProxyJump` (bastion chaining) and `ProxyCommand`.

## Phase D — Workflow & power features

- **D1. Import/export.** Import OpenSSH `~/.ssh/config` and PuTTY sessions; export
  Adit profile sets.
- **D2. Per-profile session options.** Startup command, environment, terminal
  type, and character encoding.
- **D3. Tabs & splits.** ✅ Split panes done. The workspace tiles **2–4 sessions
  at once** — 2 or 3 side by side, 4 as a 2×2 grid — via a tab-row **▥ 分屏**
  button (or View menu). Each pane has a header (session title + status dot +
  close-pane ×); the focused pane carries the accent border and is the manager's
  active session, so keyboard input, the tab highlight, and the status bar all
  follow focus. Panes are sized independently (each session's PTY is resized to
  its pane) and selection hit-tests per pane. Clicking a tab loads that session
  into the focused pane instead of collapsing the split. Still open: tab rename,
  detach/reattach.
- **D4. Triggers & snippets.** Regex-triggered actions; reusable command snippets
  / macros.
- **D5. Logging enhancements.** ✅ Partly done. A **选项** dialog (File menu)
  exposes the **configuration folder** (where `profiles.json` / `settings.json` /
  logs / downloads live) with open-in-Explorer, relocatable via the
  `ADIT_CONFIG_DIR` env override; and a **session-log** section with a
  configurable **log folder**, a **filename pattern** (`%N` name, `%H` host,
  `%Y/%M/%D` date, `%h/%m/%s` time) with a live preview, and **auto-log on
  connect**. Still open: per-profile log policy, ANSI-stripped plain-text option,
  and an optional input (keystroke) log.

## Phase E — Packaging & platform

- **E1. macOS build.** Packaging, signing/notarization (Windows installer exists).
- **E2. High-DPI & accessibility** polish.
- **E3. Auto-update.**

## Non-goals (per architecture)

Telnet, serial, RDP, local-shell tabs, and a full plugin system are out of scope
for the first native milestones.

## Suggested order

~~A1–A2 (security baseline)~~ ✅ → ~~A3 (reconnect + keepalive)~~ ✅ →
~~C1 (SFTP)~~ ✅ → **B1/B2 (scrollback search, fonts/color schemes)** →
A4 (key passphrase) → C2 (port forwarding) → the rest as demand dictates.
