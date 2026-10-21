#!/usr/bin/env python3
"""Actual Tk Entry consumption on an owned isolated X11 display; no clipboard.

The control channel can clear/read the Entry, but cannot insert text or receive
an expected value. Only Tk's native Entry class binding changes its text.
"""
import argparse
import ctypes
import ctypes.util
import json
import os
from pathlib import Path
import socket
import time


def scalars(text):
    """Tcl can expose UTF-16 pairs; pair them, preserving isolated surrogates."""
    result = []
    index = 0
    while index < len(text):
        value = ord(text[index])
        if 0xD800 <= value <= 0xDBFF and index + 1 < len(text):
            low = ord(text[index + 1])
            if 0xDC00 <= low <= 0xDFFF:
                value = 0x10000 + ((value - 0xD800) << 10) + low - 0xDC00
                index += 1
        result.append(f"U+{value:04X}")
        index += 1
    return result


def request(control, command):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.settimeout(3)
        client.connect(control)
        client.sendall(json.dumps({"command": command}).encode("ascii") + b"\n")
        client.shutdown(socket.SHUT_WR)
        data = bytearray()
        while True:
            chunk = client.recv(4096)
            if not chunk:
                break
            data.extend(chunk)
            if len(data) > 65536:
                raise RuntimeError("target reply exceeds bound")
    result = json.loads(data)
    if "error" in result:
        raise RuntimeError(result["error"])
    return result


class XFocus:
    def __init__(self):
        self.lib = ctypes.CDLL(ctypes.util.find_library("X11") or "libX11.so.6")
        self.lib.XOpenDisplay.argtypes = [ctypes.c_char_p]
        self.lib.XOpenDisplay.restype = ctypes.c_void_p
        self.lib.XGetInputFocus.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_int)]
        self.lib.XSetInputFocus.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
        self.lib.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
        self.lib.XQueryTree.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.POINTER(ctypes.c_ulong)), ctypes.POINTER(ctypes.c_uint)]
        self.lib.XFree.argtypes = [ctypes.c_void_p]
        self.lib.XCloseDisplay.argtypes = [ctypes.c_void_p]
        self.display = self.lib.XOpenDisplay(None)
        if not self.display:
            raise RuntimeError("cannot independently query X11 input focus")

    def snapshot(self, targets):
        window = ctypes.c_ulong()
        revert = ctypes.c_int()
        self.lib.XGetInputFocus(self.display, ctypes.byref(window), ctypes.byref(revert))
        original = window.value
        ancestors = []
        for _ in range(32):
            if window.value in (0, 1) or window.value in ancestors:
                break
            ancestors.append(window.value)
            if window.value in targets:
                break
            root, parent = ctypes.c_ulong(), ctypes.c_ulong()
            children = ctypes.POINTER(ctypes.c_ulong)()
            count = ctypes.c_uint()
            ok = self.lib.XQueryTree(self.display, window, ctypes.byref(root), ctypes.byref(parent), ctypes.byref(children), ctypes.byref(count))
            if children:
                self.lib.XFree(children)
            if not ok:
                break
            window = parent
        return {"x_input_focus": original, "x_ancestors": ancestors,
                "x_target_windows": list(targets),
                "x_focus_owned": any(target in ancestors for target in targets)}

    def close(self):
        self.lib.XCloseDisplay(self.display)

    def focus_window(self, window):
        # With no window manager, Tk's internal focus can remain true while
        # X11 still routes keys through PointerRoot. Set the actual owned
        # Entry window, flush the request, then independently read it back.
        self.lib.XSetInputFocus(self.display, window, 2, 0)
        self.lib.XSync(self.display, 0)


def serve(args):
    import tkinter as tk

    if os.path.lexists(args.control):
        raise RuntimeError("control path exists; refusing to replace it")
    os.environ["DISPLAY"] = f":{args.display}"
    root = tk.Tk()
    root.title("Adit synthetic Unicode text consumption")
    root.overrideredirect(True)
    root.geometry("700x220+10+10")
    tk.Label(root, text="SYNTHETIC VNC TEXT / no clipboard / native Tk Entry").pack(pady=20)
    entry = tk.Entry(root, font=("sans", 18), width=35)
    entry.pack(padx=20, pady=10)
    # These virtual events cannot serve as a hidden text insertion fallback.
    entry.bind("<<Paste>>", lambda _event: "break")
    entry.bind("<<PasteSelection>>", lambda _event: "break")
    root.update()
    xfocus = XFocus()
    started = time.monotonic()
    events = []
    counts = {"press": 0, "release": 0, "unfocused_input": 0,
              "focus_losses": 0, "truncated_events": 0}
    last_focused = False

    def focus():
        # Depending on Tk/X11, input focus names the content or its own wrapper.
        wrapper = int(str(root.tk.call("wm", "frame", root._w)), 0)
        record = xfocus.snapshot((root.winfo_id(), wrapper))
        record["tk_entry_focused"] = root.focus_get() == entry
        record["focused"] = record["x_focus_owned"] and record["tk_entry_focused"]
        return record

    def snapshot():
        value = entry.get()
        if len(value) > 256:
            raise RuntimeError("unexpected text exceeds fixture bound")
        return {"pid": os.getpid(), "display": args.display,
                "entry_xid": entry.winfo_id(), "root_xid": root.winfo_id(),
                "text": value, "python_codepoints": [f"U+{ord(c):04X}" for c in value],
                "scalars": scalars(value), "focus": focus(),
                "counts": dict(counts), "events": list(events),
                "tk_patchlevel": root.tk.call("info", "patchlevel"),
                "elapsed_ms": round((time.monotonic() - started) * 1000)}

    def observe(event, kind):
        current = focus()
        counts[kind] += 1
        if not current["focused"]:
            counts["unfocused_input"] += 1
        if len(events) < 64:
            events.append({"kind": kind, "keysym": event.keysym,
                           "char_scalars": scalars(event.char),
                           "focused": current["focused"]})
        else:
            counts["truncated_events"] += 1
        # Returning None preserves the normal Entry class text insertion.

    entry.bind("<KeyPress>", lambda event: observe(event, "press"))
    entry.bind("<KeyRelease>", lambda event: observe(event, "release"))
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    bound = False
    try:
        listener.bind(args.control)
        bound = True
        os.chmod(args.control, 0o600)
        listener.listen(1)
        listener.setblocking(False)
        root.lift()
        entry.focus_force()
        root.update()
        xfocus.focus_window(entry.winfo_id())
        root.update()

        def tick():
            nonlocal last_focused
            if time.monotonic() - started > args.seconds:
                root.destroy()
                return
            current = focus()["focused"]
            if last_focused and not current:
                counts["focus_losses"] += 1
            last_focused = current
            try:
                client, _ = listener.accept()
            except BlockingIOError:
                root.after(20, tick)
                return
            quit_requested = False
            with client:
                client.settimeout(.5)
                try:
                    data = bytearray()
                    while b"\n" not in data:
                        chunk = client.recv(256)
                        if not chunk:
                            break
                        data.extend(chunk)
                        if len(data) > 256:
                            raise ValueError("oversize command")
                    payload = json.loads(data)
                    if set(payload) != {"command"}:
                        raise ValueError("command does not accept text")
                    command = payload["command"]
                    if command == "RESET":
                        entry.delete(0, tk.END)
                        events.clear()
                        for key in counts:
                            counts[key] = 0
                        root.lift()
                        entry.focus_force()
                        root.update()
                        xfocus.focus_window(entry.winfo_id())
                        root.update()
                        last_focused = focus()["focused"]
                    elif command == "QUIT":
                        quit_requested = True
                    elif command != "STATUS":
                        raise ValueError("unsupported command")
                    reply = snapshot()
                except (ValueError, TypeError, OSError, RuntimeError) as error:
                    reply = {"error": str(error)}
                try:
                    client.sendall(json.dumps(reply, ensure_ascii=True).encode("ascii"))
                except OSError:
                    pass
            if quit_requested:
                root.destroy()
            else:
                root.after(20, tick)

        print(json.dumps({"ready": True, "pid": os.getpid(), "control": args.control,
                          "display": args.display}), flush=True)
        root.after(20, tick)
        root.mainloop()
    finally:
        listener.close()
        xfocus.close()
        if bound:
            Path(args.control).unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--display", type=int, default=100)
    parser.add_argument("--control", required=True)
    parser.add_argument("--seconds", type=int, default=180)
    parser.add_argument("--command", choices=("STATUS", "RESET", "QUIT"))
    args = parser.parse_args()
    if not 1 <= args.display <= 999 or not 1 <= args.seconds <= 1800:
        parser.error("isolated display 1..999 and bounded lifetime required")
    if not args.control.startswith("/tmp/adit-unicode-"):
        parser.error("an owned /tmp/adit-unicode-* control path is required")
    if args.command:
        print(json.dumps(request(args.control, args.command), ensure_ascii=True))
    else:
        serve(args)


if __name__ == "__main__":
    main()
