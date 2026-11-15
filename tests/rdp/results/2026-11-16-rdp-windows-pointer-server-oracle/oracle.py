#!/usr/bin/env python3
"""Prints the RDP session's own pointer position, as `X Y`.

Reads `latest.json`, which CursorLogger.exe rewrites every ~10 ms from inside
the RDP user's session. A stale sample is refused rather than reported: if the
logger died (or the session went to the lock screen), an old position would
look like a pointer that simply did not move.

  oracle.py pointer [field]   field = cursor (default) | physical | unaware
  oracle.py topology          the session's virtual screen + monitor list
"""
import datetime
import json
import os
import sys
import time

DIR = os.environ.get("ADIT_ORACLE_DIR", r"C:\adit-oracle")
MAX_AGE_S = 1.0
# A fresh logon takes a few seconds before the at-logon task starts the
# logger, so a missing or stale sample is waited for this long before failing.
STARTUP_WAIT_S = float(os.environ.get("ADIT_ORACLE_WAIT", "60"))


def latest():
    deadline = time.time() + STARTUP_WAIT_S
    while True:
        sample, age = read_once()
        if sample is not None and age <= MAX_AGE_S:
            return sample
        if time.time() > deadline:
            if sample is None:
                sys.exit("no sample: logger not running in the RDP session?")
            sys.exit(f"stale sample ({age:.1f}s old): logger not running in the RDP session?")
        time.sleep(0.1)


def read_once():
    path = os.path.join(DIR, "latest.json")
    for _ in range(50):
        try:
            with open(path, encoding="utf-8") as f:
                sample = json.loads(f.read())
            break
        except (OSError, ValueError):
            # File.Replace briefly holds the file; retry instead of failing.
            time.sleep(0.01)
    else:
        return None, None
    stamp = datetime.datetime.fromisoformat(sample["utc"][:26].rstrip("Z")).replace(tzinfo=datetime.timezone.utc)
    age = (datetime.datetime.now(datetime.timezone.utc) - stamp).total_seconds()
    return sample, age


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "pointer"
    sample = latest()
    if mode == "pointer":
        field = sys.argv[2] if len(sys.argv) > 2 else "cursor"
        if sample[field] is None:
            sys.exit(f"{field} unavailable (GetCursorPos failed: secure desktop?)")
        x, y = sample[field]
        print(f"{x} {y}")
    elif mode == "topology":
        print(json.dumps({k: sample[k] for k in ("session", "virtual", "monitor_count", "monitors", "cursor", "physical", "unaware")}))
    else:
        sys.exit(f"unknown mode {mode}")


if __name__ == "__main__":
    main()
