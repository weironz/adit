"""Calibrate one owned QEMU, then run actual UltraVNC through that same packet path."""
import argparse
import json
import os
import pathlib
import socket
import statistics
import subprocess
import threading
import time
import uuid
import sys

sys.dont_write_bytecode = True

from qemu_context import QemuGuest, SCOPE, validate_mode
from run import echo_service, exchange, require


def calibrate(guest, records):
    guest.control("baseline")
    baseline = [exchange(b"adit-independent-calibration") for _ in range(5)]
    payload = bytes(range(256)) * 1024
    baseline_bulk = exchange(payload)
    records.append({"phase": "baseline", "echo": baseline, "bulk": baseline_bulk})
    guest.idle()
    guest.control("delay")
    delayed = [exchange(b"adit-independent-calibration") for _ in range(5)]
    delta = statistics.median(item["elapsed_ms"] for item in delayed) - statistics.median(item["elapsed_ms"] for item in baseline)
    records.append({"phase": "delay80ms", "echo": delayed, "median_delta_ms": delta})
    require(delta >= 100, "Delay calibration was not observed")
    guest.idle()
    guest.control("rate")
    bulk = exchange(payload, deadline=18)
    records.append({"phase": "rate2mbit", "bulk": bulk})
    require(bulk["elapsed_ms"] >= 1500 and bulk["elapsed_ms"] >= baseline_bulk["elapsed_ms"] + 500, "Rate calibration was not observed")
    guest.idle()
    guest.control("drop")
    start = time.monotonic()
    try:
        exchange(b"must-not-bypass-loss", deadline=2)
    except (TimeoutError, OSError) as error:
        elapsed = (time.monotonic() - start) * 1000
        records.append({"phase": "100_percent_loss", "expected_failure": str(error), "elapsed_ms": elapsed})
        require(elapsed >= 1500, "Immediate refusal is not a packet-loss stall")
    else:
        raise RuntimeError("100% loss was bypassed")
    dropped = guest.control()
    require(dropped["stats"]["drops"] > 0, "No actual qdisc drops during negative control")
    records.append({"phase": "100_percent_loss_snapshot", "snapshot": dropped})
    guest.idle()
    guest.control("baseline")
    records.append({"phase": "loss_control_restored", "echo": exchange(b"restored-after-loss")})
    guest.idle()
    guest.control("impaired")
    combined = [exchange(b"same-qemu-combined-calibration") for _ in range(5)]
    combined_delta = statistics.median(item["elapsed_ms"] for item in combined) - statistics.median(item["elapsed_ms"] for item in baseline)
    records.append({"phase": "combined90ms5mbit0.1percent", "echo": combined, "median_delta_ms": combined_delta, "snapshot": guest.control()})
    require(combined_delta >= 100, "Combined-mode delay was not observed")
    guest.idle()
    return records


class SnapshotServer:
    def __init__(self, guest, controller_id):
        self.guest = guest
        self.controller_id = controller_id
        self.stopped = threading.Event()
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
        self.listener.bind(("127.0.0.1", 15914))
        self.listener.listen(2)
        self.listener.settimeout(0.5)
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        while not self.stopped.is_set():
            try:
                client, address = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            with client:
                client.settimeout(25)
                try:
                    require(address[0] == "127.0.0.1", "Snapshot peer must be loopback")
                    request = json.loads(client.makefile("r", encoding="utf-8").readline(4096))
                    require(request == {"protocol": "adit-guest-netem-v1", "controller_id": self.controller_id, "command": "snapshot"}, "Invalid fixed snapshot request")
                    snapshot = self.guest.control()
                    reply = {"ok": True, "source_sha": os.environ["GITHUB_SHA"], "qemu_pid": self.guest.process.pid, "snapshot": snapshot}
                except Exception as error:
                    reply = {"ok": False, "error": str(error)}
                try:
                    client.sendall(json.dumps(reply).encode() + b"\n")
                except OSError:
                    pass

    def close(self):
        self.stopped.set()
        self.listener.close()
        self.thread.join(timeout=26)
        require(not self.thread.is_alive(), "Owned snapshot worker failed to stop")


def main():
    parser = argparse.ArgumentParser()
    for name in ("qemu", "guest", "results", "probe"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--samples", type=int, choices=range(1, 11), default=1)
    parser.add_argument("--quality", choices=("lossless", "wan"), default="lossless")
    parser.add_argument("--workload", choices=("static", "scroll", "drag"), default="static")
    args = parser.parse_args()
    require(os.name == "nt" and os.getenv("GITHUB_ACTIONS") == "true" and os.getenv("RUNNER_ENVIRONMENT") == "github-hosted" and os.getenv("RUNNER_OS") == "Windows", "Disposable Windows CI only")
    out = pathlib.Path(args.results).resolve()
    require(out.is_relative_to(pathlib.Path(os.environ["RUNNER_TEMP"]).resolve()) and not out.exists(), "Fresh absolute runner-temp results required")
    out.mkdir()
    result = {"ok": False, "source_sha": os.environ["GITHUB_SHA"], "scope": SCOPE, "network_condition": {"guest_egress_delay_ms": 90, "guest_egress_aggregate_rate_mbit": 5, "configured_random_loss_percent": 0.1}, "quality": args.quality, "workload": args.workload, "requested_samples": args.samples}
    overall_deadline = time.monotonic() + 14 * 60
    echo = None
    snapshot_server = None
    try:
        with QemuGuest(args.qemu, args.guest, out) as guest:
            result["qemu_sha256"] = guest.sha256
            result["qemu_argv"] = guest.argv
            echo = socket.socket()
            echo.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
            echo.bind(("127.0.0.1", 15911))
            echo.listen(8)
            threading.Thread(target=echo_service, args=(echo,), daemon=True).start()
            result["calibration"] = []
            calibrate(guest, result["calibration"])
            echo.close()
            echo = None
            # No echo traffic or queued bytes may leak into VNC drop counters.
            guest.idle()
            guest.control("impaired")
            controller_id = uuid.uuid4().hex
            snapshot_server = SnapshotServer(guest, controller_id)
            smoke = pathlib.Path(__file__).resolve().parent.parent / "windows-server-smoke.ps1"

            def vendor(phase, mode, samples, quality, workload):
                guest.idle()
                snapshot = guest.control(mode)
                validate_mode(snapshot, mode)
                evidence = {"protocol": "adit-guest-netem-v1", "source_sha": os.environ["GITHUB_SHA"], "run_id": os.environ["GITHUB_RUN_ID"], "mode": mode, "scope": SCOPE, "calibration_passed": True, "qemu_pid": guest.process.pid, "qemu_path": str(guest.executable), "qemu_sha256": guest.sha256, "guest_manifest": guest.manifest, "probe_port": 15912, "vendor_port": 15911, "control_port": 15914, "controller_id": controller_id, "before": snapshot, "network_condition": result["network_condition"] if mode == "impaired" else {"guest_egress_delay_ms": 0, "guest_egress_aggregate_rate_mbit": None, "configured_random_loss_percent": 0}}
                evidence_path = out / f"{phase}.network.json"
                evidence_path.write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
                target = out / ("vnc-" + phase)
                command = ["pwsh", "-NoProfile", "-File", str(smoke), "-Probe", str(pathlib.Path(args.probe).resolve()), "-Results", str(target), "-Samples", str(samples), "-Quality", quality, "-Workload", workload, "-SyntheticLoad", "-ProbePort", "15912", "-GuestNetworkEvidence", str(evidence_path)]
                with (out / f"{phase}.stdout.txt").open("w", encoding="utf-8") as stdout, (out / f"{phase}.stderr.txt").open("w", encoding="utf-8") as stderr:
                    try:
                        process = subprocess.Popen(command, stdout=stdout, stderr=stderr, creationflags=subprocess.CREATE_NO_WINDOW)
                        try:
                            process.wait(timeout=max(1, min(150 + 40 * (samples + 1), overall_deadline - time.monotonic() - 30)))
                        except subprocess.TimeoutExpired:
                            subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"], stdout=stdout, stderr=stderr, timeout=15, check=True, creationflags=subprocess.CREATE_NO_WINDOW)
                            process.wait(timeout=5)
                            raise
                    finally:
                        # Capture failure counters before ever changing qdisc.
                        try:
                            result[phase + "_after"] = guest.control()
                        except Exception as snapshot_error:
                            result[phase + "_after_error"] = str(snapshot_error)
                require(process.returncode == 0, f"Real VNC {phase}/{mode} failed; raw logs retained")
                require(phase + "_after" in result, "Missing actual post-VNC network counters")
                data = json.loads((target / "result.json").read_text(encoding="utf-8-sig"))
                require(data["outcome"] == "passed" and len(data["samples"]) == samples + 1, "Vendor fixture did not complete every requested sample and wrong-auth assertion")
                require(all(item["passed"] for item in data["samples"]), "A VNC sample did not pass")
                after = result[phase + "_after"]["stats"]
                delta = after["drops"] - snapshot["stats"]["drops"]
                result[phase + "_summary"] = {"mode": mode, "quality": quality, "workload": workload, "normal_samples": samples, "wrong_password_rejections": 1, "qdisc_drop_delta": delta, "tcp_retrans_seg_delta": after["tcp"]["RetransSegs"] - snapshot["stats"]["tcp"]["RetransSegs"], "loss_observed": delta > 0, "loss_note": "Actual guest qdisc drops observed" if delta > 0 else "No qdisc drops observed in this VNC phase; if loss was configured, do not claim observed random packet loss"}
                guest.idle()

            vendor("baseline-control", "baseline", args.samples, args.quality, args.workload)
            vendor("impaired", "impaired", args.samples, args.quality, args.workload)
            vendor("restored", "baseline", 1, "lossless", "static")
            snapshot_server.close()
            snapshot_server = None
            result["ok"] = True
    except Exception as error:
        result["ok"] = False
        result["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        cleanup_error = None
        try:
            if echo is not None:
                echo.close()
            if snapshot_server is not None:
                snapshot_server.close()
        except Exception as error:
            cleanup_error = error
            result["ok"] = False
            result["cleanup_error"] = str(error)
        (out / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        if cleanup_error is not None:
            raise RuntimeError("Owned controller cleanup failed") from cleanup_error


if __name__ == "__main__":
    main()
