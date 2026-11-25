#!/usr/bin/env python3
"""One-connection TCP tee: forwards LISTEN_PORT -> TARGET_PORT and records every
chunk in both directions, so the wire bytes Adit emits are captured verbatim."""
import json, os, select, socket, sys, time

listen_port, target_port, out_path = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
server = socket.socket()
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(("127.0.0.1", listen_port))
server.listen(1)
print("listening", flush=True)
client, _ = server.accept()
upstream = socket.create_connection(("127.0.0.1", target_port))
start = time.monotonic()
with os.fdopen(os.open(out_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "w") as log:
    peers = {client: (upstream, "c2s"), upstream: (client, "s2c")}
    while True:
        ready, _, _ = select.select(list(peers), [], [], 30)
        if not ready:
            break
        done = False
        for sock in ready:
            data = sock.recv(65536)
            other, direction = peers[sock]
            if not data:
                done = True
                break
            other.sendall(data)
            log.write(json.dumps({"t": round(time.monotonic() - start, 4), "dir": direction, "hex": data.hex()}) + "\n")
        if done:
            break
