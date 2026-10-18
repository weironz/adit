"""One bounded Windows guestfwd=cmd experiment; fake bytes only, no VNC/auth."""
import argparse
import hashlib
import json
import os
import pathlib
import queue
import socket
import subprocess
import sys
import threading
import time

sys.dont_write_bytecode = True

GUEST_SOURCE = "8fc0866307f8d99c4bd52f5a3f888942b6ad45b1"
GUEST_RUN = 39145306679
REQUEST = b"public-cmd-fake-request"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def require_ci():
    require(os.name == "nt" and os.getenv("GITHUB_ACTIONS") == "true" and os.getenv("RUNNER_ENVIRONMENT") == "github-hosted" and os.getenv("RUNNER_OS") == "Windows", "Disposable Windows CI only")


def helper():
    # GLib connects all three stdio descriptors to the guest socket. Never
    # print, log or allow a traceback onto stdout/stderr (the fake data stream).
    require_ci()
    stopped = threading.Event()
    with socket.create_connection(("127.0.0.1", 15911), timeout=3) as target:
        target.settimeout(1)
        deadline = time.monotonic() + 45

        def from_stdin():
            try:
                while not stopped.is_set():
                    data = sys.stdin.buffer.read1(16384)
                    if not data:
                        target.shutdown(socket.SHUT_WR)
                        return
                    target.sendall(data)
            except (OSError, ValueError):
                stopped.set()

        reader = threading.Thread(target=from_stdin, daemon=True)
        reader.start()
        try:
            while not stopped.is_set() and time.monotonic() < deadline:
                try:
                    data = target.recv(16384)
                except socket.timeout:
                    continue
                if not data:
                    return
                sys.stdout.buffer.write(data)
                sys.stdout.buffer.flush()
            raise TimeoutError("Fake cmd bridge45s deadline")
        finally:
            stopped.set()
            target.shutdown(socket.SHUT_RDWR)
            reader.join(timeout=1)


def recv_exact(client, length, deadline):
    result = bytearray()
    while len(result) < length:
        client.settimeout(max(0.01, deadline - time.monotonic()))
        require(time.monotonic() < deadline, "Fake read deadline")
        chunk = client.recv(length - len(result))
        require(bool(chunk), "Early EOF before public fake marker")
        result.extend(chunk)
    return bytes(result)


class FakeServer:
    def __init__(self):
        self.records = []
        self.errors = []
        self.completed = [threading.Event(), threading.Event()]
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
        self.listener.bind(("127.0.0.1", 15911))
        self.listener.listen(4)
        self.listener.settimeout(0.5)
        self.stopped = threading.Event()
        self.clients = []
        self.workers = []
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    def accept(self):
        while not self.stopped.is_set():
            try:
                client, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            identity = len(self.clients) + 1
            self.clients.append(client)
            worker = threading.Thread(target=self.connection, args=(client, identity), daemon=True)
            self.workers.append(worker)
            worker.start()

    def connection(self, client, identity):
        record = {"connection_id": identity, "actual_host_tcp": True, "host_observed_eof": False, "scenario": "client-first" if identity == 1 else "server-first"}
        self.records.append(record)
        try:
            require(identity <= 2, "More than two actual backend connections")
            with client:
                deadline = time.monotonic() + 8
                marker = identity.to_bytes(4, "big")
                client.sendall(b"ADIT" + marker)
                require(recv_exact(client, len(REQUEST), deadline) == REQUEST, "Fake request mismatch")
                record["request_bytes"] = len(REQUEST)
                client.sendall(b"ACK" + marker)
                if identity == 2:
                    client.shutdown(socket.SHUT_WR)
                client.settimeout(max(0.01, deadline - time.monotonic()))
                require(client.recv(1) == b"", "Backend received extra bytes instead of EOF")
                record["host_observed_eof"] = True
        except Exception as error:
            self.errors.append(f"connection{identity}: {type(error).__name__}: {error}")
        finally:
            if identity <= 2:
                self.completed[identity - 1].set()

    def close(self):
        self.stopped.set()
        self.listener.close()
        for client in self.clients:
            try:
                client.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            client.close()
        self.thread.join(timeout=1)
        for worker in self.workers:
            worker.join(timeout=1)


def main():
    parser = argparse.ArgumentParser()
    for name in ("qemu", "guest", "results"):
        parser.add_argument("--" + name, required=True)
    args = parser.parse_args()
    require_ci()
    out = pathlib.Path(args.results).resolve()
    require(out.is_relative_to(pathlib.Path(os.environ["RUNNER_TEMP"]).resolve()) and not out.exists(), "Fresh runner-temp evidence required")
    out.mkdir()
    result = {"ok": False, "host_helper_source_sha": os.environ["GITHUB_SHA"], "guest_source_sha": GUEST_SOURCE, "guest_build_run": GUEST_RUN, "scope": "One Windows QEMU guestfwd=cmd feasibility experiment, two fake backend TCP connections with client/server EOF; no VNC, authentication, network impairment matrix or product acceptance", "clients": []}
    qemu = None
    serial = None
    fake = None
    serial_listener = socket.socket()
    reader = None
    overall_end = time.monotonic() + 60
    work_end = overall_end - 12
    try:
        guest = pathlib.Path(args.guest).resolve()
        manifest = json.loads((guest / "manifest.json").read_text())
        require(manifest["source_sha"] == GUEST_SOURCE, "Only the explicitly pinned existing guest source may be reused")
        for name in ("vmlinuz-virt", "initramfs.cpio.gz", "modules.dep"):
            require(hashlib.sha256((guest / name).read_bytes()).hexdigest() == manifest["files"][name], f"Guest hash mismatch: {name}")
        exe = pathlib.Path(args.qemu).resolve()
        firmware = list(exe.parent.rglob("bios-256k.bin"))
        require(len(firmware) == 1, "Unique extracted firmware required")
        fake = FakeServer()  # All listeners exist before QEMU starts.
        result["backends"] = fake.records
        result["backend_errors"] = fake.errors
        serial_listener.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
        serial_listener.bind(("127.0.0.1", 15913))
        serial_listener.listen(1)
        serial_listener.settimeout(10)
        helper_command = f'"{pathlib.Path(sys.executable).as_posix()}" -u "{pathlib.Path(__file__).resolve().as_posix()}" --helper'
        network = "user,model=e1000,restrict=on,ipv6=off,hostfwd=tcp:127.0.0.1:15912-:15912,guestfwd=tcp:10.0.2.100:15911-cmd:" + helper_command
        command = [str(exe), "-L", str(firmware[0].parent), "-accel", "tcg,thread=multi", "-machine", "q35", "-cpu", "max", "-smp", "2", "-m", "512", "-display", "none", "-monitor", "none", "-chardev", "socket,id=control,host=127.0.0.1,port=15913", "-serial", "chardev:control", "-no-reboot", "-kernel", str(guest / "vmlinuz-virt"), "-initrd", str(guest / "initramfs.cpio.gz"), "-append", "console=ttyS0 rdinit=/init panic=-1", "-nic", network]
        result["qemu_argv"] = command
        result["qemu_sha256"] = hashlib.sha256(exe.read_bytes()).hexdigest()
        messages = queue.Queue()
        with (out / "qemu-stderr.txt").open("w", encoding="utf-8") as stderr:
            qemu = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=stderr, creationflags=subprocess.CREATE_NO_WINDOW)
            serial, address = serial_listener.accept()
            require(address[0] == "127.0.0.1", "Serial must use loopback")

            def log_serial():
                try:
                    with serial.makefile("r", encoding="utf-8", errors="replace") as stream, (out / "serial.txt").open("w", encoding="utf-8") as log:
                        for line in stream:
                            log.write(line)
                            log.flush()
                            if line.startswith("ADIT_NETEM "):
                                messages.put(json.loads(line[len("ADIT_NETEM "):]))
                except (OSError, ValueError) as error:
                    messages.put({"error": str(error)})

            reader = threading.Thread(target=log_serial, daemon=True)
            reader.start()
            while True:
                require(time.monotonic() < work_end - 20, "Guest boot exceeded experiment deadline")
                require(qemu.poll() is None, f"QEMU exited:{qemu.returncode}")
                try:
                    message = messages.get(timeout=0.5)
                except queue.Empty:
                    continue
                require("error" not in message, f"Serial failed:{message}")
                if message.get("event") == "ready":
                    break
            for identity in (1, 2):
                require(time.monotonic() < work_end - 4, "No time left for fake connection")
                deadline = min(work_end - 4, time.monotonic() + 8)
                with socket.create_connection(("127.0.0.1", 15912), timeout=3) as client:
                    marker = identity.to_bytes(4, "big")
                    require(recv_exact(client, 8, deadline) == b"ADIT" + marker, "Each fresh guest connection must get a distinct actual backend ID")
                    client.sendall(REQUEST)
                    require(recv_exact(client, 7, deadline) == b"ACK" + marker, "Fake ACK mismatch or cross-session bytes")
                    if identity == 1:
                        client.shutdown(socket.SHUT_WR)
                    client.settimeout(max(0.01, deadline - time.monotonic()))
                    require(client.recv(1) == b"", "Guest connection failed to propagate EOF")
                require(fake.completed[identity - 1].wait(timeout=3), "Backend did not finish its EOF path")
                require(not fake.errors, f"Backend errors:{fake.errors}")
                result["clients"].append({"connection_id": identity, "scenario": "client-first" if identity == 1 else "server-first", "guest_observed_eof": True, "host_observed_eof": fake.records[identity - 1]["host_observed_eof"]})
            require(len(fake.clients) == 2 and all(item["host_observed_eof"] for item in fake.records), "Two distinct closed actual host TCP sessions required")
            result["ok"] = True
    except Exception as error:
        result["ok"] = False
        result["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        # Kill only this owned QEMU tree; a spawned bridge may have a blocked
        # inherited descriptor. No unrelated process or protection setting changes.
        if qemu is not None and qemu.poll() is None:
            try:
                subprocess.run(["taskkill", "/PID", str(qemu.pid), "/T", "/F"], capture_output=True, timeout=5, check=True, creationflags=subprocess.CREATE_NO_WINDOW)
                qemu.wait(timeout=3)
            except Exception as error:
                result["ok"] = False
                result["cleanup_error"] = str(error)
        if fake is not None:
            fake.close()
            if len(fake.clients) != 2 or fake.errors or not all(item["host_observed_eof"] for item in fake.records):
                result["ok"] = False
                result["final_backend_error"] = "Final state does not prove exactly two cleanly closed actual backend TCP sessions"
        if serial is not None:
            try:
                serial.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            serial.close()
        serial_listener.close()
        if reader is not None:
            reader.join(timeout=1)
        result["elapsed_seconds"] = round(60 - (overall_end - time.monotonic()), 3)
        (out / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        require(result["ok"], "Original fake cmd probe failed; inspect preserved evidence")


if __name__ == "__main__":
    if sys.argv[1:] == ["--helper"]:
        try:
            helper()
        except BaseException:
            os._exit(3)
        os._exit(0)
    main()
