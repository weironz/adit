"""ASCII control pipe for an isolated X11 clipboard. Test text travels as UTF-8 hex."""
import os, queue, sys, threading, tkinter as tk
os.environ["DISPLAY"] = ":" + sys.argv[1]
root=tk.Tk(); root.withdraw()
commands=queue.Queue()
owner = None
def reader():
    try:
        for line in sys.stdin: commands.put(line.rstrip("\r\n"))
    finally: commands.put("QUIT")
threading.Thread(target=reader,daemon=True).start()
def tick():
    global owner
    while not commands.empty():
        line=commands.get()
        if line=="QUIT":
            if owner is not None: owner.destroy()
            root.destroy(); return
        try:
            if line.startswith("SET "):
                value=bytes.fromhex(line[4:]).decode("utf-8")
                previous = owner
                owner = tk.Tk(); owner.withdraw()
                owner.clipboard_clear(); owner.clipboard_append(value)
                owner.update()
                if previous is not None: previous.destroy()
                print("OK",flush=True)
            elif line in ("GET", "GET_LATIN1"):
                try: print("TEXT "+root.clipboard_get(**({"type":"STRING"} if line=="GET_LATIN1" else {})).encode("utf-8").hex(),flush=True)
                except tk.TclError: print("NONE",flush=True)
            else: print("ERR",flush=True)
        except Exception: print("ERR",flush=True)
    root.after(20,tick)
print("READY",flush=True)
tick(); root.mainloop()
