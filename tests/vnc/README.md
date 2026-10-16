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
before every sample. Each sample runs five seconds. The matrix covers lossless/WAN
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
