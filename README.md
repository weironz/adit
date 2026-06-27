# Adit

Adit is a Rust-first desktop SSH terminal client inspired by SecureCRT and Xshell. The current MVP uses Tauri, while the long-term target is a native Rust app with Windows first and macOS support next.

## Target Architecture

The long-term direction is a native Rust desktop client based on `iced + russh + alacritty_terminal/vte`.

See [docs/native-rust-architecture.md](docs/native-rust-architecture.md).

## Stack

Current MVP stack:

- Rust backend with Tauri commands and events
- `ssh2`/libssh2 for SSH password authentication, PTY shell, input, output, and resize
- Vanilla TypeScript frontend
- xterm.js for terminal rendering

Native Rust workspace, now in progress:

- `iced` application shell
- Adit-owned `domain`, `session`, and `terminal` crate boundaries
- `russh` SSH auth chain with password, keyboard-interactive, SSH agent, and default private key fallback
- Long-lived SSH session actor with input, output, disconnect, and resize commands
- SecureCRT-style native workbench prototype with clickable menu bar, compact toolbar, editable session manager tree, tabs, terminal viewport, and status bar
- Persistent session profile CRUD: create, edit, save, delete, group, filter, move, and sort connection profiles from the native UI
- `adit-storage` JSON profile store under the platform app config directory
- Real ANSI/VT terminal core (`adit-terminal`) built on the `vte` parser: SGR 16/256/truecolor and bold/dim/reverse, cursor motion, erase/scroll regions, insert/delete, alternate screen, scrollback, wide (CJK) glyphs, and cursor-position/device-attribute replies wired back to the PTY
- Raw terminal keyboard routing for ignored window keyboard events, including normal text, Enter, Backspace, Tab, Esc, arrows, Home/End, Insert/Delete, PageUp/PageDown, F1-F12, Alt-prefix text, and Ctrl-A through Ctrl-Z
- Native SSH known-host verification stores host keys under the platform app config directory and rejects changed host keys.

## Current MVP

- Saved local connection profiles
- Password, keyboard-interactive, SSH agent, and default private key SSH login
- Interactive PTY shell
- Multi-tab terminal workspace
- Terminal resize synchronization
- Manual disconnect by closing a tab

Passwords are only used for the current connection attempt and are not saved in local storage.

## Development

Run the native Rust prototype:

```powershell
cargo run -p adit-app
```

In the native prototype, select or create a profile, edit the current form, optionally enter an SSH password, and click `连接 SSH`. If the password field is empty, Adit still tries the local SSH agent and default private keys under `~/.ssh`. The connect action uses the current form values and saves them before opening SSH. Profiles are saved to a JSON file shown in the status bar. Passwords are never written to the profile JSON; the connection dialog can optionally save them in the OS credential vault.

Check the native workspace:

```powershell
cargo check -p adit-app
```

Build a Windows release installer for the native app:

```powershell
cargo build -p adit-app --release
$env:ADIT_APP_EXE = "$PWD\target\release\adit-app.exe"
cargo build -p adit-installer --release
```

The installer binary is `target\release\adit-installer.exe`. The v0.1 release asset is published as `AditSetup-v0.1.0.exe`.

Run the current Tauri MVP:

```powershell
npm install
npm run tauri dev
```

For a production Windows build:

```powershell
npm run tauri build
```

## Roadmap

1. Native iced workspace skeleton — done
2. `russh` password SSH with long-lived PTY shell actor — done
3. `vte`-based terminal core integration — done (golden tests for color, cursor, scroll, alt screen, CJK, resize)
4. Raw keyboard input routing — done
5. Automatic UI-driven PTY resize measurement
6. Host key verification — done; known-host management UI remains
7. Key authentication, SSH agent, and credential vault support — partial: default private keys, Windows OpenSSH Agent, Pageant, SSH_AUTH_SOCK, and optional OS credential vault storage are wired
8. Session groups, tags, search, and import/export
9. Jump hosts, SFTP, logging, transcript search, and macOS packaging
