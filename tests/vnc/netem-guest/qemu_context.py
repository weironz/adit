"""Same working TCG/serial topology as echo PoC; no host network configuration."""
import hashlib
import json
import os
import pathlib
import queue
import socket
import subprocess
import threading
import time

from run import require

SCOPE = "Linux guest eth0 egress delay/rate/loss across TCP-terminating Slirp and byte relay; Windows endpoints use loopback TCP, not transparent TCP loss, physical NIC, OS sleep or GPU acceptance"


def summarize(response):
    qdiscs = None
    tcp = None
    sockets = None
    for item in response["results"]:
        if list(item["argv"])[:3] == ["tc", "-s", "-j"]:
            qdiscs = json.loads(item["stdout"])
        if list(item["argv"]) == ["cat", "/proc/net/snmp"]:
            lines = item["stdout"].splitlines()
            for index in range(len(lines) - 1):
                if lines[index].startswith("Tcp:") and lines[index + 1].startswith("Tcp:"):
                    tcp = dict(zip(lines[index].split()[1:], map(int, lines[index + 1].split()[1:])))
        if list(item["argv"]) == ["cat", "/proc/net/tcp"]:
            sockets = [{"local": fields[1], "remote": fields[2], "state": fields[3]} for line in item["stdout"].splitlines()[1:] if len(fields := line.split()) >= 4]
    require(qdiscs is not None and tcp is not None and sockets is not None, "Guest snapshot omitted actual qdisc/TCP/socket counters")
    return {"qdiscs": qdiscs, "tcp": tcp, "tcp_sockets": sockets, "drops": sum(item.get("drops", 0) for item in qdiscs), "active_connections": response["active_connections"]}


def validate_mode(snapshot, mode):
    netem = [item for item in snapshot["stats"]["qdiscs"] if item["kind"] == "netem"]
    if mode == "baseline":
        require(not netem, "Baseline still has netem installed")
    elif mode == "impaired":
        require(len(netem) == 1, "Expected exactly one actual netem qdisc")
        options = netem[0]["options"]
        require(abs(options["delay"]["delay"] - 0.09) < 0.000001 and options["rate"]["rate"] == 625000 and abs(options["loss-random"]["loss"] - 0.001) < 0.000001 and options["limit"] == 20000, "Actual tc settings differ from90ms/5mbit/0.1%")


class QemuGuest:
    def __init__(self, executable, guest, out):
        self.executable = pathlib.Path(executable).resolve()
        self.guest = pathlib.Path(guest).resolve()
        self.out = pathlib.Path(out)
        self.manifest = json.loads((self.guest / "manifest.json").read_text())
        require(self.manifest["source_sha"] == os.environ["GITHUB_SHA"], "Guest/source mismatch")
        for name in ("vmlinuz-virt", "initramfs.cpio.gz", "modules.dep"):
            require(hashlib.sha256((self.guest / name).read_bytes()).hexdigest() == self.manifest["files"][name], f"Guest hash mismatch: {name}")
        self.sha256 = hashlib.sha256(self.executable.read_bytes()).hexdigest()
        self.messages = queue.Queue()
        self.lock = threading.Lock()
        self.sequence = 0
        self.process = None
        self.serial = None
        self.listener = None
        self.reader = None
        self.stderr = None

    def __enter__(self):
        try:
            firmware = list(self.executable.parent.rglob("bios-256k.bin"))
            require(len(firmware) == 1, "Expected unique extracted SeaBIOS directory")
            self.listener = socket.socket()
            self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
            self.listener.bind(("127.0.0.1", 15913))
            self.listener.listen(1)
            self.listener.settimeout(15)
            self.argv = [str(self.executable), "-L", str(firmware[0].parent), "-accel", "tcg,thread=multi", "-machine", "q35", "-cpu", "max", "-smp", "2", "-m", "512", "-display", "none", "-monitor", "none", "-chardev", "socket,id=control,host=127.0.0.1,port=15913", "-serial", "chardev:control", "-no-reboot", "-kernel", str(self.guest / "vmlinuz-virt"), "-initrd", str(self.guest / "initramfs.cpio.gz"), "-append", "console=ttyS0 rdinit=/init panic=-1", "-nic", "user,model=e1000,restrict=on,ipv6=off,hostfwd=tcp:127.0.0.1:15912-:15912,guestfwd=tcp:10.0.2.100:15911-tcp:127.0.0.1:15911"]
            self.stderr = (self.out / "qemu-stderr.txt").open("w", encoding="utf-8")
            self.process = subprocess.Popen(self.argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=self.stderr, creationflags=subprocess.CREATE_NO_WINDOW)
            self.serial, address = self.listener.accept()
            require(address[0] == "127.0.0.1", "Serial peer must be loopback")
            self.serial.settimeout(25)
            self.listener.close()

            def read_serial():
                try:
                    with self.serial.makefile("r", encoding="utf-8", errors="replace") as stream, (self.out / "serial.txt").open("w", encoding="utf-8") as log:
                        for line in stream:
                            log.write(line)
                            log.flush()
                            if line.startswith("ADIT_NETEM "):
                                self.messages.put(json.loads(line[len("ADIT_NETEM "):]))
                except (OSError, ValueError) as error:
                    self.messages.put({"serial_reader_error": str(error)})

            # Boot may legitimately be quiet for >25s under TCG. Only control
            # writes are bounded by socket timeout; reader's waits have deadlines.
            self.serial.settimeout(None)
            self.reader = threading.Thread(target=read_serial, daemon=True)
            self.reader.start()
            self.wait(lambda item: item.get("event") == "ready", 120)
            return self
        except Exception:
            self.close()
            raise

    def wait(self, predicate, seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            require(self.process.poll() is None, f"QEMU exited: {self.process.returncode}")
            try:
                response = self.messages.get(timeout=min(0.5, max(0.01, end - time.monotonic())))
                require("serial_reader_error" not in response, f"Guest serial reader stopped: {response}")
                if predicate(response):
                    return response
            except queue.Empty:
                pass
        raise TimeoutError("Guest serial command deadline")

    def control(self, mode=None):
        with self.lock:
            self.sequence += 1
            request = {"id": self.sequence}
            if mode is not None:
                request["mode"] = mode
            self.serial.sendall(json.dumps(request).encode() + b"\n")
            response = self.wait(lambda item: item.get("id") == self.sequence, 20)
            record = {"request": request, "response": response, "utc_unix": time.time()}
            with (self.out / "controls.jsonl").open("a", encoding="utf-8") as log:
                log.write(json.dumps(record) + "\n")
            require(response.get("ok"), f"Guest control failed: {response}")
            return {"raw": response, "stats": summarize(response)}

    def idle(self):
        end = time.monotonic() + 12
        while time.monotonic() < end:
            snapshot = self.control()
            # SYN_RECV from negative calibration is not represented by relay
            # threads. It must disappear too, or its retransmits contaminate
            # later VNC loss counters. Only LISTEN/TIME_WAIT are allowed.
            no_live_sockets = all(item["state"] in ("0A", "06") for item in snapshot["stats"]["tcp_sockets"])
            if no_live_sockets and not snapshot["stats"]["active_connections"] and all(item.get("qlen", 0) == 0 and item.get("backlog", 0) == 0 for item in snapshot["stats"]["qdiscs"]):
                return snapshot
            time.sleep(0.2)
        raise TimeoutError("Owned TCP relay/queue did not drain before qdisc change")

    def close(self):
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        if self.serial is not None:
            try:
                self.serial.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            self.serial.close()
        if self.listener is not None:
            self.listener.close()
        if self.reader is not None:
            self.reader.join(timeout=2)
        if self.stderr is not None:
            self.stderr.close()

    def __exit__(self, *_):
        self.close()
