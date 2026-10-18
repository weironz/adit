#!/usr/bin/env python3
"""Loopback-only, unauthenticated synthetic RFB peer for native GUI acceptance.

This is a deliberately narrow test peer, not a vendor compatibility reference.
No desktop capture, clipboard, credentials, installation or OS configuration.
"""
import argparse
import hashlib
import json
import os
import pathlib
import socket
import struct
import threading
import time

PF = struct.pack(">BBBBHHHBBB3x", 32, 24, 0, 1, 255, 255, 255, 0, 8, 16)
EDS = -308
NAME = b"ADIT SYNTHETIC LOOPBACK GRID - no user data"
MAX_PIXELS = 4096 * 2160


def exact(conn, length):
    result = bytearray()
    while len(result) < length:
        chunk = conn.recv(length - len(result))
        if not chunk:
            raise EOFError("peer disconnected")
        result.extend(chunk)
    return bytes(result)


def rect(x, y, width, height, encoding):
    return struct.pack(">HHHHi", x, y, width, height, encoding)


def layout(width, height, reason=0):
    return rect(reason, 0, width, height, EDS) + struct.pack(
        ">B3xIHHHHI", 1, 42, 0, 0, width, height, 0
    )


def grid(width, height, pixel_format, sequence):
    bits, depth, big, true, rm, gm, bm, rs, gs, bs = struct.unpack(
        ">BBBBHHHBBB3x", pixel_format
    )
    if bits not in (16, 32) or not true or depth > bits:
        raise ValueError("fixture requires 16/32-bit true colour")
    size = bits // 8

    def colour(r, g, b):
        value = ((r * rm // 255) << rs) | ((g * gm // 255) << gs) | ((b * bm // 255) << bs)
        return value.to_bytes(size, "big" if big else "little")

    blue, line = colour(23, 50, 77), colour(66, 96, 120)
    white, cyan, yellow = colour(255, 255, 255), colour(0, 240, 220), colour(255, 204, 0)
    row = bytearray(blue * width)
    for x in range(0, width, 100):
        row[x * size:(x + 1) * size] = line
    full = bytearray()
    for y in range(height):
        full.extend(line * width if y % 100 == 0 else row)

    def pixel(x, y, value):
        if 0 <= x < width and 0 <= y < height:
            offset = (y * width + x) * size
            full[offset:offset + size] = value

    for x, y in ((100, 100), (300, 200), (500, 400), (700, 500)):
        for delta in range(-12, 13):
            # Keep crosshairs observable after real GPU minification; a 1px line
            # can land between sample centres and disappear at a fit scale.
            for stroke in range(-2, 3):
                pixel(x + delta, y + stroke, white)
                pixel(x + stroke, y + delta, white)
        # A small coloured quadrant identifies each crosshair without a font dependency.
        for delta in range(5):
            pixel(x + 18 + delta, y - 18, yellow)
    # A visible right/bottom border makes stale-size stretching easier to identify.
    for x in range(width):
        for dy in (1, 2, 3):
            pixel(x, height - dy, cyan)
    for y in range(height):
        for dx in (1, 2, 3):
            pixel(width - dx, y, cyan)
    # Binary frame identifier at the top: every resize produces new visible pixels.
    for bit in range(16):
        value = yellow if sequence & (1 << bit) else cyan
        for x in range(25 + bit * 12, 33 + bit * 12):
            for y in range(20, 32):
                pixel(x, y, value)
    return bytes(full)


class Peer:
    def __init__(self, emit):
        self.emit = emit
        self.width, self.height = 1200, 800
        self.pf = PF
        self.encodings = []
        self.sequence = 0
        self.dirty = True
        self.first = True
        self.mask = 0

    def frame(self, conn, reason=None):
        self.sequence += 1
        pixels = grid(self.width, self.height, self.pf, self.sequence)
        if reason is not None:
            if EDS not in self.encodings:
                raise ValueError("EDS was not negotiated")
            # EDS changes the coordinate system. End that update before any new-size pixels.
            conn.sendall(struct.pack(">BBH", 0, 0, 1) + layout(self.width, self.height, reason))
        conn.sendall(struct.pack(">BBH", 0, 0, 1)
                     + rect(0, 0, self.width, self.height, 0) + pixels)
        self.dirty = False
        self.emit({"event": "full_frame_sent", "sequence": self.sequence,
                   "width": self.width, "height": self.height,
                   "pixel_bytes": len(pixels), "sha256": hashlib.sha256(pixels).hexdigest(),
                   "eds_reason": reason})

    def serve(self, conn, stopping=lambda: False):
        conn.sendall(b"RFB 003.008\n")
        if exact(conn, 12) != b"RFB 003.008\n":
            raise ValueError("fixture supports RFB3.8 only")
        conn.sendall(bytes((1, 1)))
        if exact(conn, 1) != b"\x01":
            raise ValueError("fixture requires explicit None selection")
        conn.sendall(struct.pack(">I", 0))
        shared = exact(conn, 1)[0]
        conn.sendall(struct.pack(">HH", self.width, self.height) + PF + struct.pack(">I", len(NAME)) + NAME)
        self.emit({"event": "connected", "security": "None", "shared": shared})
        while not stopping():
            tag = exact(conn, 1)[0]
            if tag == 0:
                exact(conn, 3)
                self.pf = exact(conn, 16)
                self.emit({"event": "pixel_format", "hex": self.pf.hex()})
            elif tag == 2:
                _, count = struct.unpack(">BH", exact(conn, 3))
                if count > 256:
                    raise ValueError("too many encodings")
                self.encodings = list(struct.unpack(f">{count}i", exact(conn, count * 4)))
                self.emit({"event": "encodings", "values": self.encodings})
            elif tag == 3:
                incremental, x, y, width, height = struct.unpack(">BHHHH", exact(conn, 9))
                if self.dirty or not incremental:
                    reason = 0 if self.first and EDS in self.encodings else None
                    self.frame(conn, reason)
                    self.first = False
                else:
                    # Keep the production one-request flow alive without flooding raw full frames.
                    time.sleep(0.05)
                    conn.sendall(b"\x00\x00\x00\x00")
            elif tag == 4:
                down, key = struct.unpack(">B2xI", exact(conn, 7))
                self.emit({"event": "key", "down": bool(down), "keysym": key})
            elif tag == 5:
                mask, x, y = struct.unpack(">BHH", exact(conn, 5))
                self.emit({"event": "button" if mask != self.mask else "pointer",
                           "mask": mask, "x": x, "y": y,
                           "width": self.width, "height": self.height, "frame": self.sequence})
                self.mask = mask
            elif tag == 6:
                length = struct.unpack(">3xi", exact(conn, 7))[0]
                if abs(length) > 1024 * 1024:
                    raise ValueError("clipboard payload exceeded fixture bound")
                exact(conn, abs(length))
                self.emit({"event": "unexpected_clipboard", "bytes": abs(length)})
            elif tag == 251:
                width, height, count = struct.unpack(">xHHBx", exact(conn, 7))
                screens = [struct.unpack(">IHHHHI", exact(conn, 16)) for _ in range(count)]
                if not (1 <= width <= 4096 and 1 <= height <= 2160 and width * height <= MAX_PIXELS):
                    raise ValueError("resize outside fixture bounds")
                if count != 1 or screens[0][1:5] != (0, 0, width, height):
                    raise ValueError("fixture accepts one complete screen only")
                self.emit({"event": "set_desktop_size", "width": width, "height": height,
                           "screens": screens, "previous": [self.width, self.height]})
                self.width, self.height = width, height
                self.frame(conn, 1)
            else:
                raise ValueError(f"unsupported client message {tag}")


def self_test():
    events = []
    client, server = socket.socketpair()
    errors = []

    def worker():
        try:
            Peer(events.append).serve(server)
        except EOFError:
            pass
        except Exception as error:
            errors.append(repr(error))
        finally:
            server.close()

    thread = threading.Thread(target=worker)
    thread.start()
    client.settimeout(5)
    try:
        assert exact(client, 12) == b"RFB 003.008\n"
        client.sendall(b"RFB 003.008\n")
        assert exact(client, 2) == b"\x01\x01"
        client.sendall(b"\x01")
        assert exact(client, 4) == bytes(4)
        client.sendall(b"\x01")
        assert struct.unpack(">HH", exact(client, 4)) == (1200, 800)
        assert exact(client, 16) == PF
        assert exact(client, struct.unpack(">I", exact(client, 4))[0]) == NAME
        client.sendall(b"\x00\x00\x00\x00" + PF + struct.pack(">BBHii", 2, 0, 2, 0, EDS))

        def receive_frame(width, height, reason):
            assert exact(client, 4) == b"\x00\x00\x00\x01"
            assert exact(client, 12) == rect(reason, 0, width, height, EDS)
            assert exact(client, 20) == struct.pack(">B3xIHHHHI", 1, 42, 0, 0, width, height, 0)
            assert exact(client, 4) == b"\x00\x00\x00\x01"
            assert exact(client, 12) == rect(0, 0, width, height, 0)
            payload = exact(client, width * height * 4)
            # Check semantic landmarks independently of the fixture's frame hashing.
            assert payload[(200 * width + 300) * 4:(200 * width + 300) * 4 + 3] == b"\xff\xff\xff"
            assert payload[-4:-1] == bytes((0, 240, 220))

        client.sendall(struct.pack(">BBHHHH", 3, 0, 0, 0, 1200, 800))
        receive_frame(1200, 800, 0)
        client.sendall(struct.pack(">BBHHBBIHHHHI", 251, 0, 900, 600, 1, 0, 42, 0, 0, 900, 600, 0))
        receive_frame(900, 600, 1)
        client.sendall(struct.pack(">BBHH", 5, 1, 300, 200))
        client.shutdown(socket.SHUT_WR)
        while client.recv(1024):
            pass
    finally:
        client.close()
        thread.join(5)
    assert not thread.is_alive() and not errors, errors
    assert any(e["event"] == "set_desktop_size" and e["width"] == 900 for e in events)
    assert any(e["event"] == "button" and (e["x"], e["y"]) == (300, 200) for e in events)
    print(json.dumps({"self_test": "passed", "full_frames": 2, "resize": [900, 600], "button": [300, 200]}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--state-dir", type=pathlib.Path)
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    if args.state_dir is None or not args.state_dir.is_absolute():
        parser.error("--state-dir must be an absolute fresh directory")
    args.state_dir.mkdir(parents=False, exist_ok=False)
    with (args.state_dir / "events.jsonl").open("x", encoding="utf-8") as output:
        def emit(event):
            output.write(json.dumps({"monotonic": time.monotonic(), "unix_ms": time.time_ns() // 1_000_000,
                                     **event}) + "\n")
            output.flush()

        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
            # Do not reuse a listener or replace another process's port ownership.
            listener.bind(("127.0.0.1", 15915))
            listener.listen(1)
            listener.settimeout(1)
            (args.state_dir / "ready.json").write_text(json.dumps({
                "address": "127.0.0.1", "port": 15915, "security": "None",
                "synthetic": True, "pid": os.getpid(),
            }), encoding="utf-8")
            deadline = time.monotonic() + 1800
            while time.monotonic() < deadline and not (args.state_dir / "stop.request").exists():
                try:
                    conn, _ = listener.accept()
                except TimeoutError:
                    continue
                with conn:
                    conn.settimeout(60)
                    try:
                        Peer(emit).serve(conn, lambda: time.monotonic() >= deadline
                                         or (args.state_dir / "stop.request").exists())
                    except (EOFError, ConnectionError, TimeoutError) as error:
                        emit({"event": "disconnected", "reason": type(error).__name__})


if __name__ == "__main__":
    main()
