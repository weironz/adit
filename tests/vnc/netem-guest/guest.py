"""Disposable guest only: serial control and a byte relay, never RFB parsing."""
import json
import selectors
import socket
import subprocess
import sys
import termios
import threading
import itertools

PREFIX = "ADIT_NETEM "
OUTPUT_LOCK = threading.Lock()
CONNECTION_LOCK = threading.Lock()
CONNECTION_IDS = itertools.count(1)
ACTIVE_CONNECTIONS = set()
MODES = {
    "baseline": None,
    "delay": ["delay", "80ms", "limit", "20000"],
    "rate": ["rate", "2mbit", "limit", "20000"],
    "drop": ["loss", "100%", "limit", "20000"],
    "impaired": ["delay", "90ms", "rate", "5mbit", "loss", "0.1%", "limit", "20000"],
}


def emit(value):
    with OUTPUT_LOCK:
        print(PREFIX + json.dumps(value, separators=(",", ":")), flush=True)


def command(*args, allow_failure=False):
    result = subprocess.run(args, text=True, capture_output=True, timeout=5)
    if result.returncode and not allow_failure:
        raise RuntimeError(f"{args!r}: {result.returncode}: {result.stderr}")
    return {"argv": args, "exit": result.returncode, "stdout": result.stdout, "stderr": result.stderr}


def relay(client):
    connection_id = next(CONNECTION_IDS)
    reason = "peer_eof"
    with CONNECTION_LOCK:
        ACTIVE_CONNECTIONS.add(connection_id)
    try:
        with client, socket.create_connection(("10.0.2.100", 15911), timeout=5) as upstream:
            # Bounded buffers and send deadlines propagate backpressure. This
            # relay deliberately terminates TCP; it does not simulate transparent routing.
            client.settimeout(45)
            upstream.settimeout(45)
            with selectors.DefaultSelector() as poll:
                poll.register(client, selectors.EVENT_READ, upstream)
                poll.register(upstream, selectors.EVENT_READ, client)
                while True:
                    ready = poll.select(45)
                    if not ready:
                        reason = "idle_deadline_45s"
                        return
                    for key, _ in ready:
                        chunk = key.fileobj.recv(16384)
                        if not chunk:
                            return
                        key.data.sendall(chunk)
    except OSError as error:
        reason = f"socket_error: {error}"
    finally:
        with CONNECTION_LOCK:
            ACTIVE_CONNECTIONS.discard(connection_id)
        emit({"event": "relay_close", "connection_id": connection_id, "reason": reason, "timeout_seconds": 45})


def serve():
    with socket.socket() as server:
        server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        server.bind(("0.0.0.0", 15912))
        server.listen(8)
        emit({"event": "ready", "port": 15912, "target": "10.0.2.100:15911"})
        while True:
            client, _ = server.accept()
            threading.Thread(target=relay, args=(client,), daemon=True).start()


def main():
    attrs = termios.tcgetattr(sys.stdin.fileno())
    attrs[3] &= ~(termios.ECHO | termios.ICANON)
    attrs[6][termios.VMIN] = 1
    attrs[6][termios.VTIME] = 0
    termios.tcsetattr(sys.stdin.fileno(), termios.TCSANOW, attrs)
    threading.Thread(target=serve, daemon=True).start()
    for line in sys.stdin:
        request = None
        try:
            request = json.loads(line)
            request_id = request["id"]
            mode = request.get("mode")
            results = []
            if mode is not None:
                if mode not in MODES:
                    raise ValueError("Unknown fixed test mode")
                # Only guest eth0 is touched. Capture the previous counters
                # before replacing the qdisc, rather than losing drop evidence.
                results.append(command("tc", "-s", "-j", "qdisc", "show", "dev", "eth0"))
                results.append(command("tc", "qdisc", "del", "dev", "eth0", "root", allow_failure=True))
                if MODES[mode]:
                    results.append(command("tc", "qdisc", "add", "dev", "eth0", "root", "netem", *MODES[mode]))
            results.append(command("tc", "-s", "-j", "qdisc", "show", "dev", "eth0"))
            results.append(command("cat", "/proc/net/snmp"))
            results.append(command("cat", "/proc/net/tcp"))
            with CONNECTION_LOCK:
                active = sorted(ACTIVE_CONNECTIONS)
            emit({"id": request_id, "ok": True, "mode": mode, "active_connections": active, "results": results})
        except Exception as error:
            emit({"id": request.get("id") if isinstance(request, dict) else None, "ok": False, "error": str(error)})


if __name__ == "__main__":
    main()
