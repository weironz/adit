#!/usr/bin/env python3
"""xrdp file-clipboard acceptance for unlocked offers.

Drives `rdp_probe` (and through it the real `adit-rdp-host` helper) against a
local xrdp, puts files on the session's own X clipboard with `xclip` as the
session user, and reads them back through the helper's range requests with
`PULL`. Three cases: an unchanged offer must arrive byte for byte; a file that
grew or shrank after being offered must fail rather than arrive truncated.

The password is read from stdin and handed to `rdp_probe` on its stdin. It
never reaches argv or the environment.

Usage: drive.py --probe P --helper H --user U --out DIR < password-file
"""
import argparse
import hashlib
import os
import queue
import re
import shutil
import subprocess
import sys
import threading
import time


def sh_user(user, cmd, **kw):
    return subprocess.run(["sudo", "-u", user, "sh", "-c", cmd], check=True, **kw)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--probe", required=True)
    ap.add_argument("--helper", required=True)
    ap.add_argument("--host", default="127.0.0.1:3389")
    ap.add_argument("--user", required=True)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()
    password = sys.stdin.readline().rstrip("\r\n")
    log = open(os.path.join(args.out, "probe.txt"), "w")
    probe = subprocess.Popen(
        [args.probe, args.helper, args.host, args.user, "240", "--fixture-report"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        env={**os.environ, "RUST_LOG": "adit_rdp=info,adit_rdp::clipboard=debug"},
    )
    probe.stdin.write(password + "\n")
    probe.stdin.flush()
    lines = queue.Queue()

    def pump():
        for line in probe.stdout:
            log.write(line)
            log.flush()
            lines.put(line.rstrip("\n"))
        lines.put(None)

    threading.Thread(target=pump, daemon=True).start()

    def wait_for(pattern, timeout=60):
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                line = lines.get(timeout=1)
            except queue.Empty:
                continue
            if line is None:
                raise RuntimeError("probe exited")
            if re.search(pattern, line):
                return line
        raise RuntimeError(f"timed out waiting for {pattern}")

    def send(cmd):
        probe.stdin.write(cmd + "\n")
        probe.stdin.flush()

    wait_for(r"^first_tile_ms=")
    ps = subprocess.run(["ps", "-u", args.user, "-o", "args="], capture_output=True, text=True).stdout
    display = re.search(r"Xorg (:\d+)", ps).group(1)
    home = f"/home/{args.user}/adit-clip"
    sh_user(args.user, f"rm -rf {home} && mkdir -p {home}")
    xclip = None

    def offer(names):
        nonlocal xclip
        if xclip:
            xclip.kill()
        uris = "".join(f"file://{home}/{n}\r\n" for n in names)
        xclip = subprocess.Popen(
            ["sudo", "-u", args.user, "env", f"DISPLAY={display}",
             "xclip", "-quiet", "-selection", "clipboard", "-t", "text/uri-list"],
            stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        xclip.stdin.write(uris.encode())
        xclip.stdin.close()
        return wait_for(rf"^files_offered .* count={len(names)} ")

    def pull(case):
        dest = os.path.join("/tmp", f"adit-pull-{case}")
        shutil.rmtree(dest, ignore_errors=True)
        os.makedirs(dest)
        send(f"PULL {dest}")
        end = wait_for(r"^(pull_done|file_read_failed)")
        return dest, end

    def sha(path):
        return hashlib.sha256(open(path, "rb").read()).hexdigest()

    def server_sha(path):
        out = subprocess.run(["sudo", "-u", args.user, "sha256sum", path],
                             capture_output=True, text=True, check=True).stdout
        return out.split()[0]

    results = []
    # 1. Unchanged: multi-chunk with a ragged tail, exactly one chunk, empty.
    sh_user(args.user, f"head -c 200001 /dev/urandom > {home}/a.bin && "
            f"head -c 65536 /dev/urandom > {home}/b.bin && : > {home}/empty.txt")
    print(offer(["a.bin", "b.bin", "empty.txt"]))
    dest, end = pull("unchanged")
    ok = end == "pull_done" and all(
        sha(os.path.join(dest, n)) == server_sha(f"{home}/{n}") for n in ["a.bin", "b.bin", "empty.txt"])
    for n in ["a.bin", "b.bin", "empty.txt"]:
        print(f"{n} client={sha(os.path.join(dest, n))} server={server_sha(f'{home}/{n}')}")
    results.append(("unchanged", end, "PASS" if ok else "FAIL"))
    # 2. Grew after the offer: every range fills, only the SIZE check can see it.
    sh_user(args.user, f"head -c 150000 /dev/urandom > {home}/grew.bin")
    print(offer(["grew.bin"]))
    sh_user(args.user, f"head -c 10 /dev/urandom >> {home}/grew.bin")
    dest, end = pull("grew")
    leaked = os.path.exists(os.path.join(dest, "grew.bin"))
    results.append(("grew", end, "PASS" if end.startswith("file_read_failed") and not leaked else "FAIL"))
    # 3. Shrank after the offer: the final range comes back short.
    sh_user(args.user, f"head -c 150000 /dev/urandom > {home}/shrank.bin")
    print(offer(["shrank.bin"]))
    sh_user(args.user, f"truncate -s 100000 {home}/shrank.bin")
    dest, end = pull("shrank")
    leaked = os.path.exists(os.path.join(dest, "shrank.bin"))
    results.append(("shrank", end, "PASS" if end.startswith("file_read_failed") and not leaked else "FAIL"))

    if xclip:
        xclip.kill()
    send("CLOSE")
    probe.wait(timeout=30)
    with open(os.path.join(args.out, "results.txt"), "w") as f:
        for case, end, verdict in results:
            line = f"{case}\t{verdict}\t{end}"
            print(line)
            f.write(line + "\n")
    return 0 if all(r[2] == "PASS" for r in results) else 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    finally:
        # A failed run must not leave the probe holding the session.
        subprocess.run(["pkill", "-x", "rdp_probe"], check=False)
