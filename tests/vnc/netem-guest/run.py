"""Run only on disposable Windows CI. Own one QEMU process and loopback echo server."""
import argparse
import hashlib
import json
import os
import pathlib
import queue
import socket
import statistics
import subprocess
import threading
import time


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def echo_service(listener):
    def connection(client):
        try:
            with client:
                client.settimeout(20)
                while chunk := client.recv(16384):
                    client.sendall(chunk)
        except OSError:
            pass

    while True:
        try:
            client, _ = listener.accept()
        except OSError:
            return
        threading.Thread(target=connection, args=(client,), daemon=True).start()


def exchange(payload, deadline=12):
    started = time.monotonic()
    with socket.create_connection(("127.0.0.1", 15912), timeout=deadline) as client:
        client.settimeout(deadline)
        client.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        received = bytearray()
        errors = []

        def send():
            try:
                client.sendall(payload)
            except OSError as error:
                errors.append(str(error))

        writer = threading.Thread(target=send, daemon=True)
        writer.start()
        while len(received) < len(payload):
            left = deadline - (time.monotonic() - started)
            if left <= 0:
                raise TimeoutError("Application echo deadline")
            client.settimeout(left)
            chunk = client.recv(min(65536, len(payload) - len(received)))
            if not chunk:
                raise ConnectionError("Early echo EOF")
            received.extend(chunk)
        writer.join(timeout=max(0.01, deadline - (time.monotonic() - started)))
        require(not writer.is_alive() and not errors, f"Writer did not finish: {errors}")
        require(received == payload, "Echo payload mismatch")
    return {"bytes": len(payload), "elapsed_ms": round((time.monotonic() - started) * 1000, 3), "sha256": hashlib.sha256(received).hexdigest()}


def drops(response):
    for item in reversed(response["results"]):
        if item["argv"][:3] == ["tc", "-s", "-j"]:
            return sum(qdisc.get("drops", 0) for qdisc in json.loads(item["stdout"]))
    raise RuntimeError("Missing qdisc counters")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--qemu", required=True)
    parser.add_argument("--guest", required=True)
    parser.add_argument("--results", required=True)
    args = parser.parse_args()
    require(os.name == "nt" and os.getenv("GITHUB_ACTIONS") == "true" and os.getenv("RUNNER_ENVIRONMENT") == "github-hosted", "Disposable Windows GitHub runner only")
    out = pathlib.Path(args.results).resolve()
    require(out.is_relative_to(pathlib.Path(os.environ["RUNNER_TEMP"]).resolve()) and not out.exists(), "Fresh runner-temp output required")
    out.mkdir()
    guest = pathlib.Path(args.guest).resolve()
    manifest = json.loads((guest / "manifest.json").read_text())
    require(manifest["source_sha"] == os.environ["GITHUB_SHA"], "Guest artifact source mismatch")
    for name in ("vmlinuz-virt", "initramfs.cpio.gz"):
        require(hashlib.sha256((guest / name).read_bytes()).hexdigest() == manifest["files"][name], f"Guest artifact hash mismatch: {name}")
    messages = queue.Queue()
    phases = []
    qemu = None
    listener = socket.socket()
    result = {"ok": False, "source_sha": os.environ["GITHUB_SHA"], "scope": "Windows loopback TCP endpoints via TCP-terminating Slirp/guest byte relay with Linux guest-egress packet impairment; not physical NIC, transparent TCP routing, OS sleep, or VNC acceptance", "phases": phases}
    try:
        qemu_exe = pathlib.Path(args.qemu).resolve()
        bios = list(qemu_exe.parent.rglob("bios-256k.bin"))
        require(len(bios) == 1, "Expected exactly one extracted SeaBIOS directory")
        result["qemu_sha256"] = hashlib.sha256(qemu_exe.read_bytes()).hexdigest()
        listener.bind(("127.0.0.1", 15911))
        listener.listen(8)
        threading.Thread(target=echo_service, args=(listener,), daemon=True).start()
        command = [str(qemu_exe), "-L", str(bios[0].parent), "-accel", "tcg,thread=multi", "-machine", "q35", "-cpu", "max", "-smp", "2", "-m", "512", "-display", "none", "-monitor", "none", "-serial", "stdio", "-no-reboot", "-kernel", str(guest / "vmlinuz-virt"), "-initrd", str(guest / "initramfs.cpio.gz"), "-append", "console=ttyS0 rdinit=/init panic=-1", "-nic", "user,model=e1000,restrict=on,ipv6=off,hostfwd=tcp:127.0.0.1:15912-:15912,guestfwd=tcp:10.0.2.100:15911-tcp:127.0.0.1:15911"]
        result["qemu_argv"] = command
        result["guest_manifest"] = manifest
        with (out / "qemu-stderr.txt").open("w", encoding="utf-8") as stderr:
            qemu = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True, encoding="utf-8", errors="replace", bufsize=1, creationflags=subprocess.CREATE_NO_WINDOW)

            def serial_reader():
                with (out / "serial.txt").open("w", encoding="utf-8") as log:
                    for line in qemu.stdout:
                        log.write(line)
                        log.flush()
                        if line.startswith("ADIT_NETEM "):
                            try:
                                messages.put(json.loads(line[len("ADIT_NETEM "):]))
                            except json.JSONDecodeError:
                                pass

            reader = threading.Thread(target=serial_reader, daemon=True)
            reader.start()

            def wait_message(predicate, seconds):
                end = time.monotonic() + seconds
                while time.monotonic() < end:
                    require(qemu.poll() is None, f"QEMU exited: {qemu.returncode}")
                    try:
                        message = messages.get(timeout=min(0.5, max(0.01, end - time.monotonic())))
                        if predicate(message):
                            return message
                    except queue.Empty:
                        pass
                raise TimeoutError("Guest serial response deadline")

            wait_message(lambda message: message.get("event") == "ready", 120)
            sequence = 0

            def control(mode=None):
                nonlocal sequence
                sequence += 1
                request = {"id": sequence}
                if mode is not None:
                    request["mode"] = mode
                qemu.stdin.write(json.dumps(request) + "\n")
                qemu.stdin.flush()
                response = wait_message(lambda message: message.get("id") == sequence, 20)
                phases.append({"control": request, "evidence": response})
                require(response.get("ok"), f"Guest command failed: {response}")
                return response

            payload = bytes(range(256)) * 1024
            control("baseline")
            baseline_echo = [exchange(b"adit-netem-public-echo") for _ in range(5)]
            baseline_bulk = exchange(payload)
            phases.append({"name": "baseline", "echo": baseline_echo, "bulk": baseline_bulk})
            control("delay")
            delayed = [exchange(b"adit-netem-public-echo") for _ in range(5)]
            increase = statistics.median(item["elapsed_ms"] for item in delayed) - statistics.median(item["elapsed_ms"] for item in baseline_echo)
            phases.append({"name": "guest_egress_delay_80ms", "echo": delayed, "median_increase_ms": increase})
            require(increase >= 100, "Fixed guest-egress delay not observed on payload path")
            control("rate")
            limited = exchange(payload, deadline=18)
            phases.append({"name": "guest_egress_aggregate_2mbit", "bulk": limited})
            # The bytes traverse guest egress twice. Assert conservatively and
            # retain raw elapsed/bytes; this is no WAN throughput benchmark.
            require(limited["elapsed_ms"] >= 1500 and limited["elapsed_ms"] > baseline_bulk["elapsed_ms"] + 500, "Rate impairment not independently observed")
            control("drop")
            failure_started = time.monotonic()
            try:
                exchange(b"must-not-pass-netem-loss", deadline=2)
            except (TimeoutError, OSError) as error:
                failure_ms = round((time.monotonic() - failure_started) * 1000, 3)
                phases.append({"name": "100_percent_drop_negative", "expected_failure": str(error), "elapsed_ms": failure_ms})
                require(failure_ms >= 1500, "Immediate socket failure is not accepted as a loss-induced stall")
            else:
                raise RuntimeError("100% packet loss path unexpectedly delivered echo")
            dropped = control()
            require(drops(dropped) > 0, "100% loss produced no measured qdisc drops")
            control("baseline")
            recovered = exchange(b"adit-after-loss-recovery", deadline=12)
            phases.append({"name": "restored", "echo": recovered})
            control()
            result["ok"] = True
    except Exception as error:
        result["error"] = f"{type(error).__name__}: {error}"
        raise
    finally:
        if qemu is not None:
            if qemu.poll() is None:
                qemu.terminate()
                try:
                    qemu.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    qemu.kill()
                    qemu.wait(timeout=5)
            result["qemu_exit"] = qemu.returncode
        listener.close()
        (out / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
