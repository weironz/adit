"""Separate fake-calibration guest: host alias TCP, no guestfwd or host helper."""
import json
import os
import selectors
import shutil
import socket
import subprocess
import sys
import threading
import time

OUTPUT_LOCK = threading.Lock()
BUFFER_LIMIT = 65536
RELAY_LIFETIME_SECONDS = 120
RELAY_DRAIN_SECONDS = 10
ACTIVITY_LOCK = threading.Lock()
ACTIVE_CONNECTIONS = 0
MODES = {
    "baseline": None,
    "delay": ["delay", "80ms", "limit", "20000"],
    "rate": ["rate", "2mbit", "limit", "20000"],
    "drop": ["loss", "100%", "limit", "20000"],
    "impaired": ["delay", "90ms", "rate", "5mbit", "loss", "0.5%", "limit", "20000"],
}


def emit(value):
    with OUTPUT_LOCK:
        print("ADIT_DIRECT " + json.dumps(value, separators=(",", ":")), flush=True)


def relay_pair(client, upstream, lifetime=RELAY_LIFETIME_SECONDS, drain_timeout=RELAY_DRAIN_SECONDS):
    """Bounded queues; EOF shuts down only the peer writer after queued bytes drain."""
    peers = {client: upstream, upstream: client}
    names = {client: "client", upstream: "upstream"}
    buffers = {client: bytearray(), upstream: bytearray()}
    reading = {client: True, upstream: True}
    shutdown_pending = {client: False, upstream: False}
    shutdown_sent = {client: False, upstream: False}
    evidence = {"eof": [], "shutdown_write": [], "forwarded_bytes": {"client": 0, "upstream": 0}}
    deadline = time.monotonic() + lifetime
    drain_deadline = None
    for sock in peers:
        sock.setblocking(False)
    with selectors.DefaultSelector() as poll:
        while True:
            for sock in peers:
                if shutdown_pending[sock] and not buffers[sock]:
                    sock.shutdown(socket.SHUT_WR)
                    shutdown_pending[sock] = False
                    shutdown_sent[sock] = True
                    evidence["shutdown_write"].append(names[sock])
            if not any(reading.values()) and not any(buffers.values()):
                if not all(shutdown_sent.values()):
                    raise RuntimeError("Both directional write shutdowns are required")
                return evidence
            remaining = min(deadline, drain_deadline or deadline) - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("Relay lifetime or post-EOF drain deadline")
            for sock, peer in peers.items():
                events = 0
                if reading[sock] and len(buffers[peer]) < BUFFER_LIMIT:
                    events |= selectors.EVENT_READ
                if buffers[sock]:
                    events |= selectors.EVENT_WRITE
                try:
                    poll.unregister(sock)
                except KeyError:
                    pass
                if events:
                    poll.register(sock, events)
            # Pure idle does not close an otherwise healthy socket after 8s.
            for key, events in poll.select(min(0.2, remaining)):
                sock = key.fileobj
                if events & selectors.EVENT_WRITE:
                    try:
                        count = sock.send(buffers[sock])
                    except BlockingIOError:
                        count = 0
                    if count:
                        del buffers[sock][:count]
                if events & selectors.EVENT_READ:
                    peer = peers[sock]
                    try:
                        data = sock.recv(min(16384, BUFFER_LIMIT - len(buffers[peer])))
                    except BlockingIOError:
                        continue
                    if data:
                        buffers[peer].extend(data)
                        evidence["forwarded_bytes"][names[sock]] += len(data)
                    else:
                        reading[sock] = False
                        shutdown_pending[peer] = True
                        evidence["eof"].append(names[sock])
                        if drain_deadline is None:
                            drain_deadline = time.monotonic() + drain_timeout


def snapshot(argv):
    completed = subprocess.run(argv, capture_output=True, text=True, timeout=3, check=False)
    if completed.returncode:
        raise RuntimeError(f"Guest snapshot failed {argv}: stdout={completed.stdout!r}, stderr={completed.stderr!r}")
    return {"argv": argv, "exit": completed.returncode, "stdout": completed.stdout, "stderr": completed.stderr}


def state_from_results(results, active_connections):
    """Parse actual kernel/tool evidence; missing evidence cannot mean idle."""
    qdiscs = None
    tcp = None
    sockets = None
    for item in results:
        if item.get("exit", 0) != 0:
            raise RuntimeError("Failed command in guest state evidence")
        argv = item["argv"]
        if argv == ["tc", "-s", "-j", "qdisc", "show", "dev", "eth0"]:
            qdiscs = json.loads(item["stdout"])
            if not isinstance(qdiscs, list) or not qdiscs:
                raise RuntimeError("Missing actual eth0 qdisc state")
            if any(not all(type(q.get(key)) is int and q[key] >= 0 for key in ("qlen", "backlog", "drops")) for q in qdiscs):
                raise RuntimeError("Missing numeric qdisc queue/drop counters")
        elif argv == ["cat", "/proc/net/snmp"]:
            lines = item["stdout"].splitlines()
            for index in range(len(lines) - 1):
                if lines[index].startswith("Tcp:") and lines[index + 1].startswith("Tcp:"):
                    keys = lines[index].split()[1:]
                    values = list(map(int, lines[index + 1].split()[1:]))
                    if len(keys) != len(values):
                        raise RuntimeError("Malformed kernel TCP counters")
                    tcp = dict(zip(keys, values))
        elif argv == ["cat", "/proc/net/tcp"]:
            lines = item["stdout"].splitlines()
            if not lines or "local_address" not in lines[0] or "rem_address" not in lines[0]:
                raise RuntimeError("Missing kernel TCP socket header")
            sockets = []
            for line in lines[1:]:
                fields = line.split()
                if len(fields) < 4:
                    raise RuntimeError("Malformed kernel TCP socket row")
                sockets.append({"local": fields[1], "remote": fields[2], "state": fields[3]})
    if qdiscs is None or tcp is None or sockets is None or "RetransSegs" not in tcp:
        raise RuntimeError("Guest snapshot omitted actual qdisc/TCP/socket counters")
    return {"qdiscs": qdiscs, "tcp": tcp, "tcp_sockets": sockets,
            "drops": sum(item.get("drops", 0) for item in qdiscs),
            "active_connections": active_connections}


def state_is_idle(stats):
    # A negative-loss SYN_RECV is not a relay thread; kernel state matters.
    return (stats["active_connections"] == 0
            and all(row["state"] in ("0A", "06") for row in stats["tcp_sockets"])
            and all(item.get("qlen", 0) == 0 and item.get("backlog", 0) == 0 for item in stats["qdiscs"]))


def collect_state():
    results = [snapshot(argv) for argv in (
        ["tc", "-s", "-j", "qdisc", "show", "dev", "eth0"],
        ["cat", "/proc/net/snmp"], ["cat", "/proc/net/tcp"],
    )]
    with ACTIVITY_LOCK:
        active = ACTIVE_CONNECTIONS
    # Validate shape before handing the evidence to a host or changing qdisc.
    state_from_results(results, active)
    return {"active_connections": active, "results": results}


def apply_control(request, current_mode, capture=collect_state, execute=None):
    """Only fixed commands are accepted, and queued/live traffic is never flushed."""
    if (not isinstance(request, dict) or set(request) - {"id", "mode"}
            or type(request.get("id")) is not int or request["id"] <= 0):
        raise ValueError("Only positive id and optional fixed mode are accepted")
    requested = request.get("mode")
    if "mode" in request and requested not in MODES:
        raise ValueError("Unknown fixed network mode")
    response = {"id": request["id"], "ok": False, "mode": current_mode,
                "requested_mode": requested, "before": None, "reset": None, "mutation": None,
                "relay_lifetime_seconds": RELAY_LIFETIME_SECONDS}

    def change(field, argv):
        response[field] = {"argv": argv, "exit": None, "stdout": "", "stderr": ""}
        try:
            if execute is None:
                completed = subprocess.run(argv, capture_output=True, text=True, timeout=3, check=False)
                response[field].update(exit=completed.returncode, stdout=completed.stdout, stderr=completed.stderr)
            else:
                response[field].update(execute(argv))
            if response[field]["exit"] != 0:
                raise RuntimeError("Fixed qdisc command failed")
        except subprocess.TimeoutExpired as error:
            response[field]["stdout"] = (error.stdout or b"").decode("utf-8", errors="replace") if isinstance(error.stdout, bytes) else (error.stdout or "")
            response[field]["stderr"] = (error.stderr or b"").decode("utf-8", errors="replace") if isinstance(error.stderr, bytes) else (error.stderr or "")
            response[field]["error"] = "Qdisc command exceeded3s"
            raise
        except Exception as error:
            response[field]["error"] = f"{type(error).__name__}: {error}"
            raise

    try:
        if requested is not None:
            response["before"] = capture()
            before = state_from_results(response["before"]["results"], response["before"]["active_connections"])
            if not state_is_idle(before):
                raise RuntimeError("Kernel TCP/relay/qdisc must be idle before replacing network mode")
            if requested == "baseline":
                if any(item["kind"] == "netem" for item in before["qdiscs"]):
                    change("mutation", ["tc", "qdisc", "del", "dev", "eth0", "root"])
            else:
                if any(item["kind"] == "netem" for item in before["qdiscs"]):
                    # netem replace inherits omitted rate/delay options. Only
                    # after proven idle, remove it before creating a fresh mode.
                    change("reset", ["tc", "qdisc", "del", "dev", "eth0", "root"])
                    response["mode"] = "baseline"
                change("mutation", ["tc", "qdisc", "replace", "dev", "eth0", "root", "netem", *MODES[requested]])
            response["mode"] = requested
        response.update(capture())
        response["ok"] = True
    except Exception as error:
        response["error"] = f"{type(error).__name__}: {error}"
    return response


def main():
    if os.name != "posix" or os.getpid() != 1:
        raise RuntimeError("Entry point is restricted to the disposable guest PID1")
    import termios

    attrs = termios.tcgetattr(sys.stdin.fileno())
    attrs[3] &= ~(termios.ECHO | termios.ICANON)
    attrs[6][termios.VMIN] = 1
    attrs[6][termios.VTIME] = 0
    termios.tcsetattr(sys.stdin.fileno(), termios.TCSANOW, attrs)
    ip_path = shutil.which("ip")
    if ip_path is None:
        raise RuntimeError("Fixed iproute2 tool is missing")
    tool = subprocess.run([ip_path, "-Version"], capture_output=True, text=True, timeout=3, check=False)
    emit({"event": "ip_tool", "path": ip_path, "resolved_path": os.path.realpath(ip_path),
          "argv": [ip_path, "-Version"], "exit": tool.returncode, "stdout": tool.stdout, "stderr": tool.stderr})
    if (tool.returncode != 0 or not tool.stdout.startswith("ip utility, iproute2-6.15.0")
            or "busybox" in (tool.stdout + tool.stderr).lower()):
        raise RuntimeError("Exact iproute2-6.15.0 version banner required; BusyBox is not accepted")
    snapshots = [snapshot(argv) for argv in (
        ["ip", "addr", "show", "dev", "eth0"], ["ip", "route", "show"], ["ip", "-6", "route", "show"],
        ["ip", "route", "get", "10.0.2.2"], ["tc", "-s", "-j", "qdisc", "show", "dev", "eth0"],
        ["cat", "/proc/net/snmp"],
    )]
    # Preserve the actual kernel/tool output before a strict policy assertion
    # can terminate PID1; a failed ready must not hide the evidence it checked.
    emit({"event": "network_snapshot", "snapshots": snapshots})
    if "default" in snapshots[1]["stdout"] or os.path.exists("/etc/resolv.conf"):
        raise RuntimeError("No guest default route or DNS configuration permitted")
    routes = snapshots[1]["stdout"].splitlines()
    if (len(routes) != 1 or routes[0].split()[0] not in ("10.0.2.2", "10.0.2.2/32")
            or routes[0].split()[1:3] != ["dev", "eth0"]
            or "10.0.2.15/32" not in snapshots[0]["stdout"]):
        raise RuntimeError("Require exactly the fixed /32 address and host-alias unicast route")
    if snapshots[2]["stdout"].strip():
        raise RuntimeError("IPv6 routes must be absent in the direct fake guest")

    def connection(client, identity, peer):
        global ACTIVE_CONNECTIONS
        result = {"event": "relay_closed", "connection_id": identity, "ok": False, "client_peer": list(peer)}
        try:
            with client:
                if peer[0] != "10.0.2.2":
                    raise RuntimeError("Unexpected hostfwd peer; do not broaden guest routes")
                with socket.create_connection(("10.0.2.2", 15911), timeout=3) as upstream:
                    emit({"event": "relay_open", "connection_id": identity,
                          "upstream_local": list(upstream.getsockname()), "target": "10.0.2.2:15911"})
                    result.update(relay_pair(client, upstream))
                    result["ok"] = True
        except Exception as error:
            result["error"] = f"{type(error).__name__}: {error}"
        finally:
            with ACTIVITY_LOCK:
                ACTIVE_CONNECTIONS -= 1
            emit(result)

    server = socket.socket()
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("10.0.2.15", 15912))
    server.listen(4)

    def serve():
        global ACTIVE_CONNECTIONS
        identity = 0
        while True:
            client, peer = server.accept()
            with ACTIVITY_LOCK:
                ACTIVE_CONNECTIONS += 1
            identity += 1
            threading.Thread(target=connection, args=(client, identity, peer), daemon=True).start()

    with server:
        threading.Thread(target=serve, daemon=True).start()
        emit({"event": "ready", "target": "10.0.2.2:15911", "address": "10.0.2.15/32",
              "restrict": False, "no_default_route": True, "dns_configured": False, "ipv6_routes_empty": True,
              "scope": "Host-alias TCP relay; optional fixed guest-egress modes, not transparent Windows networking", "snapshots": snapshots,
              "control_protocol": "adit-direct-netem-v1", "relay_lifetime_seconds": RELAY_LIFETIME_SECONDS})
        current_mode = "baseline"
        for line in sys.stdin:
            request = None
            try:
                request = json.loads(line)
                response = apply_control(request, current_mode)
                current_mode = response["mode"]
            except Exception as error:
                response = {"id": request.get("id") if isinstance(request, dict) else None,
                            "ok": False, "error": f"{type(error).__name__}: {error}"}
            emit(response)


if __name__ == "__main__":
    main()
