#!/usr/bin/env python3
"""Login program for the telnet daemon under test: reports, from inside the
server-side pty, exactly which bytes the daemon delivered for each input."""
import os, select, sys, termios, tty

fd = sys.stdin.fileno()

def say(text):
    os.write(1, (text + "\r\n").encode())

def collect(idle):
    data = b""
    while True:
        ready, _, _ = select.select([fd], [], [], idle if data else 10)
        if not ready:
            return data
        chunk = os.read(fd, 256)
        if not chunk:
            return data
        data += chunk

saved = termios.tcgetattr(fd)
for case in ("raw-cr", "raw-crlf", "raw-iac"):
    tty.setraw(fd)
    say("READY " + case)
    say("GOT %s %s" % (case, collect(0.8).hex()))
    termios.tcsetattr(fd, termios.TCSANOW, saved)
say("READY cooked")
line = os.read(fd, 256)
say("GOT cooked %s" % line.hex())
say("DONE")
