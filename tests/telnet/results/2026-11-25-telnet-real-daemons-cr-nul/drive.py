#!/usr/bin/env python3
"""Runs Adit's telnet client (examples/telnet_probe.rs) against real telnet
daemons through proxy.py and records what each daemon's pty actually received.

usage: drive.py PROBE PASSWORD_FILE
"""
import json, os, re, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
PROBE, PASSWORD_FILE = sys.argv[1], sys.argv[2]
USER = "telnettest"

OBSERVE = [  # (case, keystrokes handed to Adit, bytes the pty must receive)
    ("raw-cr", b"a\rb", b"a\rb"),
    ("raw-crlf", b"c\r\nd", None),  # recorded, daemon-defined
    ("raw-iac", b"x\xffy", b"x\xffy"),
    ("cooked", b"hello\r", b"hello\n"),
]
TARGETS = [
    ("netkit-observe", 2301, "observe"),
    ("inetutils-observe", 2302, "observe"),
    ("busybox-observe", 2303, "observe"),
    ("netkit-login", 23, "login"),
    ("inetutils-login", 2312, "login"),
    ("busybox-login", 2313, "login"),
]

def observe_script():
    lines = []
    for case, keys, _ in OBSERVE:
        lines += [f"WAIT {('READY ' + case).encode().hex()} 8000", f"SEND {keys.hex()}",
                  f"WAIT {('GOT ' + case).encode().hex()} 8000", "SLEEP 200"]
    return lines + [f"WAIT {b'DONE'.hex()} 5000", "CLOSE"]

def login_script():
    command = b"printf 'ok-%s\\n' $((6*7))\r"
    return [f"WAIT {b'ogin:'.hex()} 10000", f"SEND {(USER + chr(13)).encode().hex()}",
            f"WAIT {b'assword:'.hex()} 10000", "SEND - 0d", "@SECRET",
            f"WAIT {b'$ '.hex()} 10000", f"SEND {command.hex()}",
            f"WAIT {b'ok-42'.hex()} 8000", f"SEND {b'exit'.hex()}0d", "SLEEP 800", "CLOSE"]

def run(name, port, mode, secret):
    proxy_port = 30000 + port
    raw_wire = f"/tmp/adit-telnet-wire-{name}.jsonl"
    proxy = subprocess.Popen([sys.executable, os.path.join(HERE, "proxy.py"), str(proxy_port), str(port), raw_wire],
                             stdout=subprocess.PIPE, text=True)
    proxy.stdout.readline()
    script = observe_script() if mode == "observe" else login_script()
    stdin = "\n".join(secret if line == "@SECRET" else line for line in script) + "\n"
    probe = subprocess.run([PROBE, "127.0.0.1", str(proxy_port), "100", "30"], input=stdin,
                           capture_output=True, text=True, timeout=120)
    proxy.wait(timeout=40)
    with open(raw_wire) as handle:
        wire_text = handle.read()
    os.unlink(raw_wire)
    secret_hex = secret.encode().hex()
    wire_text = wire_text.replace(secret_hex, "<redacted-password>")
    with open(os.path.join(HERE, f"wire-{name}.jsonl"), "w") as handle:
        handle.write(wire_text)
    with open(os.path.join(HERE, f"probe-{name}.txt"), "w") as handle:
        handle.write(probe.stdout + probe.stderr)
    c2s = b"".join(bytes.fromhex(e["hex"]) for e in map(json.loads, wire_text.splitlines())
                   if e["dir"] == "c2s" and "<" not in e["hex"])
    output = b"".join(bytes.fromhex(l[4:]) for l in probe.stdout.splitlines() if l.startswith("OUT "))
    result = {"target": name, "port": port, "mode": mode,
              "matches": probe.stdout.count("MATCH\n"), "timeouts": probe.stdout.count("TIMEOUT\n")}
    if mode == "observe":
        cases = []
        for case, keys, expect in OBSERVE:
            found = re.search(rb"GOT " + case.encode() + rb" ([0-9a-f]*)", output)
            got = bytes.fromhex(found.group(1).decode()) if found else None
            cases.append({"case": case, "keys": keys.hex(), "pty_received": got.hex() if got is not None else None,
                          "expected": expect.hex() if expect else None,
                          "pass": None if expect is None else got == expect})
        result["cases"] = cases
        result["wire_has_a_CR_NUL_b"] = b"a\r\x00b" in c2s
        result["wire_has_hello_CR_NUL"] = b"hello\r\x00" in c2s
        result["wire_has_x_IAC_IAC_y"] = b"x\xff\xffy" in c2s
    else:
        result["login_and_command_ok"] = b"ok-42" in output
        result["wire_user_CR_NUL"] = (USER.encode() + b"\r\x00") in c2s
        result["password_redacted_in_wire"] = "<redacted-password>" in wire_text
    for path in os.listdir(HERE):
        with open(os.path.join(HERE, path), "rb") as handle:
            assert secret.encode() not in handle.read(), f"password leaked into {path}"
    return result

secret = open(PASSWORD_FILE).read().strip()
results = [run(*target, secret) for target in TARGETS]
with open(os.path.join(HERE, "summary.json"), "w") as handle:
    json.dump(results, handle, indent=2)
print(json.dumps(results, indent=2))
