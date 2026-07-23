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

# The full CI gate locally (mirrors .github/workflows/ci.yml's build job)
#
# The last step compile-checks the real-sshd integration tests. They live behind
# `--features integration` and need Docker to RUN, so `cargo test --workspace`
# skips them entirely — which once let an SftpCommand field change compile locally
# yet break CI's integration job. `--no-run` catches that class of break without
# Docker; CI still runs them for real.
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

# ── release ────────────────────────────────────────────────────────────

# Bump the version across the root workspace + both RDP crates (kept in lockstep).
# Reads/writes UTF-8 without a BOM explicitly so comments with non-ASCII (em-dashes)
# survive the round-trip regardless of the host PowerShell's default encoding.
bump version:
    #!pwsh -NoProfile
    $enc = [System.Text.UTF8Encoding]::new($false)
    foreach ($f in 'Cargo.toml', '{{rdp}}', 'crates/adit-rdp-proto/Cargo.toml') {
        $text = [System.IO.File]::ReadAllText($f)
        $text = [regex]::Replace($text, '(?m)^version = ".*"', 'version = "{{version}}"')
        [System.IO.File]::WriteAllText((Resolve-Path $f), $text, $enc)
    }
    Write-Output 'bumped to {{version}}'

# Depends on `dist` on purpose: ISCC packages whatever binaries it happens to find, so a
# standalone invocation once shipped a release full of STALE binaries after the build step
# had failed. Rebuilding first makes that impossible.
# Build the Inno Setup installer for VERSION (rebuilds the binaries first)
installer version: dist
    & "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe" "/DAppVersion={{version}}" installer\adit.iss

# (Notes are auto-generated from commits; edit them on GitHub afterwards if needed.)
# Cut a patch release end-to-end: gate, bump, build, package, tag, push, publish, deploy.
# Gates on the FULL `ci` recipe (not just `test`) so a feature-gated break — like an
# integration test that doesn't compile — is caught before publishing, not after.
# NOTE: this still publishes local artifacts without waiting for GitHub Actions to go
# green; `ci` here is the local mirror of it (minus the Docker runtime step).
release version: ci kill (bump version) (installer version)
    git add -A
    git commit -m "chore: release v{{version}}"
    git tag v{{version}}
    git push origin main --tags
    gh release create v{{version}} target/release/adit-installer-v{{version}}.exe --title "Adit v{{version}}" --generate-notes
    just deploy
    Write-Output 'released v{{version}}'
