"""One 60s Windows fake calibration for a separate guest host-alias architecture."""
import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
import queue
import socket
import subprocess
import threading
import time

REQUEST = b"public-direct-fake-request"
QEMU_SHA256 = "47d57a6072e0bb3bd98f87926eb129eb1736dfe818c67b3b81ef7ce4edd0b3cd"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def tail(identity, direction):
    return direction.encode("ascii") + identity.to_bytes(4, "big") + bytes(range(256)) * 1024


def recv_exact(client, length, deadline):
    result = bytearray()
    while len(result) < length:
        require(time.monotonic() < deadline, "Public fake read deadline")
        client.settimeout(max(0.01, deadline - time.monotonic()))
        data = client.recv(min(16384, length - len(result)))
        require(bool(data), "Early EOF before complete public fake marker/tail")
        result.extend(data)
    return bytes(result)


def recv_eof(client, deadline):
    client.settimeout(max(0.01, deadline - time.monotonic()))
    require(client.recv(1) == b"", "Expected EOF, received unexpected/cross-session bytes")


class FakeServer:
    def __init__(self, deadline, port=15911):
        self.deadline = deadline
        self.records = []
        self.errors = []
        self.clients = []
        self.workers = []
        self.completed = [threading.Event(), threading.Event()]
        self.stopped = threading.Event()
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
        self.listener.bind(("127.0.0.1", port))
        self.listener.listen(4)
        self.listener.settimeout(0.2)
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    def accept(self):
        while not self.stopped.is_set():
            try:
                client, address = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            identity = len(self.clients) + 1
            self.clients.append(client)
            worker = threading.Thread(target=self.connection, args=(client, identity, address), daemon=True)
            self.workers.append(worker)
            worker.start()

    def connection(self, client, identity, address):
        record = {"connection_id": identity, "actual_host_tcp": True, "peer": list(address),
                  "host_observed_eof": False, "post_halfclose_tail_verified": False,
                  "scenario": "client-first" if identity == 1 else "server-first"}
        self.records.append(record)
        try:
            require(identity <= 2 and address[0] == "127.0.0.1", "Exactly two loopback backend connections allowed")
            with client:
                deadline = min(self.deadline, time.monotonic() + 10)
                client.settimeout(max(0.01, deadline - time.monotonic()))
                marker = identity.to_bytes(4, "big")
                client.sendall(b"ADIT" + marker)
                require(recv_exact(client, len(REQUEST), deadline) == REQUEST, "Public request mismatch")
                client.sendall(b"ACK" + marker)
                if identity == 1:
                    # The backend must see the client's FIN before producing
                    # this tail; closing both sockets on that FIN would lose it.
                    recv_eof(client, deadline)
                    record["host_observed_eof"] = True
                    payload = tail(identity, "server-after-client-eof")
                    client.sendall(payload)
                    client.shutdown(socket.SHUT_WR)
                    record["server_tail_sent_after_host_eof"] = True
                else:
                    client.shutdown(socket.SHUT_WR)
                    record["host_shutdown_write_before_client_tail"] = True
                    payload = tail(identity, "client-after-server-eof")
                    require(recv_exact(client, len(payload), deadline) == payload, "Post-server-EOF client tail lost/corrupt")
                    recv_eof(client, deadline)
                    record["host_observed_eof"] = True
                record["tail_bytes"] = len(payload)
                record["tail_sha256"] = hashlib.sha256(payload).hexdigest()
                record["post_halfclose_tail_verified"] = True
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


def client_exchange(client, identity, deadline):
    marker = identity.to_bytes(4, "big")
    require(recv_exact(client, 8, deadline) == b"ADIT" + marker, "Fresh session has wrong actual backend ID")
    client.sendall(REQUEST)
    require(recv_exact(client, 7, deadline) == b"ACK" + marker, "Fake ACK mismatch")
    if identity == 1:
        client.shutdown(socket.SHUT_WR)
        payload = tail(identity, "server-after-client-eof")
        require(recv_exact(client, len(payload), deadline) == payload, "Tail after client half-close lost/corrupt")
        recv_eof(client, deadline)
    else:
        # This read must observe server EOF while the opposite direction is
        # still writable. A relay that closes both directions cannot pass.
        recv_eof(client, deadline)
        payload = tail(identity, "client-after-server-eof")
        client.sendall(payload)
        client.shutdown(socket.SHUT_WR)
    return {"connection_id": identity, "client_observed_eof": True,
            "post_halfclose_tail_verified": True, "tail_bytes": len(payload),
            "tail_sha256": hashlib.sha256(payload).hexdigest()}


def main():
    parser = argparse.ArgumentParser()
    for name in ("qemu", "guest", "results"):
        parser.add_argument("--" + name, required=True)
    args = parser.parse_args()
    require(os.name == "nt" and os.getenv("GITHUB_ACTIONS") == "true"
            and os.getenv("RUNNER_ENVIRONMENT") == "github-hosted"
            and os.getenv("RUNNER_OS") == "Windows", "Disposable Windows GitHub runner only")
    runner = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
    out = Path(args.results).resolve()
    require(out.parent == runner and not out.exists(), "Fresh direct RUNNER_TEMP evidence required")
    out.mkdir()
    began = time.monotonic()
    # Reserve 18s for process/thread/socket cleanup plus one bounded native
    # TCP-table query; a short connect_ex alone is inconclusive on Windows.
    work_end = began + 42
    result = {"ok": False, "source_sha": os.environ["GITHUB_SHA"], "phase": "guard_passed",
              "scope": "Separate fake TCP lifecycle calibration via Linux guest host alias, QEMU restrict=off/IPv6 off. No guestfwd, host cmd/helpers, netem impairment, VNC, credentials or product acceptance.",
              "network_boundary": "Guest .15/32 and only .2/32 unicast route, no default/DNS; this is not complete host-port isolation. Host backend and hostfwd remain loopback only. SLIRP and relay terminate TCP.",
              "hard_deadline_seconds": 60, "clients": [], "guest_events": [],
              "calibration_complete_before_cleanup": False}
    qemu = None
    fake = None
    serial = None
    serial_listener = None
    reader = None
    messages = queue.Queue()

    def save():
        (out / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")

    def hard_stop():
        # No helper children exist in this architecture. Terminating this
        # owned QEMU closes its hostfwd; process exit closes our own listeners.
        evidence = {"error": "60 second hard calibration deadline", "owned_qemu_pid": qemu.pid if qemu else None}
        try:
            if qemu is not None and qemu.poll() is None:
                qemu.kill()
                evidence["qemu_termination_requested"] = True
            (out / "hard-deadline.json").write_text(json.dumps(evidence) + "\n", encoding="utf-8")
        finally:
            os._exit(124)

    save()
    watchdog = threading.Timer(60, hard_stop)
    watchdog.daemon = True
    watchdog.start()
    try:
        result["phase"] = "verify_current_source_guest_and_qemu"
        guest = Path(args.guest).resolve(strict=True)
        require(guest.parent == runner, "Guest artifact must be a direct runner-temp child")
        manifest = json.loads((guest / "manifest.json").read_text())
        result["guest_manifest"] = manifest
        require(manifest["source_sha"] == os.environ["GITHUB_SHA"]
                and manifest.get("build_mode") == "direct-host-alias-fake", "Exact current source and separate guest mode required")
        for name in ("vmlinuz-virt", "initramfs.cpio.gz", "modules.dep"):
            require(hashlib.sha256((guest / name).read_bytes()).hexdigest() == manifest["files"][name], "Guest artifact hash mismatch: " + name)
        for source_name, installed in (("direct-init", "init"), ("direct_guest.py", "guest.py")):
            data = Path(__file__).with_name(source_name).read_bytes().replace(b"\r\n", b"\n")
            require(hashlib.sha256(data).hexdigest() == manifest["guest_source_files"][installed], "Guest script source mismatch: " + source_name)
        policy = manifest["network_policy"]
        require(policy["address"] == "10.0.2.15/32" and policy["only_unicast_route"] == "10.0.2.2/32 dev eth0"
                and policy["default_route"] is False and policy["dns_configured"] is False
                and policy["qemu_restrict"] is False, "Exact narrowed guest policy required")
        exe = Path(args.qemu).resolve(strict=True)
        require(exe == runner / "adit-netem-direct-tools" / "extracted" / "qemu-system-x86_64.exe", "Fixed fresh QEMU extraction required")
        result["qemu_sha256"] = hashlib.sha256(exe.read_bytes()).hexdigest()
        require(result["qemu_sha256"] == QEMU_SHA256, "Exact previously inventoried QEMU executable required")
        firmware = list(exe.parent.rglob("bios-256k.bin"))
        require(len(firmware) == 1, "Unique fixed firmware required")
        # Bind every controller endpoint before QEMU. Reserve/check hostfwd's
        # loopback port, then release it immediately before the owned launch.
        fake = FakeServer(work_end)
        result["backends"] = fake.records
        result["backend_errors"] = fake.errors
        serial_listener = socket.socket()
        serial_listener.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
        serial_listener.bind(("127.0.0.1", 15913))
        serial_listener.listen(1)
        serial_listener.settimeout(10)
        with socket.socket() as reserved:
            reserved.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
            reserved.bind(("127.0.0.1", 15912))
        network = "user,model=e1000,restrict=off,ipv6=off,hostfwd=tcp:127.0.0.1:15912-10.0.2.15:15912"
        command = [str(exe), "-L", str(firmware[0].parent), "-accel", "tcg,thread=multi", "-machine", "q35", "-cpu", "max", "-smp", "2", "-m", "512", "-display", "none", "-monitor", "none", "-chardev", "socket,id=control,host=127.0.0.1,port=15913", "-serial", "chardev:control", "-serial", "file:" + str(out / "kernel-console.txt"), "-no-reboot", "-kernel", str(guest / "vmlinuz-virt"), "-initrd", str(guest / "initramfs.cpio.gz"), "-append", "console=ttyS1 rdinit=/init panic=-1 ipv6.disable=1", "-nic", network]
        result["qemu_argv"] = command
        result["phase"] = "start_owned_qemu"
        save()
        with (out / "qemu-stderr.txt").open("w", encoding="utf-8") as stderr:
            qemu = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                    stderr=stderr, creationflags=subprocess.CREATE_NO_WINDOW)
            result["qemu_pid"] = qemu.pid
            serial, peer = serial_listener.accept()
            require(peer[0] == "127.0.0.1", "Serial must be loopback")
            serial_listener.close()

            def read_serial():
                try:
                    with serial.makefile("r", encoding="utf-8", errors="replace") as stream, (out / "serial.txt").open("w", encoding="utf-8") as log:
                        for line in stream:
                            log.write(line)
                            log.flush()
                            if line.startswith("ADIT_DIRECT "):
                                message = json.loads(line[len("ADIT_DIRECT "):])
                                result["guest_events"].append(message)
                                messages.put(message)
                except (OSError, ValueError) as error:
                    messages.put({"reader_error": str(error)})

            reader = threading.Thread(target=read_serial, daemon=True)
            reader.start()

            def wait_message(predicate, deadline):
                while time.monotonic() < deadline:
                    require(qemu.poll() is None, f"QEMU exited early: {qemu.returncode}")
                    try:
                        message = messages.get(timeout=min(0.2, max(0.01, deadline - time.monotonic())))
                    except queue.Empty:
                        continue
                    require("reader_error" not in message, "Serial reader failed: " + str(message))
                    require(not (message.get("event") == "relay_closed" and message.get("ok") is False), "Guest relay failed: " + str(message))
                    if predicate(message):
                        return message
                raise TimeoutError("Guest serial evidence deadline")

            ready = wait_message(lambda m: m.get("event") == "ready", min(work_end - 22, time.monotonic() + 25))
            require(ready["restrict"] is False and ready["no_default_route"] is True
                    and ready["dns_configured"] is False and ready["ipv6_routes_empty"] is True
                    and ready["address"] == "10.0.2.15/32", "Guest actual ready policy differs")
            result["phase"] = "two_fresh_fake_lifecycles"
            for identity in (1, 2):
                deadline = min(work_end - 4, time.monotonic() + 10)
                require(deadline - time.monotonic() >= 3, "Insufficient time for next fake connection")
                with socket.create_connection(("127.0.0.1", 15912), timeout=3) as client:
                    result["clients"].append(client_exchange(client, identity, deadline))
                require(fake.completed[identity - 1].wait(timeout=max(0.01, min(2, work_end - time.monotonic()))), "Backend EOF/tail completion deadline")
                require(not fake.errors, "Backend failed: " + str(fake.errors))
                wait_message(lambda m: m.get("event") == "relay_closed" and m.get("connection_id") == identity, min(work_end, time.monotonic() + 3))
            require(len(fake.clients) == 2 and len(fake.records) == 2
                    and all(r["host_observed_eof"] and r["post_halfclose_tail_verified"] for r in fake.records), "Two actual backend TCP connections with both EOF/tail paths required")
            opened = [e for e in result["guest_events"] if e.get("event") == "relay_open"]
            closed = [e for e in result["guest_events"] if e.get("event") == "relay_closed"]
            require([e["connection_id"] for e in opened] == [1, 2] and [e["connection_id"] for e in closed] == [1, 2], "Exactly two fresh guest upstream lifecycles required")
            require(all(e["ok"] and set(e["eof"]) == {"client", "upstream"}
                        and set(e["shutdown_write"]) == {"client", "upstream"} for e in closed), "Directional EOF and drain not proven")
            result["calibration_complete_before_cleanup"] = True
            result["ok"] = True
    except Exception as error:
        result["ok"] = False
        result["error"] = f"{type(error).__name__}: {error}"
    finally:
        result["phase"] = "cleanup_owned_endpoints"
        if qemu is not None:
            try:
                if qemu.poll() is None:
                    qemu.kill()
                qemu.wait(timeout=3)
                result["qemu_stopped"] = qemu.poll() is not None
                result["qemu_exit_after_owned_cleanup"] = qemu.returncode
            except Exception as error:
                result["ok"] = False
                result["qemu_cleanup_error"] = str(error)
        if fake is not None:
            fake.close()
            if len(fake.clients) != 2 or fake.errors:
                result["ok"] = False
        if serial is not None:
            try:
                serial.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            serial.close()
        if serial_listener is not None:
            serial_listener.close()
        if reader is not None:
            reader.join(timeout=1)
            result["serial_reader_stopped"] = not reader.is_alive()
            if reader.is_alive():
                result["ok"] = False
        cleanup = []
        for port in (15911, 15912, 15913):
            with socket.socket() as check:
                check.settimeout(0.3)
                code = check.connect_ex(("127.0.0.1", port))
                cleanup.append({"loopback_port": port, "connect_ex": code,
                                "diagnostic": "refused" if code in (10061, errno.ECONNREFUSED) else "inconclusive"})
        result["port_cleanup_connect_diagnostic"] = cleanup
        native_query = r"""
        $ErrorActionPreference='Stop'
        $filter='LocalPort=15911 OR LocalPort=15912 OR LocalPort=15913 OR RemotePort=15911 OR RemotePort=15912 OR RemotePort=15913'
        $rows=@(Get-CimInstance -Namespace root/StandardCimv2 -ClassName MSFT_NetTCPConnection -Filter $filter -ErrorAction Stop)
        $records=@($rows | ForEach-Object {
            [ordered]@{ local_address=[string]$_.LocalAddress; local_port=[int]$_.LocalPort;
                        remote_address=[string]$_.RemoteAddress; remote_port=[int]$_.RemotePort;
                        state=[int]$_.State; owning_process=[uint32]$_.OwningProcess }
        })
        [ordered]@{ query_success=$true; source='MSFT_NetTCPConnection'; filter=$filter; records=$records } | ConvertTo-Json -Depth 6 -Compress
        """
        try:
            queried = subprocess.run(["pwsh.exe", "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", native_query],
                                     capture_output=True, timeout=5, check=False,
                                     creationflags=subprocess.CREATE_NO_WINDOW)
            (out / "native-tcp.stdout.txt").write_bytes(queried.stdout)
            (out / "native-tcp.stderr.txt").write_bytes(queried.stderr)
            require(queried.returncode == 0, "Native TCP table query failed; absence is not established")
            native = json.loads(queried.stdout.decode("utf-8-sig"))
            require(native.get("query_success") is True and native.get("source") == "MSFT_NetTCPConnection"
                    and isinstance(native.get("records"), list), "Malformed native TCP table evidence")
            # MSFT_NetTCPConnection state 11 is TIME_WAIT. Any other residual
            # state (LISTEN, ESTABLISHED, SYN/FIN/CLOSE states, or BOUND) fails.
            residual = [row for row in native["records"] if row["state"] != 11]
            result["port_cleanup_native_table"] = native
            result["port_cleanup_active_residual"] = residual
            require(not residual, "Owned calibration ports retain non-TIME_WAIT TCP state")
            result["port_cleanup_verified"] = True
        except subprocess.TimeoutExpired as error:
            (out / "native-tcp.stdout.txt").write_bytes(error.stdout or b"")
            (out / "native-tcp.stderr.txt").write_bytes(error.stderr or b"")
            result["ok"] = False
            result["port_cleanup_verified"] = False
            result["native_tcp_query_error"] = "Native TCP table query exceeded 5 seconds"
        except Exception as error:
            result["ok"] = False
            result["port_cleanup_verified"] = False
            result["native_tcp_query_error"] = f"{type(error).__name__}: {error}"
        result["elapsed_seconds"] = round(time.monotonic() - began, 3)
        result["phase"] = "complete" if result["ok"] else "failed"
        save()
        watchdog.cancel()
    require(result["ok"], "Direct host-alias fake calibration failed; original evidence preserved")


if __name__ == "__main__":
    main()
