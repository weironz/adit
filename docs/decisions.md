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

**Alternatives considered.** Only the web view was weighed at the time; the native
toolkits were not compared on the record. Reviewed since, the conclusion holds:

- **Slint** — the only mature Rust toolkit with official Android support (iOS in
  progress), so it is the obvious candidate if mobile ever matters. It is still not on
  the decision path. What it gains over `iced` is platform reach and a declarative DSL;
  neither touches the actual bottleneck, which is data-dense desktop chrome — the
  session tree, the sortable multi-select dual-pane browser, the drag-and-drop. Slint's
  standard widgets cover lists and basic tables, so those would still be hand-built,
  exactly as they are now. What Slint *preserves* — one Rust stack, one binary, no FFI
  seam — `iced` already gives us, and it would cost a rewrite of `adit-ui` plus a GPL
  obligation or a paid licence.
- **Flutter + Rust** (via `flutter_rust_bridge`) — the only option generally available
  on all five platforms today, and genuinely stronger on dense chrome: `DataTable`,
  `Draggable`/`DragTarget`, `desktop_drop`, and hot reload instead of a recompile per
  tweak. `xterm.dart` proves a 60fps terminal grid is achievable there. Rejected on
  structure: it inverts the hosting relationship, demoting Rust from *the application*
  to *a library called by the application*. Crash traces span two runtimes, logging
  needs bridging, lifecycle is owned by Dart, and CI carries two toolchains — ongoing
  costs, not one-off. It is also self-undermining here: Dart already has `xterm.dart`
  and `dartssh2`, so "Flutter + Rust" tends to collapse into "Flutter", i.e. a rewrite.
  With `adit-ui` at ~12.7k lines and RDP living in its own workspace and process
  (see #4), that rewrite is not a UI swap.
- **`egui`** — immediate mode. Fine for tooling, wrong for a dual-pane file manager
  with persistent selection and drag state.
- **`Xilem`** — the Linebender/Vello successor to Druid, and the strongest long-term
  bet in Rust GUI. Excluded only because it is not production ready.
- **A custom `winit` + `wgpu` renderer**, the Alacritty/WezTerm/Rio/Zed route. That
  precedent does not transfer: those products have almost no application chrome —
  Alacritty has no settings UI at all — whereas half of Adit is session management,
  SFTP, and dialogs. It needs a real widget toolkit; they do not.

**Revisit if** (and only if) mobile enters scope — Flutter + Rust would lead, and note
that the mobile feature set is a strict subset regardless of toolkit, because iOS
forbids fork/exec (no local shell) and has no serial stack; **or** Xilem reaches 1.0;
**or** `iced` goes unmaintained *and* System76's `libcosmic` fork stalls with it.
Infrequent `iced` releases are **not** a trigger — a slow release cadence and a dead
project are different things.

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

**Cost.** Software decode. H.264/AVC is deliberately filtered out of the advertised
capabilities (no decoder is wired), so servers negotiate the tile path instead of
sending AVC we would render black. What "the tile path" grew into is decision #17.

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
checkout produces.* So the whole release moved onto CI, and the trigger is **manual and
separate from `just`** — dispatched straight from `gh`:

```bash
gh workflow run release.yml -f version=0.1.60
```

[`release.yml`](../.github/workflows/release.yml) is a `workflow_dispatch`: it bumps the
three crate versions in lockstep, runs the full gate (build + clippy + test), builds both
binaries, installs Inno Setup, packages the installer, commits + tags the bump, and
creates the GitHub Release. A red gate produces no installer, so a release can no longer
ship on a broken tree. `just ci` also compile-checks the integration tests (`--no-run`)
now, so that class of break is caught locally too.

An intermediate design had `just release` push a version tag and a `tags: ['v*']` trigger
fire the workflow. That was dropped: it tangled the local tool (`just`) up with the CI
trigger, and a `gh workflow run` dispatch is one explicit, self-contained action with the
version passed as an input. There is deliberately **no `just release`** recipe now.

**Cost.** A release is no longer instant — it waits on a CI runner (cold cache + Inno
Setup install ≈ several minutes). Worth it: the artifact is now reproducible and gated.

**Mechanics.** The workflow bumps versions with `sed`, not PowerShell — Windows
PowerShell 5.1's `Set-Content` defaults to ANSI and once corrupted `Cargo.toml`'s UTF-8
em-dashes during a bump, which is also why the [`justfile`](../justfile) pins
`windows-shell` to `pwsh` for its local recipes. `just installer` / `just deploy` still
build locally for smoke-testing, but no longer publish.

## 17. IronRDP as the RDP engine, and the tile path before H.264

**Decision.** Build RDP on **IronRDP** (pure Rust) rather than binding **FreeRDP**
(C), and ship the codec-mosaic "tile path" — RemoteFX Progressive + ClearCodec +
NSCodec + the EGFX bitmap cache — before any H.264/AVC support.

**Why IronRDP.** One language end to end, no FFI seam, no C build chain on Windows
(FreeRDP means vcpkg/CMake plus an ffmpeg or openh264 dependency for its codecs), and
memory safety in the one component that parses hostile network input. The helper is
already a separate process (#4), which would also have fit a FreeRDP wrapper — the
FFI and build-chain costs were the deciding factor, not architecture.

**What it actually cost.** IronRDP's codec coverage was written against GNOME Remote
Desktop and xrdp, not against what Windows really sends. The gap was paid down by
hand, with a capture-driven loop, over one long campaign (2026-08): NSCodec did not
exist upstream and was ported from FreeRDP; ClearCodec had five wire-level bugs and
needed destination seeding; Progressive needed a dozen fixes ending in a tile
application rule — fresh passes blit whole, upgrades clip to the region rects — that
**no reference implementation has** (FreeRDP clips both, which is provably wrong for
fresh passes on a Windows stream; both halves were proven from captured bytes). All
of it is inventoried in
[`crates/adit-rdp/IRONRDP-PATCHES.md`](../crates/adit-rdp/IRONRDP-PATCHES.md), and the
forensic method is a runbook: [rdp-debugging.md](rdp-debugging.md).

**The sequencing mistake, on the record.** The tile path came first because IronRDP
ships no H.264 decoder — the day-one choice was "tile path or black screen", not
"tile path or AVC". But with hindsight the right second step was wiring H.264, not
polishing the tile path: Windows prefers AVC444, mstsc exercises the AVC pipeline —
the server's best-tested path — and most of the artefact campaign happened on a
fallback road that AVC sessions never drive. The tile work is not wasted — servers
without hardware encoders (GNOME Remote Desktop, xrdp, GPU-less VMs) require that
path and it is now solid — but its priority was inherited from a library limitation,
not chosen.

**Two remoting philosophies, for context.** RDP carries both: content-aware tile
codecs (Microsoft-proprietary, specced as MS-RDPEGFX / MS-RDPRFX / MS-RDPNSC) and
screen-as-video (open ITU standards, H.264/AVC444). The industry is converging on the
latter — one hardware-accelerated decoder instead of five interlocking software ones.

**Done (2026-08-05).** AVC landed the same day this entry was written, and cheaper
than planned: ironrdp-egfx ships a pluggable `H264Decoder` trait with a bundled
OpenH264 implementation (AVC420), and a new `avc444.rs` decodes AVC444 — two H.264
views recombined into per-surface YUV 4:4:4, kernels ported from FreeRDP. The
capability ladder is V10.7 (AVC444) → V8.1 (AVC420) → V8 (tiles), the server picks.
Windows' `AVC444ModePreferred` policy demands exactly the 444 path (it refuses an
AVC420-only client outright — observed live). Media Foundation remains the upgrade
route if hardware decode is ever wanted; the trait boundary makes the swap local.
Note: desktop Windows keeps ALL AVC off by default — the tile path still serves
every unconfigured host, which is most of them.

**Revisit if** IronRDP ships its own AVC decode + the codec fixes upstream (drop the
vendored patches), or the RustCrypto pin conflict (#4) dissolves and a single-binary
layout becomes possible.
