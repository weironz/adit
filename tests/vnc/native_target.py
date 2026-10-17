#!/usr/bin/env python3
"""Synthetic grid plus remote input coordinates for native viewer acceptance."""
import json
import time
import tkinter as tk
import ctypes as c

x11 = c.CDLL("libX11.so.6")
x11.XOpenDisplay.argtypes = [c.c_char_p]
x11.XOpenDisplay.restype = c.c_void_p
x11.XDefaultRootWindow.argtypes = [c.c_void_p]
x11.XDefaultRootWindow.restype = c.c_ulong
x11.XGetGeometry.argtypes = [c.c_void_p, c.c_ulong, c.POINTER(c.c_ulong),
                            c.POINTER(c.c_int), c.POINTER(c.c_int),
                            c.POINTER(c.c_uint), c.POINTER(c.c_uint),
                            c.POINTER(c.c_uint), c.POINTER(c.c_uint)]
display = x11.XOpenDisplay(None)
if not display:
    raise RuntimeError("synthetic X display is unavailable")
screen = x11.XDefaultRootWindow(display)


def screen_size():
    root_id, x, y = c.c_ulong(), c.c_int(), c.c_int()
    width, height, border, depth = (c.c_uint() for _ in range(4))
    # Tk's initial screen-width cache may survive an RFB desktop resize.
    # A server round trip reads the current root geometry instead.
    if not x11.XGetGeometry(display, screen, c.byref(root_id), c.byref(x), c.byref(y),
                           c.byref(width), c.byref(height), c.byref(border), c.byref(depth)):
        raise RuntimeError("synthetic root geometry is unavailable")
    return width.value, height.value

root = tk.Tk()
root.overrideredirect(True)
canvas = tk.Canvas(root, highlightthickness=0, background="#17324d", cursor="")
canvas.pack(fill="both", expand=True)
size = None


def report(event):
    print(json.dumps({"event": "button", "x": event.x_root, "y": event.y_root,
                      "button": event.num, "width": root.winfo_width(),
                      "height": root.winfo_height(), "time": time.monotonic()}), flush=True)
    canvas.delete("click")
    canvas.create_oval(event.x-5, event.y-5, event.x+5, event.y+5,
                       fill="#ffcc00", tags="click")


def refresh():
    global size
    current = screen_size()
    if current != size:
        size = current
        root.geometry(f"{size[0]}x{size[1]}+0+0")
        canvas.delete("all")
        for x in range(0, size[0], 100):
            canvas.create_line(x, 0, x, size[1], fill="#426078")
        for y in range(0, size[1], 100):
            canvas.create_line(0, y, size[0], y, fill="#426078")
        for x, y in ((100, 100), (300, 200), (500, 400), (700, 500)):
            canvas.create_line(x-12, y, x+12, y, fill="white", width=2)
            canvas.create_line(x, y-12, x, y+12, fill="white", width=2)
            canvas.create_text(x+12, y-15, text=f"({x},{y})", fill="white", anchor="w")
        canvas.create_text(25, 25, text="SYNTHETIC VNC GRID — no user data", anchor="nw", fill="white")
        print(json.dumps({"event":"resize", "width":size[0], "height":size[1]}), flush=True)
    root.after(100, refresh)


canvas.bind("<ButtonPress>", report)
refresh()
root.mainloop()
