# Native workbench validation

The D/E/F workbench uses the production iced view and dispatcher. The HTML in
`docs/ui-prototypes/2026-10-26/termius.html` is a design reference only.

## Gates

Run `just ci` for both workspaces' required checks. `just ci-vnc` includes that
gate and adds the opt-in VNC tests and vendored graphics decoder tests.
The native workbench example is compiled separately:

```powershell
cargo clippy -p adit-ui --example workbench_acceptance --features native-workbench-acceptance --locked -- -D warnings
cargo build -p adit-ui --example workbench_acceptance --features native-workbench-acceptance --locked
```

Formal CI retains the repository's dedicated Windows/Linux x64 runner labels.
This harness does not enable another platform, deploy an installer, or publish.

## Isolated screenshots

Use a fresh process and fresh paths for each run:

```powershell
$env:ADIT_CONFIG_DIR = Join-Path $env:TEMP ("adit-workbench-" + [guid]::NewGuid().ToString("N"))
$env:ADIT_WORKBENCH_SCREENSHOT_DIR = Join-Path $env:TEMP ("adit-captures-" + [guid]::NewGuid().ToString("N"))
$env:ADIT_WORKBENCH_VIEW = "hosts"  # hosts | terminal | missions
$env:ADIT_WORKBENCH_THEME = "dark"  # dark | light
$env:ADIT_WORKBENCH_SIZE = "1360x860"
$env:ADIT_WORKBENCH_SCALE = "1"     # application scale: 1 | 1.5 | 2
& .\target\debug\examples\workbench_acceptance.exe
```

The harness rejects an existing nonempty config/output directory. Config must
be beneath OS temp. It creates 12 explicitly synthetic mock sessions, performs
16 captures through `window::screenshot`, and writes `report.json`. It checks
session IDs, order and status across resource/focus/task navigation, sidebar
hiding, two separate task member sets, and focus/return. A mock PTY has no
remote process to redraw after a resize, so the harness repaints its labelled
synthetic terminal text after layout changes.

The report records OS, application and compositor scale separately. Application
scaling exercises native iced layout at that scale; it is not evidence that
Windows display settings were changed. Use independent OS/DPI and remote-peer
observations for system scaling and actual remote input.

Set `ADIT_WORKBENCH_INTERACTIVE=1` to keep the window open after a successful
sequence. Production runtime events then drive a restricted set of navigation,
focus and layout operations for OS pointer smoke checks. Transport creation,
credential operations, clipboard shortcuts, sync and production polling remain
disabled. Subsequent manual interactions are not added to the automated report.
Close the fixture window normally when finished.

## Regression boundaries

- `missions::tests` checks runtime membership, view/task/focus transitions,
  local input ownership, closed-session pruning, and shared layout origins.
- `desktop_panes::tests` checks contain mapping, letterboxes and rounding at
  multiple scales; independent frame caches; invalid/error eviction; and a
  loopback RFB peer's actual input packets, read-only and local-overlay guards.
- Existing terminal, VNC input, clipboard, reconnect and independent RDP helper
  checks remain applicable. Mock screenshots do not replace those tests.
- The first six open sessions are the limit of the existing tiling command;
  each mission can contain up to six chosen open sessions.
- Missions are runtime UI organization. Switching or removing a task does not
  close connections; restarting the app does not restore tasks.

Real SSH/SFTP/RDP/VNC hosts, IME composition, OS system-key capture, assistive
technology, Windows system DPI changes and physical NIC/sleep recovery need
their corresponding native or peer-observed evidence. In particular, these UI
checks do not resolve issues #205, #212 or #223.
