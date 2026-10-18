"""Separate fake-calibration guest: host alias TCP, no guestfwd or host helper."""
import json
import os
import selectors
import socket
import subprocess
import threading
import time

OUTPUT_LOCK = threading.Lock()
BUFFER_LIMIT = 65536


def emit(value):
    with OUTPUT_LOCK:
        print("ADIT_DIRECT " + json.dumps(value, separators=(",", ":")), flush=True)


def relay_pair(client, upstream, lifetime=50, drain_timeout=5):
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
        raise RuntimeError(f"Guest snapshot failed {argv}: {completed.stderr}")
    return {"argv": argv, "stdout": completed.stdout, "stderr": completed.stderr}


def main():
    if os.name != "posix" or os.getpid() != 1:
        raise RuntimeError("Entry point is restricted to the disposable guest PID1")
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
            emit(result)

    with socket.socket() as server:
        server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        server.bind(("10.0.2.15", 15912))
        server.listen(4)
        emit({"event": "ready", "target": "10.0.2.2:15911", "address": "10.0.2.15/32",
              "restrict": False, "no_default_route": True, "dns_configured": False, "ipv6_routes_empty": True,
              "scope": "Separate fake TCP lifecycle calibration; no netem impairment or VNC matrix", "snapshots": snapshots})
        identity = 0
        while True:
            client, peer = server.accept()
            identity += 1
            threading.Thread(target=connection, args=(client, identity, peer), daemon=True).start()


if __name__ == "__main__":
    main()
