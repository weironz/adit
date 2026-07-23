# CLAUDE.md

Guidance for Claude Code (and humans) working in this repo. Start here, then follow
the links — [README.md](README.md) covers what Adit is and how to run it.

Orientation: [docs/architecture.md](docs/architecture.md) (how it's built) ·
[docs/features.md](docs/features.md) (what it does + known gaps) ·
[docs/decisions.md](docs/decisions.md) (why, including reversals).

Adit is a Windows-first SSH / SFTP / RDP terminal client in Rust (`iced` + `russh` +
`vte`), aiming at SecureCRT-style ergonomics.

## Build & test

Everything is wrapped in a [`justfile`](justfile) — run `just` to list recipes.

```powershell
just ci        # exactly what CI runs: build + clippy (-D warnings) + test
just dist      # release-build BOTH shippable binaries (app + RDP helper)
just deploy    # copy fresh binaries over the installed Adit, for quick testing
```

- **CI treats warnings as errors** (`cargo clippy --workspace --all-targets -- -D warnings`).
  Run `just clippy` before pushing; a stray warning turns the build red.
- `cargo test --workspace` does **not** cover `crates/adit-rdp` (separate workspace) and
  skips `#[ignore]`d tests (they need a real host or Docker).

## Traps that cost real debugging time

### The RDP helper is a separate workspace and a separate process
IronRDP's `picky` exact-pins pre-release RustCrypto crates that conflict
irreconcilably with `russh`'s. Two `ecdsa` versions can't live in one binary, and
`=`-pins can't be `[patch]`ed apart, so `crates/adit-rdp` is `exclude`d from the root
workspace and ships as `adit-rdp-host.exe`, driven over stdin/stdout (length-prefixed
bincode, types in `adit-rdp-proto`).

- Build it with `--manifest-path crates/adit-rdp/Cargo.toml` — **`-p adit-rdp` does not
  work**, and `cargo build -p adit-app` alone leaves the helper stale.
- Its version must be bumped in lockstep with the root workspace and `adit-rdp-proto`.
- Do **not** "fix" this by bumping `russh` in the root workspace; that was tried and
  reverted. See [`crates/adit-rdp/IRONRDP-PATCHES.md`](crates/adit-rdp/IRONRDP-PATCHES.md).

### `ironrdp-egfx` does not decode RemoteFX Progressive
It only decodes H.264. For a progressive frame it frame-acknowledges and hands the raw
stream to `on_wire_to_surface2`. Leaving that unimplemented renders a **solid black
desktop on a fully healthy session** — the symptom looks like a connection bug and is
not. `egfx.rs` decodes it via `ironrdp_graphics::progressive::ProgressiveDecoder`.
Details in [docs/rdp-gnome-remote-desktop.md](docs/rdp-gnome-remote-desktop.md).

### Windows main-thread stack is 1 MiB
Deep RDP render paths overflow it and the process dies **silently — no panic, no
crash.log**. `crates/adit-app/build.rs` passes `/STACK:33554432` on MSVC. If Adit
vanishes with no log, check the Windows Event Log (Application Error) for
`0xc00000fd` (STATUS_STACK_OVERFLOW) before assuming anything else.

### Blocking the UI thread reads as "Not Responding"
`fs::write` can block for seconds behind antivirus or a cloud-synced folder, and
spawning a process can block on Defender. Both have caused apparent freezes; profile
saves and helper spawns are off-thread now. Keep them that way.

### `powershell.exe` (5.1) mangles UTF-8
Its `Set-Content` defaults to ANSI and corrupted `Cargo.toml`'s em-dashes during a
version bump. The justfile pins `windows-shell` to **`pwsh`** (7) and the `bump`
recipe writes UTF-8-no-BOM explicitly. Don't switch it back.

### Terminal selection is anchored in absolute scrollback rows
Not viewport rows — that's what lets a selection survive scrolling and lets a drag
past the pane edge auto-scroll. It's mapped back to viewport space only at render.
`terminal_point_from_cursor` stays viewport-relative on purpose, because mouse
reporting must send viewport cells to the remote app. A drag that leaves the widget
is tracked via the runtime's global `CursorMoved`, since `mouse_area::on_move` stops
at the widget's bounds.

## Conventions

- **Releases are patch bumps** (`0.1.54` → `0.1.55`) and happen **only when asked**.
  Otherwise just commit and push. **The release is built and published entirely on CI,
  and triggered manually from `gh` — never via `just`:**

  ```bash
  gh workflow run release.yml -f version=0.1.60
  ```

  That dispatches [`.github/workflows/release.yml`](.github/workflows/release.yml)
  (a `workflow_dispatch`), which bumps the three crate versions in lockstep (root
  workspace, `crates/adit-rdp/Cargo.toml`, `crates/adit-rdp-proto/Cargo.toml`), runs
  the gate (build + clippy + test), builds both binaries, packages the Inno Setup
  installer, commits + tags the bump, and creates the GitHub Release — so what ships is
  exactly what a clean, gated checkout produces, never a developer's local artifacts.
  A red gate produces no installer. Watch it with `gh run watch`. There is deliberately
  **no `just release`** (that used to push a tag and tangle the trigger up with the
  build). `just installer` / `just deploy` still build locally for smoke-testing, but
  never publish.
- **Secrets never go in the repo.** Passwords/keys live in the encrypted credential
  store; the RDP helper takes its password over **stdin, never argv or env** (argv is
  world-readable via the process list). Test-host credentials stay out of git.
- UI strings are **Simplified Chinese** — match the surrounding text.
- Comments explain *why*, not *what*. Several in this codebase encode a bug that was
  expensive to find (see the traps above); don't delete them as noise.

## Debugging leverage

- The RDP helper logs to `%APPDATA%\Adit\rdp-helper.log` (`RUST_LOG` overrides the
  default filter) — the GUI discards its stderr, so this is the only window into it.
- Manual RDP harness against a real host, which can dump decoded frames to PNG:
  `crates/adit-rdp-proto/tests/debug_connect.rs` (env-driven, `--ignored`).
- Real-sshd integration tests: `cargo test -p adit-ssh --features integration --
  --include-ignored` (needs Docker; CI runs these).

**Capture state before theorizing.** The black-screen and connect-freeze bugs each ate
several wrong "fixes" that pattern-matched the symptom. What actually solved them was
dumping real frames to PNG and looking, and taking a minidump of the hung process.
Reach for the harness and the logs first.
