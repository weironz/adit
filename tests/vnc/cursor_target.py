#!/usr/bin/env python3
"""Isolated X11 cursor fixture, controlled over a container-local Unix socket."""
import argparse
import ctypes as c
import json
import os
import socket

CONTROL = "/tmp/adit-vnc-cursor.sock"


class Image(c.Structure):
    _fields_ = [(name, c.c_uint) for name in
                ("version", "size", "width", "height", "xhot", "yhot", "delay")] + [
                    ("pixels", c.POINTER(c.c_uint))]


def serve():
    x11, xc = c.CDLL("libX11.so.6"), c.CDLL("libXcursor.so.1")
    x11.XOpenDisplay.argtypes = [c.c_char_p]
    x11.XOpenDisplay.restype = c.c_void_p
    x11.XDefaultRootWindow.argtypes = [c.c_void_p]
    x11.XDefaultRootWindow.restype = c.c_ulong
    x11.XCreateSimpleWindow.argtypes = [c.c_void_p, c.c_ulong, c.c_int, c.c_int,
                                      c.c_uint, c.c_uint, c.c_uint, c.c_ulong, c.c_ulong]
    x11.XCreateSimpleWindow.restype = c.c_ulong
    x11.XMapRaised.argtypes = [c.c_void_p, c.c_ulong]
    x11.XCreateFontCursor.argtypes = [c.c_void_p, c.c_uint]
    x11.XCreateFontCursor.restype = c.c_ulong
    x11.XDefineCursor.argtypes = [c.c_void_p, c.c_ulong, c.c_ulong]
    x11.XFreeCursor.argtypes = [c.c_void_p, c.c_ulong]
    x11.XSync.argtypes = [c.c_void_p, c.c_int]
    xc.XcursorImageCreate.argtypes = [c.c_int, c.c_int]
    xc.XcursorImageCreate.restype = c.POINTER(Image)
    xc.XcursorImageLoadCursor.argtypes = [c.c_void_p, c.POINTER(Image)]
    xc.XcursorImageLoadCursor.restype = c.c_ulong
    xc.XcursorImageDestroy.argtypes = [c.POINTER(Image)]
    display = x11.XOpenDisplay(None)
    if not display:
        raise RuntimeError("isolated X display is unavailable")
    root = x11.XDefaultRootWindow(display)
    window = x11.XCreateSimpleWindow(display, root, 0, 0, 800, 600, 0, 0, 0x244466)
    x11.XMapRaised(display, window)
    previous = None

    def set_shape(name):
        nonlocal previous
        if name in ("arrow", "ibeam"):
            cursor = x11.XCreateFontCursor(display, 68 if name == "arrow" else 152)
            expected = {"shape": name}
        elif name in ("rgba", "hidden"):
            image = xc.XcursorImageCreate(16, 16)
            if not image:
                raise RuntimeError("Xcursor allocation failed")
            image.contents.xhot, image.contents.yhot = 5, 7
            for y in range(16):
                for x in range(16):
                    # Two corner pixels keep the complete bounding box: X11
                    # servers may legitimately crop all-transparent borders.
                    visible = name == "rgba" and (
                        (2 <= x <= 13 and 2 <= y <= 13 and (x, y) != (7, 7))
                        or (x, y) in ((0, 15), (15, 0)))
                    colour = (0xFFFF0000, 0xFF00FF00, 0xFF0000FF)[(x + y) % 3]
                    image.contents.pixels[y * 16 + x] = colour if visible else 0
            cursor = xc.XcursorImageLoadCursor(display, image)
            xc.XcursorImageDestroy(image)
            expected = {"shape": name, "width": 16, "height": 16, "hotspot_x": 5, "hotspot_y": 7}
        else:
            raise ValueError("unknown fixture cursor")
        if not cursor:
            raise RuntimeError("X cursor creation failed")
        x11.XDefineCursor(display, root, cursor)
        x11.XDefineCursor(display, window, cursor)
        x11.XSync(display, 0)
        if previous is not None:
            x11.XFreeCursor(display, previous)
        previous = cursor
        return expected

    set_shape("arrow")
    if os.path.exists(CONTROL):
        os.unlink(CONTROL)
    with socket.socket(socket.AF_UNIX) as listener:
        listener.bind(CONTROL)
        listener.listen(1)
        while True:
            connection, _ = listener.accept()
            with connection:
                try:
                    result = set_shape(connection.recv(64).decode("ascii").strip())
                except Exception as error:
                    result = {"error": str(error)}
                connection.sendall(json.dumps(result).encode("utf-8") + b"\n")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--set", choices=("arrow", "ibeam", "rgba", "hidden"))
    args = parser.parse_args()
    if args.set:
        with socket.socket(socket.AF_UNIX) as connection:
            connection.settimeout(5)
            connection.connect(CONTROL)
            connection.sendall(args.set.encode("ascii"))
            response = connection.recv(4096).decode("utf-8")
            print(response.strip())
            if "error" in json.loads(response):
                raise SystemExit(1)
    else:
        serve()


if __name__ == "__main__":
    main()
