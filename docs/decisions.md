# Design decisions

Why Adit is built the way it is. Each entry records the decision, the reasoning, and
what it costs — including the ones we later **reversed**, because a decision record
that quietly edits out its own mistakes is worth very little.

For what the code looks like today see [architecture.md](architecture.md); for what
it does see [features.md](features.md). The original 2026-06-27 design doc is kept at
[native-rust-architecture.md](native-rust-architecture.md) for historical context.

---

## 1. A native Rust GUI (`iced`), not a web view

**Decision.** Build the client in Rust end to end with `iced`, and retire the earlier
Tauri/TypeScript MVP.

**Why.** The product is a terminal: keyboard handling, selection, and large-scrollback
rendering are the whole experience, and owning them in one language avoids a
WebView/IPC seam in the hot path.

**Cost.** `iced` is young; several things a browser gives free had to be hand-built —
notably the terminal grid renderer and the scrollbar (see #10, #11). Every widget must
be theme-aware manually.

## 2. `russh` for SSH

**Decision.** Pure-Rust `russh` rather than binding libssh2/OpenSSH.

**Why.** No C toolchain in the Windows build, and direct access to the auth state
machine — needed for the interactive-MFA and host-key-prompt flows, which are hard to
express through a callback-based C API.

**Cost.** `russh`'s dependency pins later collided head-on with IronRDP's (see #4).

## 3. `vte` plus an Adit-owned grid — *changed from the original plan*

**Original plan.** Reuse `alacritty_terminal`, falling back to `vte` only if the
integration proved too coupled.

**What we did.** Went straight to `vte` (the parser only) with our own grid, scrollback,
and snapshot types.

**Why.** `alacritty_terminal` owns its own rendering and window assumptions; Adit needs
a *snapshot* it can hand to `iced` widgets, plus split panes and per-session viewports.
Taking only the parser kept the boundary clean.

**Cost.** We own the emulator's gaps: no reflow on resize, no DCS/Sixel, no combining
marks. See the limitations table in [features.md](features.md).

## 4. RDP lives in a separate workspace *and* a separate process

**Decision.** `crates/adit-rdp` is `exclude`d from the root workspace, keeps its own
`Cargo.lock`, and ships as `adit-rdp-host.exe` driven over stdin/stdout.

**Why.** Not a style choice — a hard constraint. IronRDP pulls `picky`, which
**exact-pins** pre-release RustCrypto crates (`ecdsa`, `p256`…) that conflict with what
`russh` requires. Two `ecdsa` versions cannot coexist in one binary, and `=`-pins can't
be reconciled with `[patch]`. One binary is therefore impossible until that RC train
stabilises.

**Rejected alternative.** Bumping `russh` to align. This was attempted and reverted —
no `russh` version satisfies both sides.

**Consequences.**
- Process isolation is a genuine bonus: an RDP crash can't take the terminal app down.
- The helper must be built separately; `cargo build -p adit-app` silently leaves it stale.
- Versions must be bumped in lockstep across three manifests.
- Frames cross an IPC boundary, which forced decision #9.
- The password crosses that boundary too — over **stdin**, never argv/env, which would
  expose it in the process list.

## 5. Vendor the IronRDP connector with three marked patches, don't fork

**Decision.** `[patch.crates-io]` a vendored `ironrdp-connector` carrying exactly three
hunks, each tagged `ADIT PATCH`, documented in
[`crates/adit-rdp/IRONRDP-PATCHES.md`](../crates/adit-rdp/IRONRDP-PATCHES.md).

**Why.** Upstream can't be used unmodified (EGFX must be advertised; RDSTLS must be
requested on a redirect; the MCS message channel deadlocks against some Windows hosts).
A fork would drift; three marked hunks in one file make re-vendoring a mechanical reapply
and let each patch be dropped independently when upstream lands the fix.

## 6. Implement RDSTLS and Server Redirection ourselves

**Decision.** Hand-write both (`rdstls.rs`, `redirect.rs`) rather than wait for IronRDP.

**Why.** GNOME Remote Desktop's system ("Remote Login") mode authenticates on a front
daemon and then hands the client to the real session via a Server Redirection PDU
(pduType `0xa`), re-authenticating with one-time credentials over RDSTLS. IronRDP has
the RDSTLS *flag* but not the exchange, and can't decode the redirect PDU
([IronRDP#139](https://github.com/Devolutions/IronRDP/issues/139)). Without both, that
entire deployment mode is unreachable.

**Note.** The redirect parser scans for the `SEC_REDIRECTION_PKT` marker rather than
hard-coding a pad width, because the padding varies by encoder.

## 7. Decode RemoteFX Progressive ourselves

**Decision.** `egfx.rs` decodes progressive frames with
`ironrdp_graphics::progressive::ProgressiveDecoder` and composites the 64×64 tiles.

**Why.** `ironrdp-egfx` decodes **only H.264**. For a progressive frame it
frame-acknowledges and hands over the raw stream. With no decoder configured, servers
fall back to progressive — so leaving this unimplemented renders a **solid black desktop
on a fully connected session**, a symptom that looks like a connection bug and isn't.

**Cost.** Software decode; no H.264 path, so hosts that negotiate AVC still render black.

## 8. Credentials: encrypted in the config directory, not the OS keyring — *reversal*

**Original non-goal.** The 2026-06-27 design explicitly listed "password persistence
outside the OS credential vault" as out of scope.

**What we do now.** Passwords and passphrases live encrypted in `credentials.json`
inside the (relocatable) config directory. XChaCha20-Poly1305, Argon2id-derived key,
fresh nonce per write, atomic temp+rename. Legacy keyring secrets are imported once.

**Why we reversed it.** The keyring is machine-local. Users who point the config
directory at a synced folder (Dropbox) to carry their sessions between machines found
every password missing on the other end — the store defeated the workflow it was
supposed to support.

**The security trade-off, stated plainly.** The KDF's only secret input is a key
**compiled into the binary**; there is no master password. This was chosen deliberately
for zero-setup syncing. It keeps credentials out of plaintext on disk, out of backups,
and un-greppable — but **anyone holding the file and the (open-source) key recovers
every password**. It is *obfuscation, not secrecy*, and must never be described
otherwise. `derive_key` reserves the mixing point so a real user passphrase can be added
later without a format change; only that would make the store genuinely secret.

## 9. Full-frame RDP tiles over IPC (dirty rectangles deferred)

**Decision.** Every RDP update ships the whole framebuffer as one `HostMsg::Tile`.

**Why.** Simple and correct while the graphics path was still being brought up; the
`Tile` message already carries x/y/w/h, so partial updates need no wire change.

**Cost.** Bandwidth, and a message cap that must accommodate a full 8192×8192 RGBA frame
(288 MiB). Marked `TODO(perf)` in `session.rs`.

## 10. The terminal selection is anchored in absolute scrollback rows

**Decision.** Store selections as absolute row indices, mapping to viewport rows only at
render time.

**Why.** Viewport-relative selections silently change meaning when the view scrolls,
which forced the old code to *discard* the selection on any scroll. Absolute anchoring is
what makes "keep selecting while the view scrolls" possible at all — including the
auto-scroll when a drag passes the pane edge.

**Related.** A drag that leaves the widget is tracked via the runtime's global
`CursorMoved`, because `mouse_area::on_move` stops reporting at the widget bounds. Hit
testing (`terminal_point_from_cursor`) stays viewport-relative on purpose: mouse
reporting must send *viewport* cells to the remote application.

## 11. A hand-built terminal scrollbar

**Decision.** Don't wrap the terminal in an `iced::scrollable`.

**Why.** The terminal renders a fixed, viewport-sized grid; scrollback is served by
re-snapshotting at a different offset, so there is no overflowing content for a native
scrollable to scroll. The thumb is sized/positioned from the snapshot and dragged via
global cursor tracking.

**Cost.** The gutter's width must be subtracted when fitting columns, or the remote wraps
just past the visible edge.

## 12. One global scrollback limit, not per-terminal

**Decision.** `SCROLLBACK_LIMIT` is a process-wide `AtomicUsize`.

**Why.** It is a user preference, like the theme — a global read avoids threading it
through every session and terminal constructor.

**Cost.** Genuinely global; a per-session override would need a real refactor. Changes
apply lazily, on the next line pushed.

## 13. A 32 MiB main-thread stack on Windows

**Decision.** `crates/adit-app/build.rs` passes `/STACK:33554432` on MSVC.

**Why.** Windows gives the main thread 1 MiB. Deep RDP render paths overflow it, and the
process dies **silently** — no panic, no crash log, nothing but a `0xc00000fd` in the
Windows Event Log.

## 14. Profile writes are async and atomic; the UI thread never blocks on disk

**Decision.** `save_catalog_async` serializes on the caller, hands the bytes to a
dedicated writer thread, coalesces bursts, and writes temp+rename.

**Why.** `fs::write` can block for seconds behind antivirus or a cloud-synced folder. On
the UI thread that is indistinguishable from a hang, and was a real "Not Responding"
report. (Process spawning has the same hazard — the RDP helper is spawned off-thread for
the same reason.)

**Known gap.** The *synchronous* `save_catalog` and `SettingsStore::save` are still plain
non-atomic writes. See [features.md](features.md#known-gaps).

## 15. More protocols than SSH — *reversal*

**Original non-goal.** "Telnet, serial, RDP, or local shell tabs" were explicitly out of
scope for the first native milestone.

**What we do now.** Local shell (ConPTY on Windows), serial, and RDP are all supported
protocols. The milestone was met, and the session model generalised cleanly because a
session is defined by its event stream rather than by SSH specifics — RDP is the only one
that isn't a VT terminal, and it carries a separate surface.

## 16. Releases are patch-only, cut on request, and built on CI — *reversal*

**Decision.** Every release bumps the patch component; releases happen when asked, not
automatically after a change. CI runs clippy with `-D warnings`. **The shippable
artifact is built and published by GitHub Actions on a version tag — not from a
developer's machine.**

**Why.** The project ships from one machine to one user; a stream of minor bumps conveyed
nothing. Treating warnings as errors keeps the lint budget at zero rather than letting it
rot.

**What changed, and why.** The flow used to be fully local: `just release` ran the gate,
built both binaries, packaged the Inno Setup installer, and `gh release create`d it — all
on the maintainer's machine, *without waiting for GitHub Actions*. That bit us twice.
First, a local `just dist` once crashed mid-build (a toolchain-class fault) while a
piped-to-`grep` invocation hid the failure, and `just installer` then packaged the
**stale** binaries left over from a prior build — v0.1.57 shipped a 0.1.56 helper. The
immediate fix was to make `installer` depend on `dist` so packaging can't outrun a failed
build. Then v0.1.58 shipped on a **red CI**: an `SftpCommand` field change compiled
locally but broke the `--features integration` tests, which `cargo test --workspace`
and `cargo clippy --all-targets` don't compile — so the local gate was green while CI was
not, and the local release didn't wait for CI to find out.

The lesson both times: *what a developer's machine produces is not what a clean, gated
checkout produces.* So the build and publish moved onto CI. `just release <ver>` now only
bumps the three crate versions in lockstep, commits, tags, and pushes; the tag triggers
[`release.yml`](../.github/workflows/release.yml), which re-runs the full gate
(build + clippy + test), builds both binaries, installs Inno Setup, packages the
installer, and creates the GitHub Release. A red gate produces no installer, so a release
can no longer ship on a broken tree. `just ci` also compile-checks the integration tests
(`--no-run`) now, so that class of break is caught locally too.

**Cost.** A release is no longer instant — it waits on a CI runner (cold cache + Inno
Setup install ≈ several minutes). Worth it: the artifact is now reproducible and gated.

**Mechanics.** [`justfile`](../justfile) wraps the local half; it pins `windows-shell` to
`pwsh`, because Windows PowerShell 5.1's `Set-Content` defaults to ANSI and corrupted
`Cargo.toml`'s UTF-8 during a version bump. `just installer` / `just deploy` still build
locally for smoke-testing, but no longer publish.
