"""Calibrate one owned QEMU, then run actual UltraVNC through that same packet path."""
import argparse
import ctypes
import datetime
import hashlib
import struct
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

from direct_context import DirectGuest, SCOPE, validate_mode
from run import exchange, require


def file_hash(path):
    return hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()


def creation_filetime(pid):
    from ctypes import wintypes
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.GetProcessTimes.argtypes = [wintypes.HANDLE] + [ctypes.POINTER(wintypes.FILETIME)] * 4
    kernel.GetProcessTimes.restype = wintypes.BOOL
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    handle = kernel.OpenProcess(0x1000, False, pid)
    if not handle:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        values = [wintypes.FILETIME() for _ in range(4)]
        if not kernel.GetProcessTimes(handle, *(ctypes.byref(item) for item in values)):
            raise ctypes.WinError(ctypes.get_last_error())
        return (values[0].dwHighDateTime << 32) | values[0].dwLowDateTime
    finally:
        kernel.CloseHandle(handle)


class OwnedEcho:
    def __init__(self):
        self.stopped = threading.Event()
        self.lock = threading.Lock()
        self.clients = set()
        self.workers = []
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
        self.listener.bind(("127.0.0.1", 15911))
        self.listener.listen(8)
        self.listener.settimeout(0.2)
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def connection(self, client):
        try:
            with client:
                client.settimeout(20)
                deadline = time.monotonic() + 25
                while not self.stopped.is_set() and time.monotonic() < deadline:
                    chunk = client.recv(16384)
                    if not chunk:
                        return
                    client.sendall(chunk)
        except OSError:
            pass
        finally:
            with self.lock:
                self.clients.discard(client)

    def serve(self):
        while not self.stopped.is_set():
            try:
                client, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            with self.lock:
                self.clients.add(client)
                worker = threading.Thread(target=self.connection, args=(client,), daemon=True)
                self.workers.append(worker)
            worker.start()

    def close(self):
        self.stopped.set()
        self.listener.close()
        self.thread.join(timeout=1)
        with self.lock:
            clients = list(self.clients)
        for client in clients:
            try:
                client.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            client.close()
        deadline = time.monotonic() + 2
        for worker in self.workers:
            worker.join(timeout=max(0, deadline - time.monotonic()))
        require(not clients and not self.thread.is_alive() and all(not worker.is_alive() for worker in self.workers), "Calibration backend retained active clients or workers")


def loss_stall():
    # SLIRP accepts the host leg before the dropped guest SYN can complete.
    # Abort that owned leg after the negative observation to cancel its SYN.
    client = socket.socket()
    try:
        deadline = time.monotonic() + 2
        client.settimeout(2)
        client.connect(("127.0.0.1", 15912))
        client.sendall(b"adit-loss-negative-control")
        client.settimeout(max(0.001, deadline - time.monotonic()))
        payload = client.recv(1024)
        if not payload:
            raise ConnectionError("Unexpected early EOF during loss control")
    finally:
        client.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("HH", 1, 0))
        client.close()


def native_cleanup(out):
    query = r"""
    $ErrorActionPreference='Stop'
    $filter='LocalPort=15911 OR LocalPort=15912 OR LocalPort=15913 OR LocalPort=15914 OR RemotePort=15911 OR RemotePort=15912 OR RemotePort=15913 OR RemotePort=15914'
    $rows=@(Get-CimInstance -Namespace root/StandardCimv2 -ClassName MSFT_NetTCPConnection -Filter $filter -ErrorAction Stop)
    $records=@($rows | ForEach-Object { [ordered]@{local_address=[string]$_.LocalAddress;local_port=[int]$_.LocalPort;remote_address=[string]$_.RemoteAddress;remote_port=[int]$_.RemotePort;state=[int]$_.State;owning_process=[uint32]$_.OwningProcess} })
    [ordered]@{query_success=$true;source='MSFT_NetTCPConnection';filter=$filter;records=$records} | ConvertTo-Json -Depth 6 -Compress
    """
    try:
        process = subprocess.run(["pwsh", "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", query], capture_output=True, timeout=5, creationflags=subprocess.CREATE_NO_WINDOW)
    except subprocess.TimeoutExpired as error:
        (out / "controller-native-tcp.stdout.txt").write_bytes(error.stdout or b"")
        (out / "controller-native-tcp.stderr.txt").write_bytes(error.stderr or b"")
        raise
    (out / "controller-native-tcp.stdout.txt").write_bytes(process.stdout)
    (out / "controller-native-tcp.stderr.txt").write_bytes(process.stderr)
    require(process.returncode == 0, "Native owned TCP cleanup query failed")
    table = json.loads(process.stdout.decode("utf-8-sig"))
    require(table.get("query_success") is True and table.get("source") == "MSFT_NetTCPConnection" and isinstance(table.get("records"), list), "Invalid native cleanup evidence")
    require(all(row["state"] == 11 for row in table["records"]), "Owned controller/backend ports retain non-TIME_WAIT TCP state")
    return table


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
        loss_stall()
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
    records.append({"phase": "combined90ms5mbit0.5percent", "echo": combined, "median_delta_ms": combined_delta, "snapshot": guest.control()})
    require(combined_delta >= 100, "Combined-mode delay was not observed")
    guest.idle()
    return records


class SnapshotServer:
    def __init__(self, guest, controller_id):
        self.guest = guest
        self.controller_id = controller_id
        self.qemu_creation_filetime = creation_filetime(guest.process.pid)
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
                client.settimeout(15)
                try:
                    require(address[0] == "127.0.0.1", "Snapshot peer must be loopback")
                    request = json.loads(client.makefile("r", encoding="utf-8").readline(4096))
                    require(request == {"protocol": "adit-direct-netem-v1", "controller_id": self.controller_id, "command": "snapshot"}, "Invalid fixed snapshot request")
                    snapshot = self.guest.control()
                    reply = {"ok": True, "protocol": "adit-direct-netem-v1", "controller_id": self.controller_id, "source_sha": os.environ["GITHUB_SHA"], "run_id": os.environ["GITHUB_RUN_ID"], "qemu_pid": self.guest.process.pid, "qemu_creation_filetime": self.qemu_creation_filetime, "snapshot": snapshot}
                except Exception as error:
                    reply = {"ok": False, "error": str(error)}
                try:
                    client.sendall(json.dumps(reply).encode() + b"\n")
                except OSError:
                    pass

    def close(self):
        self.stopped.set()
        self.listener.close()
        self.thread.join(timeout=16)
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
    result = {"ok": False, "source_sha": os.environ["GITHUB_SHA"], "scope": SCOPE, "network_condition": {"guest_egress_delay_ms": 90, "guest_egress_aggregate_rate_mbit": 5, "configured_random_loss_percent": 0.5}, "quality": args.quality, "workload": args.workload, "requested_samples": args.samples}
    overall_deadline = time.monotonic() + 14 * 60
    echo = None
    snapshot_server = None
    try:
        with DirectGuest(args.qemu, args.guest, out) as guest:
            result["qemu_sha256"] = guest.sha256
            result["qemu_argv"] = guest.argv
            result["guest_manifest"] = guest.manifest
            result["probe_sha256"] = file_hash(args.probe)
            echo = OwnedEcho()
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
                evidence = {"protocol": "adit-direct-netem-v1", "source_sha": os.environ["GITHUB_SHA"], "run_id": os.environ["GITHUB_RUN_ID"], "mode": mode, "scope": SCOPE, "calibration_passed": True, "qemu_pid": guest.process.pid, "qemu_path": str(guest.executable), "qemu_sha256": guest.sha256, "qemu_creation_filetime": creation_filetime(guest.process.pid), "qemu_argv": guest.argv, "controller_pid": os.getpid(), "controller_path": sys.executable, "controller_sha256": file_hash(sys.executable), "controller_creation_filetime": creation_filetime(os.getpid()), "controller_script": str(pathlib.Path(__file__).resolve()), "controller_script_sha256": file_hash(__file__), "created_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(), "guest_manifest": guest.manifest, "probe_port": 15912, "vendor_port": 15911, "control_port": 15914, "controller_id": controller_id, "before": snapshot, "network_condition": result["network_condition"] if mode == "impaired" else {"guest_egress_delay_ms": 0, "guest_egress_aggregate_rate_mbit": None, "configured_random_loss_percent": 0}}
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
                if mode == "impaired" and samples >= 10:
                    require(delta > 0, "No actual qdisc loss observed during this ten-sample VNC cell")
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
        try:
            result["native_tcp_cleanup"] = native_cleanup(out)
        except Exception as native_error:
            result["ok"] = False
            result["native_cleanup_error"] = str(native_error)
            cleanup_error = cleanup_error or native_error
        (out / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        if cleanup_error is not None:
            raise RuntimeError("Owned controller cleanup failed") from cleanup_error


if __name__ == "__main__":
    main()
