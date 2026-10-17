#!/usr/bin/env python3
"""Bounded, allowlisted Tk key events on an existing isolated X11 display.

This process never reads the clipboard or changes the keyboard layout. Starting
it raises a new fixture window; run it only after other foreground acceptance.
"""
import argparse
import collections
import json
import os
from pathlib import Path
import socket
import time


CONTROL = "/tmp/adit-vnc-keyboard.sock"
PHASES = ("idle", "modifiers", "win", "alt_tab", "altgr", "ime", "release")
KEYSYMS = frozenset((
    "Super_L", "Super_R", "Meta_L", "Meta_R", "Control_L", "Control_R",
    "Alt_L", "Alt_R", "Shift_L", "Shift_R", "ISO_Level3_Shift", "Mode_switch",
    "Tab", "ISO_Left_Tab", "Return", "KP_Enter", "Escape",
    "a", "A", "q", "at", "e", "eacute", "dead_acute", "combining_acute",
))
TEXT = frozenset(("a", "A", "q", "@", "e", "中", "文", "中文", "é", "e\u0301", "\u0301", "😀"))
CODEPOINTS = frozenset(ord(char) for text in TEXT for char in text)
MAX_EVENTS = 256


def sanitized_event(event, kind):
    """Unknown input contributes only to a counter, never a raw field."""
    keysym = str(event.keysym)
    known = keysym in KEYSYMS
    if keysym.startswith("U") and 4 <= len(keysym[1:]) <= 8:
        try:
            known = int(keysym[1:], 16) in CODEPOINTS
        except ValueError:
            pass
    text = event.char if event.char in TEXT else ""
    if not known and not text:
        return None
    record = {"event": kind, "keysym": keysym if known else "<unmapped>",
              "state": int(event.state) & 0xFFFF}
    if text:
        record["text"] = text
    return record


def control(args):
    payload = {"command": args.command, "phase": args.phase}
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(3)
        client.connect(args.control)
        client.sendall(json.dumps(payload).encode("ascii"))
        reply = bytearray()
        while True:
            part = client.recv(4096)
            if not part:
                break
            reply.extend(part)
            if len(reply) > 131072:
                raise RuntimeError("fixture reply exceeded its bound")
    print(json.dumps(json.loads(reply), ensure_ascii=False, indent=2))


def serve(args):
    import tkinter as tk

    if os.path.lexists(args.control):
        raise RuntimeError("control path already exists; refusing to replace another target")
    os.environ["DISPLAY"] = ":" + str(args.display)
    root = tk.Tk()
    root.title("Adit synthetic keyboard target")
    root.overrideredirect(True)
    root.geometry(f"{min(1100, root.winfo_screenwidth()-20)}x{min(740, root.winfo_screenheight()-20)}+10+10")
    root.configure(background="#17324d")
    caption = tk.Label(root, background="#17324d", foreground="white", anchor="w",
                       text="SYNTHETIC KEYS ONLY — no clipboard, no free text storage")
    caption.pack(fill="x", padx=12, pady=10)
    view = tk.Label(root, background="#17324d", foreground="white", anchor="nw",
                    justify="left", font=("monospace", 13), takefocus=True)
    view.pack(fill="both", expand=True, padx=12)
    tk.Button(root, text="Quit target", command=root.destroy, takefocus=False).pack(pady=8)
    events = collections.deque(maxlen=MAX_EVENTS)
    counts = {"allowed": 0, "ignored": 0, "evicted": 0}
    phase = "idle"
    started = time.monotonic()

    def snapshot():
        return {"phase": phase, "elapsed_seconds": round(time.monotonic()-started, 3),
                "max_events": MAX_EVENTS, "counts": dict(counts), "events": list(events)}

    def redraw():
        lines = [f"phase={phase}  counts={counts}  state bits: Shift=1 Ctrl=4 Mod1=8 Mod4=64 Mod5=128",
                 "Physical keys only. Use the client local Ctrl+Alt+Enter escape before leaving.", ""]
        lines.extend(json.dumps(item, ensure_ascii=False) for item in list(events)[-18:])
        view.configure(text="\n".join(lines))

    def capture(event, kind):
        record = sanitized_event(event, kind)
        if record is None:
            counts["ignored"] += 1
        else:
            counts["allowed"] += 1
            if len(events) == MAX_EVENTS:
                counts["evicted"] += 1
            record.update(phase=phase, elapsed_ms=round((time.monotonic()-started)*1000))
            events.append(record)
        redraw()
        return "break"

    root.bind("<KeyPress>", lambda event: capture(event, "KeyPress"))
    root.bind("<KeyRelease>", lambda event: capture(event, "KeyRelease"))
    view.bind("<Button-1>", lambda _event: view.focus_set())
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    bound = False
    try:
        listener.bind(args.control)
        bound = True
        os.chmod(args.control, 0o600)
        listener.listen(1)
        listener.setblocking(False)

        def tick():
            nonlocal phase
            if time.monotonic()-started >= args.seconds:
                root.destroy()
                return
            try:
                client, _ = listener.accept()
            except BlockingIOError:
                root.after(100, tick)
                return
            quit_requested = False
            with client:
                client.settimeout(.2)
                try:
                    request = json.loads(client.recv(256))
                    command = request.get("command")
                    if command == "CLEAR":
                        events.clear()
                        counts.update(allowed=0, ignored=0, evicted=0)
                    elif command == "PHASE" and request.get("phase") in PHASES:
                        phase = request["phase"]
                    elif command == "QUIT":
                        quit_requested = True
                    elif command != "STATUS":
                        raise ValueError("unsupported command")
                    reply = snapshot()
                except (ValueError, TypeError, AttributeError, OSError):
                    reply = {"error": "invalid fixture command"}
                try:
                    client.sendall(json.dumps(reply, ensure_ascii=False).encode("utf-8"))
                except OSError:
                    pass
            redraw()
            if quit_requested:
                root.destroy()
            else:
                root.after(100, tick)

        redraw()
        root.after(100, tick)
        root.after(200, lambda: (root.lift(), view.focus_force()))
        print("READY synthetic keyboard target; bounded in-memory events only", flush=True)
        root.mainloop()
    finally:
        listener.close()
        if bound:
            Path(args.control).unlink(missing_ok=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--display", type=int, default=112)
    parser.add_argument("--control", default=CONTROL)
    parser.add_argument("--seconds", type=int, default=600)
    parser.add_argument("--command", choices=("STATUS", "CLEAR", "PHASE", "QUIT"))
    parser.add_argument("--phase", choices=PHASES, default="idle")
    options = parser.parse_args()
    if not 1 <= options.display <= 999 or not 1 <= options.seconds <= 1800:
        parser.error("isolated display 1..999 and lifetime 1..1800 seconds required")
    control(options) if options.command else serve(options)
