# Adit — local build & release tasks.
# Run `just` (or `just --list`) to see everything.
#
# Recipes run in PowerShell because the installer (Inno Setup) and the
# "copy into the installed app" step are Windows-specific. The cargo/git
# recipes are cross-platform regardless.

# PowerShell 7 (pwsh), NOT Windows PowerShell 5.1 — 5.1's Set-Content defaults to
# ANSI and would mangle the UTF-8 (em-dashes) in Cargo.toml on `bump`.
set windows-shell := ["pwsh", "-NoProfile", "-Command"]

# The RDP helper is a SEPARATE workspace (own Cargo.lock — see crates/adit-rdp);
# it must be built with its own manifest, not `-p`.
rdp := "crates/adit-rdp/Cargo.toml"

# Windows on ARM. Needs `rustup target add aarch64-pc-windows-msvc` once.
arm := "aarch64-pc-windows-msvc"

# Show the recipe list
default:
    @just --list

# ── build ──────────────────────────────────────────────────────────────

# Release-build the GUI app
build:
    cargo build --release -p adit-app

# Release-build the out-of-process RDP helper (its own workspace)
helper:
    cargo build --release --manifest-path {{rdp}}

# Release-build both shippable binaries
dist: build helper

# The arm64 recipes below cross-compile. CI builds arm64 NATIVELY on a
# windows-11-arm runner, and that is the path proven to work: the RDP helper
# exact-pins RustCrypto crates and carries three vendored IronRDP patches, none
# of which has been tried as a cross-build. If `helper-arm64` fails here that is
# the likely reason, and it says nothing about whether the release will build.

# Cross-compile the GUI app for Windows on ARM
build-arm64:
    cargo build --release -p adit-app --target {{arm}}

# Cross-compile the RDP helper for Windows on ARM
helper-arm64:
    cargo build --release --manifest-path {{rdp}} --target {{arm}}

# Cross-compile both shippable binaries for Windows on ARM
dist-arm64: build-arm64 helper-arm64

# Debug-build the whole workspace
build-debug:
    cargo build --workspace

# ── quality gates ──────────────────────────────────────────────────────

# Run all workspace tests
test:
    cargo test --workspace

# Lint exactly like CI — warnings are errors
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Auto-format the tree
fmt:
    cargo fmt --all

# The full CI gate locally (build + clippy + test + integration-compile).
#
# The last step compile-checks the real-sshd integration tests: they live behind
# `--features integration` and need Docker to RUN, so `cargo test --workspace` skips
# them entirely — which once let an SftpCommand field change compile locally yet break
# CI's integration job. `--no-run` catches that class of break without Docker (CI still
# runs them for real).
ci:
    cargo build --workspace
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
    cargo test -p adit-ssh --features integration --no-run

# ── run / deploy ───────────────────────────────────────────────────────

# Build then launch the app
run: build
    & 'target/release/adit-app.exe'

# Stop any running Adit so the release exes aren't locked
kill:
    Get-Process Adit,adit-app,adit-rdp-host -ErrorAction SilentlyContinue | Stop-Process -Force; Write-Output 'ok'

# Copy freshly-built binaries into the installed Adit (for quick local testing)
deploy: dist
    Copy-Item 'target/release/adit-app.exe' "$env:LOCALAPPDATA\Programs\Adit\Adit.exe" -Force
    Copy-Item 'crates/adit-rdp/target/release/adit-rdp-host.exe' "$env:LOCALAPPDATA\Programs\Adit\adit-rdp-host.exe" -Force
    Write-Output 'deployed to installed Adit'

# `dist` comes before `kill` deliberately: the build is the slow part, so doing
# it first keeps Adit usable until the moment its exe is actually replaced.
# `deploy` then finds `dist` already satisfied and only copies — and without
# `kill` in between, that copy fails on a locked exe.
#
# Start-Process rather than `&`, unlike `run`: this recipe exists for a
# look-tweak-look loop, so the shell should come back instead of blocking until
# Adit is closed.

# Replace the installed Adit with a fresh build and start it again
restart: dist kill deploy
    Start-Process "$env:LOCALAPPDATA\Programs\Adit\Adit.exe"
    Write-Output 'restarted'

# ── release ────────────────────────────────────────────────────────────
#
# There is deliberately no `release` recipe. Releasing is done entirely on CI and is
# triggered MANUALLY, straight from `gh` — never from `just` — so the trigger and the
# build aren't tangled together:
#
#     gh workflow run release.yml -f version=0.1.60
#
# That dispatches .github/workflows/release.yml, which bumps the three crate versions,
# runs the gate, builds both binaries, packages the installer, commits+tags the bump,
# and publishes the GitHub Release. Watch it with `gh run watch`.
#
# The recipes below build locally ONLY for smoke-testing — they never publish.

# Bump the version across the root workspace + both RDP crates (kept in lockstep).
# Reads/writes UTF-8 without a BOM explicitly so comments with non-ASCII (em-dashes)
# survive the round-trip regardless of the host PowerShell's default encoding.
# (The release workflow does its own bump on CI; this is for local testing.)
bump version:
    #!pwsh -NoProfile
    $enc = [System.Text.UTF8Encoding]::new($false)
    foreach ($f in 'Cargo.toml', '{{rdp}}', 'crates/adit-rdp-proto/Cargo.toml') {
        $text = [System.IO.File]::ReadAllText($f)
        $text = [regex]::Replace($text, '(?m)^version = ".*"', 'version = "{{version}}"')
        [System.IO.File]::WriteAllText((Resolve-Path $f), $text, $enc)
    }
    # Carry both lockfiles along. Rewriting the manifests alone leaves
    # Cargo.lock on the previous version, so the next build silently rewrites
    # it and every checkout starts dirty — that is exactly how v0.1.59 shipped
    # with a 0.1.58 lockfile. `-w` re-resolves only the workspace members, so
    # it never drags third-party dependencies forward.
    cargo update -w -q
    cargo update -w -q --manifest-path {{rdp}}
    Write-Output 'bumped to {{version}}'

# Depends on `dist` on purpose: ISCC packages whatever binaries it happens to find, so a
# standalone invocation once shipped a release full of STALE binaries after the build step
# had failed. Rebuilding first makes that impossible.
# Build the Inno Setup installer for VERSION (rebuilds the binaries first, local smoke-test only)
installer version: dist
    & "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe" "/DAppVersion={{version}}" installer\adit.iss

# /DBuildDir is what keeps this honest. Without it ISCC would package whatever
# sits in target\release — on an x64 machine that is the x64 build, and the
# result would be an installer labelled arm64 containing binaries that are not.

# Build the Windows-on-ARM installer for VERSION (cross-compiles first, local smoke-test only)
installer-arm64 version: dist-arm64
    & "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe" "/DAppVersion={{version}}" "/DArch=arm64" "/DBuildDir={{arm}}\release" installer\adit.iss
