"""Bounded Windows QEMU context for the calibrated direct guest host-alias path."""
import hashlib
import json
import os
from pathlib import Path
import queue
import select
import socket
import subprocess
import threading
import time

from direct_guest import MODES, state_from_results, state_is_idle

SCOPE = "Linux guest eth0 aggregate egress impairment across TCP-terminating SLIRP and per-connection byte relay; restrict=off, /32 host-alias route, no default/DNS/IPv6. Not transparent Windows TCP loss, physical NIC, OS sleep or VNC acceptance."
QEMU_SHA256 = "47d57a6072e0bb3bd98f87926eb129eb1736dfe818c67b3b81ef7ce4edd0b3cd"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def summarize(response):
    return state_from_results(response["results"], response["active_connections"])


def validate_mode(snapshot, mode):
    require(mode in MODES, "Unknown actual mode in control response")
    netem = [item for item in snapshot["stats"]["qdiscs"] if item["kind"] == "netem"]
    if mode == "baseline":
        require(not netem, "Baseline still has netem installed")
        return
    require(len(netem) == 1, "Expected one actual netem qdisc")
    options = netem[0]["options"]
    actual_delay = options.get("delay", {}).get("delay", 0)
    actual_rate = options.get("rate", {}).get("rate", 0)
    actual_loss = options.get("loss-random", {}).get("loss", 0)
    expected = {"delay": (0.08, 0, 0), "rate": (0, 250000, 0),
                "drop": (0, 0, 1), "impaired": (0.09, 625000, 0.005)}[mode]
    require(options.get("limit") == 20000
            and abs(actual_delay - expected[0]) < 0.000001
            and actual_rate == expected[1]
            and abs(actual_loss - expected[2]) < 0.000001,
            "Actual tc options differ from the fixed requested guest-egress mode")


class DirectGuest:
    MAX_SECONDS = 15 * 60

    def __init__(self, qemu, guest, out):
        require(os.name == "nt" and os.getenv("GITHUB_ACTIONS") == "true"
                and os.getenv("RUNNER_ENVIRONMENT") == "github-hosted"
                and os.getenv("RUNNER_OS") == "Windows", "Disposable Windows GitHub runner only")
        runner = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
        self.out = Path(out).resolve()
        require(self.out.is_relative_to(runner) and self.out != runner, "Context output must be private runner-temp")
        if self.out.exists():
            require(self.out.is_dir() and not any(self.out.iterdir()), "Fresh empty context output required")
        else:
            self.out.mkdir()
        self.process = None
        self.serial = None
        self.listener = None
        self.reader = None
        self.stderr = None
        self.watchdog = None
        self.argv = []
        self.executable = Path(qemu).resolve(strict=True)
        self.guest = Path(guest).resolve(strict=True)
        self.manifest = {}
        self.sha256 = None
        self.events = []
        self.messages = queue.Queue()
        self.lock = threading.Lock()
        self.closing = threading.Event()
        self.expired = threading.Event()
        self.sequence = 0
        self.closed = False
        self.deadline = time.monotonic() + self.MAX_SECONDS
        self.record = {"ok": False, "phase": "verify_sources", "source_sha": os.environ["GITHUB_SHA"],
                       "run_id": os.environ.get("GITHUB_RUN_ID"), "scope": SCOPE,
                       "context_max_seconds": self.MAX_SECONDS}
        self._save()
        try:
            require(self.executable == runner / "adit-netem-direct-tools" / "extracted" / "qemu-system-x86_64.exe", "Fixed fresh QEMU extraction required")
            require(self.guest.is_relative_to(runner) and self.guest != runner, "Guest artifact must be runner-temp")
            self.manifest = json.loads((self.guest / "manifest.json").read_text())
            require(self.manifest["source_sha"] == os.environ["GITHUB_SHA"]
                    and self.manifest.get("build_mode") == "direct-host-alias-fake"
                    and self.manifest.get("additional_fixed_packages") == ["iproute2-minimal=6.15.0-r0"],
                    "Exact current direct guest source/mode/tool package required")
            for name in ("vmlinuz-virt", "initramfs.cpio.gz", "modules.dep"):
                require(hashlib.sha256((self.guest / name).read_bytes()).hexdigest() == self.manifest["files"][name], "Guest boot artifact hash mismatch: " + name)
            for source, installed in (("direct-init", "init"), ("direct_guest.py", "guest.py")):
                data = Path(__file__).with_name(source).read_bytes().replace(b"\r\n", b"\n")
                require(hashlib.sha256(data).hexdigest() == self.manifest["guest_source_files"][installed], "Guest script source mismatch: " + source)
            policy = self.manifest["network_policy"]
            require(policy["address"] == "10.0.2.15/32" and policy["only_unicast_route"] == "10.0.2.2/32 dev eth0"
                    and policy["default_route"] is False and policy["dns_configured"] is False
                    and policy["qemu_restrict"] is False, "Exact narrowed guest route policy required")
            self.sha256 = hashlib.sha256(self.executable.read_bytes()).hexdigest()
            require(self.sha256 == QEMU_SHA256, "Exact pinned QEMU executable required")
            self.record.update(qemu_path=str(self.executable), qemu_sha256=self.sha256, guest_manifest=self.manifest)
            self._save()
        except Exception as error:
            self.record.update(phase="source_verification_failed", error=f"{type(error).__name__}: {error}")
            self._save()
            raise

    def _save(self):
        (self.out / "context.json").write_text(json.dumps(self.record, indent=2) + "\n", encoding="utf-8")

    def _expire(self):
        self.expired.set()
        record = {"error": "Fixed context lifetime expired", "seconds": self.MAX_SECONDS}
        try:
            if self.process is not None and self.process.poll() is None:
                self.process.kill()
        except OSError as error:
            record["termination_error"] = str(error)
        finally:
            (self.out / "context-deadline.json").write_text(json.dumps(record) + "\n", encoding="utf-8")
            self.messages.put({"serial_reader_error": "Context lifetime expired"})

    def _read(self):
        pending = bytearray()
        try:
            with (self.out / "serial.txt").open("wb") as log:
                while not self.closing.is_set():
                    if not select.select([self.serial], [], [], 0.2)[0]:
                        continue
                    data = self.serial.recv(65536)
                    if not data:
                        raise ConnectionError("Guest serial reached EOF")
                    log.write(data)
                    log.flush()
                    pending.extend(data)
                    require(len(pending) <= 262144, "Serial line exceeds bounded control protocol size")
                    while b"\n" in pending:
                        line, _, rest = pending.partition(b"\n")
                        pending = bytearray(rest)
                        if line.startswith(b"ADIT_DIRECT "):
                            message = json.loads(line[len(b"ADIT_DIRECT "):].decode("utf-8"))
                            self.events.append(message)
                            self.messages.put(message)
        except (OSError, ValueError, RuntimeError) as error:
            if not self.closing.is_set():
                self.messages.put({"serial_reader_error": f"{type(error).__name__}: {error}"})

    def __enter__(self):
        try:
            firmware = list(self.executable.parent.rglob("bios-256k.bin"))
            require(len(firmware) == 1, "Unique pinned firmware required")
            self.listener = socket.socket()
            self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
            self.listener.bind(("127.0.0.1", 15913))
            self.listener.listen(1)
            self.listener.settimeout(10)
            with socket.socket() as reserved:
                reserved.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
                reserved.bind(("127.0.0.1", 15912))
            self.argv = [str(self.executable), "-L", str(firmware[0].parent), "-accel", "tcg,thread=multi", "-machine", "q35", "-cpu", "max", "-smp", "2", "-m", "512", "-display", "none", "-monitor", "none", "-chardev", "socket,id=control,host=127.0.0.1,port=15913", "-serial", "chardev:control", "-no-reboot", "-kernel", str(self.guest / "vmlinuz-virt"), "-initrd", str(self.guest / "initramfs.cpio.gz"), "-append", "console=ttyS0 rdinit=/init panic=-1 ipv6.disable=1", "-nic", "user,model=e1000,restrict=off,ipv6=off,hostfwd=tcp:127.0.0.1:15912-10.0.2.15:15912"]
            self.record.update(phase="boot", argv=self.argv)
            self._save()
            self.stderr = (self.out / "qemu-stderr.txt").open("wb")
            self.watchdog = threading.Timer(max(0.01, self.deadline - time.monotonic()), self._expire)
            self.watchdog.daemon = True
            self.watchdog.start()
            self.process = subprocess.Popen(self.argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                            stderr=self.stderr, creationflags=subprocess.CREATE_NO_WINDOW)
            self.record["qemu_pid"] = self.process.pid
            self.serial, peer = self.listener.accept()
            require(peer[0] == "127.0.0.1", "Serial must use loopback")
            self.listener.close()
            self.serial.setblocking(False)
            self.reader = threading.Thread(target=self._read, daemon=True)
            self.reader.start()
            ready = self.wait(lambda item: item.get("event") == "ready", 30)
            require(ready.get("control_protocol") == "adit-direct-netem-v1"
                    and ready["address"] == "10.0.2.15/32" and ready["restrict"] is False
                    and ready["no_default_route"] is True and ready["dns_configured"] is False
                    and ready["ipv6_routes_empty"] is True and ready["relay_lifetime_seconds"] == 120,
                    "Actual guest ready policy/control protocol differs")
            self.record.update(phase="ready", ready=ready)
            self._save()
            return self
        except Exception as error:
            self.record.update(error=f"{type(error).__name__}: {error}", failure_phase=self.record["phase"])
            try:
                self.close()
            except Exception:
                # close has preserved its own raw failure. Keep the original
                # boot/serial failure as the exception the caller receives.
                pass
            raise

    def wait(self, predicate, seconds):
        end = min(self.deadline, time.monotonic() + seconds)
        while time.monotonic() < end:
            require(not self.expired.is_set() and self.process.poll() is None, "Owned QEMU stopped or context lifetime expired")
            try:
                response = self.messages.get(timeout=min(0.2, max(0.01, end - time.monotonic())))
            except queue.Empty:
                continue
            require("serial_reader_error" not in response, "Serial failed: " + str(response))
            if predicate(response):
                return response
        raise TimeoutError("Guest serial command/context deadline")

    def _send(self, payload, seconds):
        end = min(self.deadline, time.monotonic() + seconds)
        while payload:
            remaining = end - time.monotonic()
            require(remaining > 0, "Serial write deadline")
            if select.select([], [self.serial], [], min(0.2, remaining))[1]:
                try:
                    count = self.serial.send(payload)
                except BlockingIOError:
                    continue
                require(count > 0, "Serial closed during write")
                payload = payload[count:]

    def _control(self, mode, seconds):
        require(mode is None or mode in MODES, "Unknown fixed network mode")
        started = time.monotonic()
        require(self.lock.acquire(timeout=max(0.001, min(seconds, self.deadline - started))), "Control lock deadline")
        try:
            self.sequence += 1
            request = {"id": self.sequence}
            if mode is not None:
                request["mode"] = mode
            self.record["phase"] = "control:" + (mode or "snapshot")
            self._save()
            self._send(json.dumps(request).encode() + b"\n", min(3, seconds))
            response = self.wait(lambda item: item.get("id") == self.sequence, max(0.01, seconds - (time.monotonic() - started)))
            with (self.out / "controls.jsonl").open("a", encoding="utf-8") as log:
                log.write(json.dumps({"request": request, "response": response, "utc_unix": time.time()}) + "\n")
            require(response.get("ok"), "Guest control failed: " + str(response))
            snapshot = {"raw": response, "stats": summarize(response)}
            require(mode is None or response["mode"] == mode, "Guest applied a different fixed mode")
            validate_mode(snapshot, response["mode"])
            return snapshot
        finally:
            self.lock.release()

    def control(self, mode=None):
        return self._control(mode, min(25, self.deadline - time.monotonic()))

    def idle(self):
        end = min(self.deadline, time.monotonic() + 12)
        while time.monotonic() < end:
            snapshot = self._control(None, end - time.monotonic())
            if state_is_idle(snapshot["stats"]):
                return snapshot
            time.sleep(min(0.2, max(0, end - time.monotonic())))
        raise TimeoutError("Relay count, kernel TCP states or qdisc queue did not become idle")

    def close(self):
        if self.closed:
            return
        self.closed = True
        self.record["phase"] = "cleanup"
        errors = []
        if self.process is not None:
            try:
                if self.process.poll() is None:
                    self.process.kill()
                self.process.wait(timeout=3)
                self.record["qemu_stopped"] = self.process.poll() is not None
            except Exception as error:
                errors.append("QEMU: " + str(error))
        self.closing.set()
        if self.serial is not None:
            try:
                self.serial.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            self.serial.close()
        if self.listener is not None:
            self.listener.close()
        if self.reader is not None:
            self.reader.join(timeout=1)
            self.record["serial_reader_stopped"] = not self.reader.is_alive()
            if self.reader.is_alive():
                errors.append("Serial reader did not stop")
        if self.stderr is not None:
            self.stderr.close()
        native_query = r"""
        $ErrorActionPreference='Stop'
        $filter='LocalPort=15912 OR LocalPort=15913 OR RemotePort=15912 OR RemotePort=15913'
        $rows=@(Get-CimInstance -Namespace root/StandardCimv2 -ClassName MSFT_NetTCPConnection -Filter $filter -ErrorAction Stop)
        $records=@($rows | ForEach-Object { [ordered]@{local_address=[string]$_.LocalAddress;local_port=[int]$_.LocalPort;remote_address=[string]$_.RemoteAddress;remote_port=[int]$_.RemotePort;state=[int]$_.State;owning_process=[uint32]$_.OwningProcess} })
        [ordered]@{query_success=$true;source='MSFT_NetTCPConnection';filter=$filter;records=$records} | ConvertTo-Json -Depth 6 -Compress
        """
        try:
            native = subprocess.run(["pwsh.exe", "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", native_query],
                                    capture_output=True, timeout=5, check=False, creationflags=subprocess.CREATE_NO_WINDOW)
            (self.out / "native-tcp.stdout.txt").write_bytes(native.stdout)
            (self.out / "native-tcp.stderr.txt").write_bytes(native.stderr)
            require(native.returncode == 0, "Native TCP table query failed")
            table = json.loads(native.stdout.decode("utf-8-sig"))
            self.record["native_tcp_cleanup"] = table
            require(table.get("query_success") is True and table.get("source") == "MSFT_NetTCPConnection"
                    and isinstance(table.get("records"), list), "Malformed native TCP table evidence")
            require(all(row["state"] == 11 for row in table["records"]), "Owned QEMU ports retain non-TIME_WAIT TCP state")
        except subprocess.TimeoutExpired as error:
            (self.out / "native-tcp.stdout.txt").write_bytes(error.stdout or b"")
            (self.out / "native-tcp.stderr.txt").write_bytes(error.stderr or b"")
            errors.append("Native TCP query exceeded 5s")
        except Exception as error:
            errors.append("Native TCP query: " + str(error))
        if self.watchdog is not None:
            self.watchdog.cancel()
        self.record.update(ok=not errors and not self.expired.is_set() and "error" not in self.record,
                           phase="closed", cleanup_errors=errors, events=self.events)
        self._save()
        require(not errors and not self.expired.is_set(), "Direct guest cleanup/deadline failed: " + str(errors))

    def __exit__(self, exc_type, exc, traceback):
        if exc is not None:
            self.record.update(error=f"{type(exc).__name__}: {exc}", failure_phase=self.record["phase"])
        try:
            self.close()
        except Exception:
            if exc is None:
                raise
