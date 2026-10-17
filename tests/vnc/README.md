# Isolated VNC verification

These fixtures create disposable Linux desktops. They never connect to a production
server. All published ports bind to host loopback. `adit-ci` and
`adit-test-isolated` are public synthetic fixture passwords, not deployable secrets.
No screen or clipboard from the user's actual desktop is captured.

## SSH gateway and ordinary x11vnc

```powershell
docker build -t adit-vnc-integration:local tests/vnc
docker run -d --name adit-vnc-integration --memory 1g -p 127.0.0.1:15901:15901 -p 127.0.0.1:12222:22 adit-vnc-integration:local
cargo test -p adit-session --test vnc_gateway -- --ignored
```

The target VNC listener `127.0.0.1:15900` exists only inside the container. The test
first proves that the host cannot connect directly, then opens an SSH direct-tcpip
channel, handles the unknown-key trust conversation, authenticates VNC separately
and receives actual pixels. Wrong SSH password and rejected host key must fail.
It uses a temporary known-hosts path and TrustOnce, leaving normal Adit trust alone.

The gateway suite also starts a separate, unpublished TigerVNC display using
`start-gateway.sh`. It reads actual X11 selections for bidirectional Unicode,
observes input at the remote X11 window, and checks the actual SSH sockets after
closing or cancelling. A host listener trap proves a refused target cannot fall
back to a direct connection. The suite owns and cleans up only its separate display.

`crates/adit-ui/tests/vault_connections.rs` exercises a cold-loaded protected
credential store against SSH 12222 and TigerVNC 15903. It verifies migration
backups, locked/wrong-password behavior, then authenticates with the unlocked
secrets and reads SSH output and complete VNC pixels. Its RDP profile checks
persistence only; real RDP and native settings controls require separate acceptance.

## Reproducible Linux load matrix

The image contains Ubuntu 24.04's x11vnc/TigerVNC and the official TurboVNC 3.3.1
release payload. The Dockerfile pins the TurboVNC asset's SHA-256 from its
[official release](https://github.com/TurboVNC/turbovnc/releases/tag/3.3.1). Only the
payload is extracted; it is started directly as Xvnc, without installing a system
service in the container. No vendor binary is checked into this repository.

```powershell
docker run -d --name adit-vnc-matrix --memory 1g --cap-add NET_ADMIN -p 127.0.0.1:15902:15901 -p 127.0.0.1:15903:15902 -p 127.0.0.1:15904:15903 --entrypoint /bin/sh adit-vnc-integration:local /start-matrix.sh
docker exec adit-vnc-matrix sh -c 'ls /tmp/adit-ready-*'
cargo build -p adit-session --example vnc_probe
Copy-Item target/debug/examples/vnc_probe.exe $env:TEMP/adit-vnc-matrix-probe.exe
python tests/vnc/benchmark.py --probe $env:TEMP/adit-vnc-matrix-probe.exe --output tests/vnc/results/linux-matrix.json
```

Wait for all three readiness files (`99`, `100`, `101`). The probe is copied outside
Cargo's output directory so tests/builds can replace the original Windows executable
without sharing violations while the longer matrix runs.

The synthetic canvas is identical on all three servers and resets its viewport
before every sample. Each sample allows up to 15 seconds for first pixel coverage, then measures a fixed five seconds of steady workload. The matrix covers lossless/WAN
quality, static/scroll/drag workloads, and loopback versus container egress netem
`delay 180ms rate 5mbit loss 0.1% limit 10000`, with ten fresh connections per cell.
The injection is on the **container** eth0 only, not a host adapter. It is an egress
condition, not a claim that both directions have 180 ms delay or that measured
application RTT always equals the configured delay. TCP queues can make Fence RTT
larger during a large image transfer.

JSON records include every sample's exit code, connection/paint times, framebuffer
update bytes and metadata. A nonzero probe stops the run with evidence retained.
Report sample count, median and min/max, failures and their conditions. Ten samples
are insufficient for a reliable tail-latency P95. These are **debug** client builds,
headless session-layer samples, not a GPU presentation or subjective mouse-latency
benchmark. First complete RFB update may be metadata-only; initial pixel coverage
and UI generation sampling are separate measurements.

`vnc_probe` obtains passwords on stdin. Its default is read-only; scripted input
requires an explicit `--workload=scroll` or `--workload=drag` and accepts only
localhost/127.0.0.1. Do not pass a real password as argv/env or use scripted input on
an external host. Snapshot output is opt-in and should never be committed if it
contains real desktop content.

The per-profile **兼容无损 ZRLE** mode (probe flag `--zrle`) omits Tight and JPEG
quality advertisements while retaining ZRLE/CopyRect/Raw. Select it explicitly
when Tight/JPEG interoperability or the measured CPU/byte tradeoff is unsuitable.
It does not retry failed security negotiation or silently downgrade authentication.

## Verified VeNCrypt interoperability

`start-tls.sh` generates a one-day isolated CA and a leaf valid only for DNS
`localhost`. It starts TigerVNC X509Vnc and TurboVNC x509vnc without offering None or
ordinary VncAuth. Only the public CA is copied to the host; private keys stay in
the disposable container.

```powershell
docker run -d --name adit-vnc-tls --memory 512m -p 127.0.0.1:15905:15905 -p 127.0.0.1:15906:15906 --entrypoint /bin/sh adit-vnc-integration:local /start-tls.sh
docker cp adit-vnc-tls:/tmp/ca.pem $env:TEMP/adit-vnc-test-ca.pem
'adit-ci' | target/debug/examples/vnc_probe.exe localhost 15905 5 --tls-ca=$env:TEMP/adit-vnc-test-ca.pem
'adit-ci' | target/debug/examples/vnc_probe.exe localhost 15906 5 --tls-ca=$env:TEMP/adit-vnc-test-ca.pem
```

Both must report `security: "X509Vnc"`, complete pixel coverage and exit 0. Repeat
using host `127.0.0.1` with the same CA: it must fail target-name validation. Repeat
with host `localhost` and `--tls` only: it must fail with an unknown issuer. The
protocol `policy_flow` test additionally verifies expired certificates and proves
that the password callback is not invoked before certificate validation.

## Cleanup

After collecting evidence, remove only these named disposable containers:

```powershell
docker rm -f adit-vnc-integration adit-vnc-matrix adit-vnc-tls
```

Linux results do not establish Windows TightVNC/UltraVNC, UAC, locked-session or
service-mode compatibility. The Windows matrix requires its own isolated VM and
versioned results. Cross-platform CI is an additional compile/test gate.

## Real X11 clipboard exchange

The matrix container also supports an independent Tk selection owner. Copy the
control helper, then test TigerVNC (15903, display 100) and TurboVNC (15904, 101):

```powershell
docker cp tests/vnc/clipboard_target.py adit-vnc-matrix:/tmp/adit-clipboard-target.py
$env:ADIT_VNC_CLIP_PORT='15903'; $env:ADIT_VNC_CLIP_DISPLAY='100'
cargo test -p adit-session --test vnc_clipboard -- --ignored
$env:ADIT_VNC_CLIP_PORT='15904'; $env:ADIT_VNC_CLIP_DISPLAY='101'
cargo test -p adit-session --test vnc_clipboard -- --ignored
```

The assertions use actual X11 clipboard reads/writes for Chinese, emoji,
multiline Latin text, empty text, read-only receive and sharing disabled. Each
copy creates a fresh selection owner; before testing a new remote ownership
transition the client offers a synthetic marker. All text is synthetic. The
helper control pipe has a bounded read timeout and contains no user clipboard.

Ubuntu's x11vnc 0.9.16 fixture advertises Extended Clipboard through its
LibVNCServer dependency, but the current end-to-end Unicode test does **not** pass
against its X11 selection path. Capability negotiation alone is not acceptance.
Keep that combination open for compatibility investigation; do not count it as
a passed clipboard target.

The same suite now exercises an explicit per-profile Latin-1 compatibility mode
(without advertising Extended Clipboard), reads actual X11 STRING selections,
and requires a visible nonfatal notice for unrepresentable text. Both clipboard
tests serialize access to their one X11 clipboard. TigerVNC has passed this path.
The isolated x11vnc legacy path has now passed three consecutive Rust runs
after observing server-side X11 readiness. The fixture has no window manager:
`X11VNC_AVOID_WINDOWS=never` lets x11vnc create its selection window promptly,
and the runner separately waits for its initial `selection_send`. Between runs
it establishes a distinct ASCII selection owner and observes another send, so
x11vnc's previous-text cache does not suppress a repeated test transition.
These are fixture readiness checks; the Rust exchange timeout remains 10 seconds.

```powershell
cargo test -p adit-session --test vnc_clipboard --no-run
cargo build -p adit-session --example vnc_probe
# Select the executable printed by --no-run; its hash is recorded in the result.
python tests/vnc/legacy-clipboard.py --test-binary target/debug/deps/vnc_clipboard-<hash>.exe --probe target/debug/examples/vnc_probe.exe --output tests/vnc/results/x11vnc-rust-legacy-local.json --repeats 3
```

The runner owns only a new `adit-vnc-clipboard` container (:113, port 15913 bound
to loopback), refuses to replace an existing one, and removes it on exit. It
records exact executable/image/script hashes, readiness observations and all
test outputs. The recorded checkout base is separate from executable provenance:
reused binaries do not prove an exact build of that commit or acceptance of
concurrent worktree changes. Its extra shared RFB warmup connection is deliberately terminated
at cleanup; that exit code is not a failed acceptance sample. The
[accepted result](results/2026-10-17-x11vnc-rust-legacy.json) preserves a link to
the [earlier failed repeat](results/2026-10-17-x11vnc-rust-legacy-initial-failure.json).
Acceptance covers real Latin-1 café/newlines in both directions, a nonfatal
notice for unrepresentable Chinese, and a connected session with UTF-8 disabled.
It does **not** accept x11vnc Extended Clipboard or prove long-running reliability.

The upstream 0.9.16 [callback registration](https://github.com/LibVNC/x11vnc/blob/0.9.16/src/screen.c)
registers the old X11 cut-text hook. LibVNCServer 0.9.14 dispatches extended text
to a [separate UTF-8 callback](https://github.com/LibVNC/libvncserver/blob/LibVNCServer-0.9.14/libvncserver/rfbserver.c).
This explains why a capability advertisement alone is insufficient; it does not
prove the cause of the earlier legacy fixture-repeat failure.

## Shared resize acceptance

`start-resize.sh` starts a separate TigerVNC display on loopback-published port
15908 (rebuild the fixture image after changing its scripts). Keep it separate from the performance matrix: changing a benchmark
display would invalidate later samples. Run
`cargo test -p adit-session --test vnc_resize -- --ignored` against this fixture.
The test proves that default local scaling leaves the server size unchanged,
explicit remote resizing changes real pixels, and a second shared client follows
the change and can restore the size for the original client. Both views must
receive complete pixel coverage after each change; same-size requests retain a
usable frame and the outstanding refresh count stays bounded.

CPU supplement measurements use process user+kernel time (Windows GetProcessTimes
or Unix child resource usage), including initial transfer and the fixed steady
window. They are a separate run with their own runner hash; they are not UI/GPU
latency or FPS. Timeout cases preserve the output collected before terminating
the probe.

## Real cursor plane acceptance

`cursor_target.py` uses X11/Xcursor in a dedicated `adit-vnc-cursor` container,
with TigerVNC :111 published only at 127.0.0.1:15912. Copy the helper and
`start-cursor.sh` into that container, start the script, and wait for its Unix
control socket `/tmp/adit-vnc-cursor.sock`. Run
`cargo test -p adit-session --test vnc_cursor_real -- --ignored`.
The test observes actual arrow/I-beam/custom colour and transparency-mask changes,
nonzero hotspots, hidden shapes, uniform framebuffer isolation, and same-tab
reconnection. Tiny visible corner pixels preserve the custom image bounding box
against legitimate server cropping. Native DPI and toolbars still need UI acceptance.
The Ubuntu CI job runs this test and cleans up the owned container.

## Isolated Windows acceptance drivers

`crates/adit-session/examples/vnc_fixture_driver.rs` takes an explicitly selected
private IPv4 fixture, port and timeout as arguments. Its password is the first
stdin line; subsequent stdin lines are synthetic input commands (`WAIT`, `KEY`,
`CLIP`, `CLICK`, `READONLY`, `SNAP`, `CLOSE`). Never supply real credentials as
commands or arguments. Snapshot output belongs in an explicitly selected private
local path, never the repository. Only synthetic `ADIT_VNC_TEST:` clipboard
markers are reported. Waiting for complete initial pixel coverage does not
guarantee a later login/paste transition has settled: verify focus and allow
Windows clipboard paste to finish before submitting a command.

`crates/adit-ui/examples/vault_rdp_probe.rs` takes private IPv4, username and an
optional port; the password is a single stdin line, and `ADIT_RDP_HOST` selects
the separately built RDP helper. It creates an isolated encrypted v2 vault with
a fresh master, cold-loads it, verifies locked/wrong-master behavior, unlocks,
and uses the saved password in the production SessionManager RDP connection
path. Acceptance requires a complete 1280x800 desktop, then vault relock and
session closure. Its unique scratch directory is cleaned on normal exit. It
never reads the user's profiles or credential store. This is a headless
authentication check; settings dialogs and normal interactive connection flows
require separate native UI acceptance. Closing RDP can lock the fixture console,
so serialize it with VNC lock/UAC tests on that same VM.

TightVNC 2.8.90 vendor UTF-8 requires Tight security type 16 and all matching
directional TGHT capabilities; see [wire provenance](../../crates/adit-vnc/UPSTREAM.md).
The security envelope itself does not encrypt the desktop or clipboard. The
explicit legacy option disables both automatic UTF-8 extensions; it must not be
used as a workaround that silently sends Unicode bytes as classic Latin-1.

## Isolated native viewer acceptance

The opt-in `native-vnc-acceptance` feature builds the `vnc_native_acceptance`
example. It uses the shipping Adit view and dispatcher, an isolated temporary
profile directory, and an explicitly unauthenticated **synthetic loopback-only**
target on port 15915. Clipboard send/receive, synchronization and auto-reconnect
are disabled. Do not repoint this entry point at a user desktop or credential
store. It is not included in the default app build.

Start a dedicated fixture container from this test image, publish only
`127.0.0.1:15915:15915`, copy `start-native.sh`, `native_target.py` and
`cursor_target.py` into its root, and execute `sh /start-native.sh`. The target
draws labelled crosshairs and reports server-side click coordinates; its root
geometry query follows real RFB resizes instead of relying on Tk's initial
screen-size cache. Build and launch the development example explicitly:

```powershell
cargo build -p adit-ui --features native-vnc-acceptance --example vnc_native_acceptance
```

Use native application automation only for ordinary canvas/control interactions.
The bottom test controls vary **application scale**, leaving Windows OS DPI
unchanged. Do not report app scale 100/150/200% as an OS DPI acceptance matrix.
The example records only synthetic pointer/window/geometry metadata to
`events.jsonl` under its isolated temporary directory. Compare each observed
crosshair click with the target's `/tmp/native-events.jsonl`, and exclude user
interference or unsuccessful input attempts from passed cases.

The [2026-10-17 native result](results/2026-10-17-windows-native-scale.json)
records six actual Windows-native application-scale/fit combinations at an
unchanged **125% OS DPI**, all within two remote pixels of a labelled target.
It also observes explicit remote resize, native maximize/restore and preserved
coordinate mapping. It does not accept rapid border dragging, the three-value
OS DPI matrix, shipping toolbar boundary cursor restoration, or native IME.

## Actual isolated Linux client interface recovery

The Ubuntu CI job builds `vnc_recovery_probe` and runs `network-recovery.sh`.
For a dedicated Linux test machine with the fixture dependencies already
installed, provide a fresh result directory:

```sh
cargo build -p adit-session --example vnc_recovery_probe --locked
timeout --signal=TERM --kill-after=10s 180s bash tests/vnc/network-recovery.sh target/debug/examples/vnc_recovery_probe /tmp/adit-vnc-network-result-unique
```

The runner creates its own uniquely named client network namespace and two
veth pairs. A real TigerVNC display starts with a red background. After the
production SessionManager receives complete pixels, the runner takes the
client's first interface down, waits for its disconnected/reconnecting state,
and switches the route to a second interface with a different source address.
Acceptance requires one unchanged tab, old pixels withdrawn, a complete new
blue desktop, and retained VncAuth, ZRLE and read-only policy. A further
20-second `SIGSTOP`/`SIGCONT` cycle requires a fresh green desktop afterward.
Only these owned namespace/interface/display resources are cleaned; previous
result directories are never overwritten. CI retains logs, phases, route,
binary hash and a success/failure result as `vnc-network-recovery` artifacts.
Build and execution have separate CI steps. X11 commands and the outer execution
have deadlines; runner phases, server and client logs are written directly into
the result directory so a blocked cleanup cannot hide the last observation.
The first cancelled run is retained as
[`2026-10-17-network-recovery-initial-incomplete.json`](results/2026-10-17-network-recovery-initial-incomplete.json);
it establishes no recovery acceptance.
The next run reached both recovered frame phases but timed out waiting for the
wrapper to exit; its [raw evidence](results/2026-10-17-network-wrapper-timeout.json)
is also retained. Sudo can suspend its supervisor when its child stops, including
the [non-PTY path](https://github.com/sudo-project/sudo/blob/SUDO_1_9_15p5/src/exec_nopty.c#L709-L725).
The runner therefore resumes both its namespace client and its own sudo wrapper,
and records the wrapper's state before resuming it. This changes only test
process supervision, not production reconnect behavior.

This is a real Linux virtual-interface change, not a mocked socket error. The
process pause is additional evidence; it is not OS suspend/hibernate. Neither
case establishes Windows Wi-Fi hardware, Windows sleep or native GUI recovery.
Those remain separately tracked in #212. Fixture credentials are disposable
public constants, never a real target password in argv or environment.
