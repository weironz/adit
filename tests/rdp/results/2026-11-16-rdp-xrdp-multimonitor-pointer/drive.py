#!/usr/bin/env python3
"""Server-side half of the #244 pointer acceptance.

Drives `rdp_probe` (and through it the real `adit-rdp-host` helper) against an
RDP server with a multi-monitor layout, sends `MOVE x y` for each target, and
after each move asks an *oracle* where the server itself believes the pointer
is. The oracle is any command that prints `X Y` — on xrdp it is `xdotool`
inside the session's own X server; on Windows it is the session's
`GetCursorPos`. Nothing here trusts the client: the expected value is the wire
coordinate that was sent, the observed value is the server's.

The password is read from stdin and handed to `rdp_probe` on its stdin. It
never reaches argv or the environment.

Usage:
  drive.py --probe P --helper H --host host:port --user U \
      --monitors 1280x800,1280x800 --oracle "cmd ..." --out results.jsonl \
      TARGET... < password-file

TARGET is `label=x,y`.
"""
import argparse
import json
import queue
import subprocess
import sys
import threading
import time


def read_oracle(cmd):
    out = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=10)
    if out.returncode != 0:
        raise RuntimeError(f"oracle failed: {out.stderr.strip()}")
    x, y = out.stdout.split()[:2]
    return int(x), int(y)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--probe", required=True)
    ap.add_argument("--helper", required=True)
    ap.add_argument("--host", required=True)
    ap.add_argument("--user", required=True)
    ap.add_argument("--domain")
    ap.add_argument("--monitors", required=True)
    ap.add_argument("--oracle", required=True)
    ap.add_argument("--layout-oracle", help="command whose output records the server's monitor topology")
    ap.add_argument(
        "--relayout",
        help="LAYOUT argument sent once connected (e.g. 1280x800@150,1280x800@150), "
        "for what only a post-connect layout can set, such as a desktop scale factor",
    )
    ap.add_argument("--out", required=True)
    ap.add_argument("--settle-timeout", type=float, default=3.0)
    ap.add_argument("targets", nargs="+")
    args = ap.parse_args()

    password = sys.stdin.readline().rstrip("\r\n")
    targets = []
    for t in args.targets:
        label, xy = t.split("=")
        x, y = xy.split(",")
        targets.append((label, int(x), int(y)))

    argv = [args.probe, args.helper, args.host, args.user, "300", "--monitors", args.monitors]
    if args.domain:
        argv += ["--domain", args.domain]
    probe = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1)
    probe.stdin.write(password + "\n")
    probe.stdin.flush()
    del password

    lines = queue.Queue()

    def pump():
        for line in probe.stdout:
            lines.put(line.rstrip("\n"))
        lines.put(None)

    threading.Thread(target=pump, daemon=True).start()
    log = []

    def wait_for(prefix, timeout):
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                line = lines.get(timeout=0.2)
            except queue.Empty:
                continue
            if line is None:
                raise RuntimeError("probe exited")
            print(f"probe: {line}", flush=True)
            log.append(line)
            if line.startswith(prefix):
                return line
        raise RuntimeError(f"timed out waiting for {prefix}")

    def send(cmd):
        probe.stdin.write(cmd + "\n")
        probe.stdin.flush()

    records = []
    failures = 0
    try:
        wait_for("connected=", 60)
        # The topology report can trail the first Connected message.
        send("WAIT 1500")
        send("ACK settled")
        wait_for("command_ack=settled", 30)
        if args.relayout:
            send(f"LAYOUT {args.relayout}")
            result = wait_for("layout_result", 30)
            records.append({"kind": "relayout", "request": args.relayout, "result": result})
            # The server applies a new layout (and any DPI change) after it
            # answers; give it time before reading topology.
            send("WAIT 3000")
            send("ACK relaid")
            wait_for("command_ack=relaid", 30)
        if args.layout_oracle:
            topo = subprocess.run(args.layout_oracle, shell=True, capture_output=True, text=True, timeout=10)
            print("server topology:\n" + topo.stdout, flush=True)
            records.append({"kind": "server_topology", "output": topo.stdout.strip()})
        for i, (label, x, y) in enumerate(targets):
            before = read_oracle(args.oracle)
            send(f"MOVE {x} {y}")
            token = f"t{i}"
            send(f"ACK {token}")
            wait_for(f"command_ack={token}", 30)
            sent_at = time.time()
            observed = read_oracle(args.oracle)
            while observed != (x, y) and time.time() - sent_at < args.settle_timeout:
                time.sleep(0.05)
                observed = read_oracle(args.oracle)
            ok = observed == (x, y)
            failures += not ok
            rec = {
                "kind": "pointer",
                "label": label,
                "wire": [x, y],
                "server_before": list(before),
                "server_after": list(observed),
                "match": ok,
                "settle_ms": round((time.time() - sent_at) * 1000),
            }
            records.append(rec)
            print(json.dumps(rec), flush=True)
        send("CLOSE")
    finally:
        try:
            probe.wait(timeout=10)
        except subprocess.TimeoutExpired:
            probe.kill()
        while True:
            try:
                line = lines.get_nowait()
            except queue.Empty:
                break
            if line is not None:
                log.append(line)
        with open(args.out, "w") as f:
            f.write(json.dumps({"kind": "run", "monitors": args.monitors, "host": args.host}) + "\n")
            for line in log:
                f.write(json.dumps({"kind": "probe", "line": line}) + "\n")
            for rec in records:
                f.write(json.dumps(rec) + "\n")
    matched = sum(r.get("match", False) for r in records if r["kind"] == "pointer")
    total = sum(1 for r in records if r["kind"] == "pointer")
    print(f"RESULT {matched}/{total} matched", flush=True)
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
