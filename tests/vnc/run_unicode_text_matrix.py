#!/usr/bin/env python3
"""Host-side short Unicode consumption matrix against an owned Docker fixture.

Requires an existing locally built vnc_fixture_driver and start-matrix.sh
container with loopback-published ports. Never builds code or starts a server.
Uses public isolated adit-ci authentication over stdin; no CLIP command exists
in this runner. Failures stay in per-case JSON/stdout/stderr and do not stop
the remaining independent cases.
"""
import argparse
import hashlib
import json
from pathlib import Path
import queue
import re
import subprocess
import threading
import time
import uuid

from unicode_text_target import scalars


CASES = (("ascii", "abc"), ("bmp-cjk", "\u4e2d\u6587"),
         ("nonbmp-emoji", "\U0001f60a"), ("combining", "e\u0301"))
DEFAULT_SERVERS = ("x11vnc:15902:99", "tigervnc:15903:100", "turbovnc:15904:101")
VERSIONS = {"x11vnc": ["x11vnc", "-version"], "tigervnc": ["Xtigervnc", "-version"],
            "turbovnc": ["/opt/TurboVNC/bin/Xvnc", "-version"]}


def write_json(path, value):
    path.write_text(json.dumps(value, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")


def run(command, timeout=15, check=True):
    result = subprocess.run(command, capture_output=True, text=True, encoding="utf-8",
                            errors="replace", timeout=timeout)
    if check and result.returncode:
        raise RuntimeError(f"command failed ({result.returncode}): {result.stderr[-2000:]}")
    return result


def read_lines(stream, output, ready):
    try:
        for line in stream:
            output.append(line)
            if line.startswith("ready_ms="):
                ready.set()
    finally:
        stream.close()


def evaluate(before, after, expected, driver_exit, ready):
    checks = {"driver_ready": ready, "driver_exit_zero": driver_exit == 0,
              "empty_before": before["text"] == "",
              "same_target_pid": isinstance(before.get("pid"), int) and before["pid"] == after.get("pid"),
              "focus_before": before["focus"]["focused"],
              "focus_after": after["focus"]["focused"],
              "no_focus_loss": after["counts"]["focus_losses"] == 0,
              "all_input_focused": after["counts"]["unfocused_input"] == 0,
              "key_events_consumed": after["counts"]["press"] > 0 and after["counts"]["release"] > 0,
              "no_event_truncation": after["counts"]["truncated_events"] == 0,
              "exact_scalars": after["scalars"] == scalars(expected)}
    return checks, all(checks.values())


def case(args, target, control, expected, directory):
    directory.mkdir()
    record = {"outcome": "failed", "expected": expected,
              "expected_scalars": scalars(expected), "clipboard_send_used": False}
    process = None
    stdout, stderr, readers = [], [], []
    ready = threading.Event()
    def query(command):
        return json.loads(run(["docker", "exec", args.container, "python3", target,
                               "--control", control, "--command", command]).stdout)
    try:
        process = subprocess.Popen([str(args.driver), "127.0.0.1", str(args.port), "25"],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   text=True, encoding="utf-8", errors="strict")
        for stream, output in ((process.stdout, stdout), (process.stderr, stderr)):
            reader = threading.Thread(target=read_lines, args=(stream, output, ready), daemon=True)
            reader.start()
            readers.append(reader)
        process.stdin.write("adit-ci\n")
        process.stdin.flush()
        if not ready.wait(15):
            raise RuntimeError("driver never reported complete-frame ready")
        before = query("RESET")
        record["before"] = before
        if not before["focus"]["focused"] or before["text"]:
            raise RuntimeError("target not empty/focused; no TEXT sent")
        # TEXT traverses production Unicode input. No expected value crosses
        # the target control channel, and no clipboard/paste path is used.
        commands = f"TEXT {expected}\nWAIT 1500\nCLOSE\n"
        record["commands"] = commands.splitlines()
        process.stdin.write(commands)
        process.stdin.flush()
        process.stdin.close()
        process.wait(timeout=20)
        after = query("STATUS")
        record["after"] = after
        record["driver_exit"] = process.returncode
        record["checks"], passed = evaluate(before, after, expected, process.returncode, ready.is_set())
        record["outcome"] = "passed" if passed else "failed"
    except Exception as error:
        record["error"] = str(error)
    finally:
        if process and process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        for reader in readers:
            reader.join(timeout=3)
        (directory / "driver.stdout.txt").write_text("".join(stdout), encoding="utf-8")
        (directory / "driver.stderr.txt").write_text("".join(stderr), encoding="utf-8")
        write_json(directory / "result.json", record)
    return record


def server(args, specification, directory):
    name, port, display = specification.split(":")
    if name not in VERSIONS or not port.isdigit() or not display.isdigit():
        raise ValueError("server must be x11vnc|tigervnc|turbovnc:host-port:display")
    args.port = int(port)
    directory.mkdir()
    token = uuid.uuid4().hex
    target = f"/tmp/adit-unicode-{token}.py"
    control = f"/tmp/adit-unicode-{token}.sock"
    source = Path(__file__).with_name("unicode_text_target.py")
    record = {"outcome": "failed", "server": name, "host_port": args.port,
              "display": int(display), "cases": [], "cleanup_verified": False}
    process = None
    lines, errors, pid = [], [], None
    ready_queue = queue.Queue()
    threads = []
    try:
        # The start-matrix container uses these service ports internally.
        internal_port = {"x11vnc": 15901, "tigervnc": 15902, "turbovnc": 15903}[name]
        published = run(["docker", "port", args.container, f"{internal_port}/tcp"]).stdout.splitlines()
        record["published"] = published
        if published != [f"127.0.0.1:{args.port}"]:
            raise RuntimeError("requires exactly one host-loopback publication for this server")
        version = run(["docker", "exec", args.container, *VERSIONS[name]], check=False)
        record["version"] = {"stdout": version.stdout, "stderr": version.stderr,
                             "exit": version.returncode}
        if not version.stdout.strip() and not version.stderr.strip():
            raise RuntimeError("server version could not be recorded")
        run(["docker", "cp", str(source), f"{args.container}:{target}"])
        process = subprocess.Popen(["docker", "exec", args.container, "python3", "-u", target,
                                    "--display", display, "--control", control, "--seconds", "180"],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                                   encoding="utf-8", errors="replace")
        def target_reader():
            for line in process.stdout:
                lines.append(line)
                try:
                    payload = json.loads(line)
                    if payload.get("ready"):
                        ready_queue.put(payload)
                except (ValueError, AttributeError):
                    pass
        for worker in (target_reader, lambda: errors.extend(process.stderr.readlines())):
            thread = threading.Thread(target=worker, daemon=True)
            thread.start()
            threads.append(thread)
        try:
            identity = ready_queue.get(timeout=15)
        except queue.Empty as error:
            raise RuntimeError("Tk Entry target readiness deadline expired; see target.stderr.txt") from error
        pid = identity["pid"]
        record["target_identity"] = identity
        for name_case, expected in CASES:
            record["cases"].append(case(args, target, control, expected, directory / name_case))
    except Exception as error:
        record["error"] = str(error)
    finally:
        cleanup = {"quit": False, "fallback_termination": False}
        try:
            stopped = run(["docker", "exec", args.container, "python3", target,
                           "--control", control, "--command", "QUIT"], check=False)
            cleanup["quit"] = stopped.returncode == 0
            if process:
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    pass
            # A stopped docker CLI does not guarantee its container child died.
            # Identify this exact random script/control pair before SIGTERM.
            script = "import os,sys,signal,time; from pathlib import Path; target,control=sys.argv[1:3]; pids=[]\nfor p in Path('/proc').iterdir():\n if p.name.isdigit() and int(p.name)!=os.getpid():\n  try:\n   cmd=(p/'cmdline').read_bytes().split(b'\\0')\n   if target.encode() in cmd and control.encode() in cmd and b'--command' not in cmd: pids.append(int(p.name))\n  except (FileNotFoundError,PermissionError,ProcessLookupError): pass\nfor pid in pids: os.kill(pid,signal.SIGTERM)\ntime.sleep(.3)\nremaining=[pid for pid in pids if Path('/proc',str(pid)).exists()]\nif remaining: raise RuntimeError('owned target did not exit: '+str(remaining))\nfor path in (target,control): Path(path).unlink(missing_ok=True)\nprint(len(pids))"
            verified = run(["docker", "exec", args.container, "python3", "-c", script, target, control])
            cleanup["fallback_termination"] = verified.stdout.strip() != "0"
            record["cleanup_verified"] = True
        except Exception as error:
            cleanup["error"] = str(error)
        if process and process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        for thread in threads:
            thread.join(timeout=3)
        record["cleanup"] = cleanup
        (directory / "target.stdout.txt").write_text("".join(lines), encoding="utf-8")
        (directory / "target.stderr.txt").write_text("".join(errors), encoding="utf-8")
        if len(record["cases"]) == len(CASES) and all(item["outcome"] == "passed" for item in record["cases"]) and record["cleanup_verified"] and "error" not in record:
            record["outcome"] = "passed"
        write_json(directory / "result.json", record)
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--container", required=True)
    parser.add_argument("--driver", type=Path, required=True)
    parser.add_argument("--driver-source", required=True, help="declared exact source SHA; driver hash is independently recorded")
    parser.add_argument("--results", type=Path, required=True)
    parser.add_argument("--server", action="append", help="name:host-port:display; defaults match README's start-matrix publication")
    args = parser.parse_args()
    if not re.fullmatch(r"[a-fA-F0-9]{40}", args.driver_source):
        parser.error("--driver-source must be a full SHA, not an unverified current checkout label")
    args.driver = args.driver.resolve(strict=True)
    args.results.mkdir(parents=True, exist_ok=False)
    record = {"outcome": "failed", "scope": "production TEXT to real Linux Tk Entry; no native IME or physical keyboard acceptance",
              "driver_sha256": hashlib.sha256(args.driver.read_bytes()).hexdigest(),
              "driver_source_declared": args.driver_source, "clipboard_send_used": False,
              "target_sha256": hashlib.sha256(Path(__file__).with_name("unicode_text_target.py").read_bytes()).hexdigest(),
              "servers": []}
    try:
        # Select metadata at Docker's boundary; never retrieve Config.Env.
        identity_format = '{"id":{{json .Id}},"image":{{json .Image}},"created":{{json .Created}},"name":{{json .Name}}}'
        record["container"] = json.loads(run(["docker", "inspect", "--format", identity_format, args.container]).stdout)
        record["container"]["os"] = json.loads(run([
            "docker", "exec", args.container, "python3", "-c",
            "import json,platform; print(json.dumps({'release':platform.freedesktop_os_release(),'platform':platform.platform(),'python':platform.python_version()}))"
        ]).stdout)
        for index, specification in enumerate(args.server or DEFAULT_SERVERS):
            record["servers"].append(server(args, specification, args.results / f"server-{index}"))
        if record["servers"] and all(item["outcome"] == "passed" for item in record["servers"]):
            record["outcome"] = "passed"
    except Exception as error:
        record["error"] = str(error)
    finally:
        write_json(args.results / "result.json", record)
    print(json.dumps({"outcome": record["outcome"], "results": str(args.results)}, ensure_ascii=True))
    return 0 if record["outcome"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
