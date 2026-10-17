"""Run the existing Rust Latin-1 acceptance only after x11vnc selection readiness.

Creates and removes one named disposable container; never changes matrix servers.
All text/passwords are public synthetic fixture constants. No GPL code is copied.
"""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import queue
import subprocess
import threading
import time

ROOT = Path(__file__).resolve().parents[2]
NAME = "adit-vnc-clipboard"
PORT = 15913
DISPLAY = "113"


def docker(*args, check=True):
    return subprocess.run(["docker", *args], check=check, text=True, encoding="utf-8", errors="replace", capture_output=True)


def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def wait_for(predicate, description, timeout=35):
    start = time.monotonic()
    while time.monotonic() - start < timeout:
        if predicate():
            return round(time.monotonic() - start, 3)
        time.sleep(0.1)
    raise RuntimeError("fixture readiness timeout: " + description)


def log():
    return docker("exec", NAME, "cat", "/tmp/clipboard-x11vnc.log", check=False).stdout


def run(args):
    evidence = {
        "date": datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=8))).date().isoformat(),
        "scope": "Rust production SessionManager against actual x11vnc X11 STRING clipboard; public synthetic fixture text only",
        "source_state": {
            "checkout_base_sha": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
            "binary_build_source_sha": "unverified",
            "binary_provenance": "Reused existing executables identified by their separate SHA-256 hashes; the checkout base is not an exact-build claim.",
            "current_worktree_changes_covered": False,
            "scope": "Clipboard acceptance only; does not verify the concurrent reconnect changes.",
        },
        "test_binary_sha256": sha(args.test_binary), "probe_sha256": sha(args.probe),
        "fixture_sha256": {name: sha(ROOT / "tests/vnc" / name) for name in ["legacy-clipboard.py", "start-clipboard.sh", "clipboard_target.py"]},
        "fixture": {"container": NAME, "display": DISPLAY, "port": PORT, "host_binding": "127.0.0.1", "memory": "512m", "window_manager": False, "X11VNC_AVOID_WINDOWS": "never"},
        "extended_utf8_accepted": False, "attempts": [],
        "previous_failed_fixture_attempt": "2026-10-17-x11vnc-rust-legacy-initial-failure.json",
    }
    owned = False
    warmup = None
    helper = None
    result_code = 1
    try:
        if docker("inspect", NAME, check=False).returncode == 0:
            raise RuntimeError("refusing to replace an existing named fixture container")
        docker("create", "--name", NAME, "--memory", "512m", "-p", f"127.0.0.1:{PORT}:{PORT}", "--entrypoint", "/bin/sh", args.image, "-c", "while :; do sleep 3600; done")
        owned = True
        docker("start", NAME)
        for source, target in [("start-clipboard.sh", "/tmp/adit-start-clipboard.sh"), ("clipboard_target.py", "/tmp/adit-clipboard-target.py")]:
            docker("cp", str(ROOT / "tests/vnc" / source), NAME + ":" + target)
        docker("exec", "-d", NAME, "sh", "/tmp/adit-start-clipboard.sh")
        wait_for(lambda: "Listening for VNC connections" in log(), "VNC listener")
        versions = docker("exec", NAME, "dpkg-query", "-W", "-f", "${Package} ${Version}\n", "x11vnc", "libvncserver1").stdout
        evidence["versions"] = versions.splitlines()
        evidence["image_id"] = docker("inspect", NAME, "--format", "{{.Image}}").stdout.strip()
        warmup = subprocess.Popen([str(Path(args.probe).resolve()), "127.0.0.1", str(PORT), "90", "--zrle"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, encoding="utf-8", errors="replace")
        warmup.stdin.write("adit-ci\n"); warmup.stdin.close()
        # Keep a real shared RFB connection alive while x11vnc initializes selwin.
        evidence["selwin_wait_seconds"] = wait_for(lambda: "created selwin" in log(), "x11vnc created selwin")
        helper = subprocess.Popen(["docker", "exec", "-i", NAME, "python3", "-u", "/tmp/adit-clipboard-target.py", DISPLAY], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="replace")
        replies = queue.Queue()
        def reader():
            for line in helper.stdout:
                replies.put(line.rstrip("\r\n"))
        threading.Thread(target=reader, daemon=True).start()
        if replies.get(timeout=5) != "READY": raise RuntimeError("Tk selection owner not ready")
        helper.stdin.write("SET " + "synthetic readiness marker".encode().hex() + "\n"); helper.stdin.flush()
        if replies.get(timeout=5) != "OK": raise RuntimeError("Tk could not claim X11 clipboard")
        # The independent initial selection-send delay is also observed, rather
        # than hidden by increasing the Rust test's 10-second exchange timeout.
        evidence["initial_selection_send_wait_seconds"] = wait_for(lambda: "selection_send: send-ing it" in log(), "first X11 selection transmission")
        environment = dict(os.environ, ADIT_VNC_CLIP_CONTAINER=NAME, ADIT_VNC_CLIP_PORT=str(PORT), ADIT_VNC_CLIP_DISPLAY=DISPLAY, CARGO_BUILD_JOBS="1")
        environment.pop("ADIT_VNC_CLIP_LOCAL", None)
        evidence["between_samples_selection_ready_seconds"] = []
        for index in range(args.repeats):
            if index > 0:
                # x11vnc caches the last remote X11 selection. The fixed test
                # deliberately reuses the same café text; establish a distinct
                # actual remote ownership before repeating its transition.
                count = log().count("selection_send: send-ing it")
                marker = "synthetic repeat readiness marker " + str(index)
                helper.stdin.write("SET " + marker.encode().hex() + "\n"); helper.stdin.flush()
                if replies.get(timeout=5) != "OK": raise RuntimeError("Tk repeat owner not ready")
                elapsed = wait_for(lambda: log().count("selection_send: send-ing it") > count, "new X11 ownership transmitted between samples")
                evidence["between_samples_selection_ready_seconds"].append(elapsed)
            sample = subprocess.run([str(Path(args.test_binary).resolve()), "real_legacy_clipboard_uses_latin1_and_reports_unsupported_text", "--ignored", "--nocapture", "--test-threads=1"], cwd=ROOT, env=environment, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=45)
            evidence["attempts"].append({"index": index+1, "returncode": sample.returncode, "stdout": sample.stdout, "stderr": sample.stderr})
            if sample.returncode: break
        result_code = 0 if len(evidence["attempts"]) == args.repeats and all(s["returncode"] == 0 for s in evidence["attempts"]) else 1
        evidence["legacy_accepted"] = result_code == 0
        evidence["assertions"] = ["real client-to-X11 Latin-1 café plus newline", "real X11-to-client Latin-1 café plus newline", "unrepresentable Chinese produces a nonfatal visible notice without changing X11 text", "UTF-8 remains unnegotiated and session remains connected"]
    except Exception as exc:
        evidence["error"] = str(exc)
        evidence["legacy_accepted"] = False
    finally:
        if helper is not None:
            try:
                helper.stdin.write("QUIT\n"); helper.stdin.flush(); helper.wait(timeout=5)
            except Exception:
                helper.kill(); helper.wait()
        if warmup is not None:
            evidence["warmup_terminated_by_runner"] = warmup.poll() is None
            if evidence["warmup_terminated_by_runner"]: warmup.terminate()
            try: warmup.wait(timeout=5)
            except subprocess.TimeoutExpired: warmup.kill(); warmup.wait()
            evidence["warmup_returncode"] = warmup.returncode
            evidence["warmup_stdout"] = warmup.stdout.read()
        if owned:
            events = log()
            evidence["readiness_events"] = [line for line in events.splitlines() if "created selwin" in line or "selection_send: send-ing it" in line]
            removed = docker("rm", "-f", NAME, check=False)
            evidence["cleanup_returncode"] = removed.returncode
        output = Path(args.output)
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(json.dumps(evidence, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    return result_code


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--test-binary", required=True)
    parser.add_argument("--probe", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--image", default="adit-vnc-integration:local")
    parser.add_argument("--repeats", type=int, default=3)
    options = parser.parse_args()
    if options.repeats < 1:
        parser.error("--repeats must be positive")
    raise SystemExit(run(options))
