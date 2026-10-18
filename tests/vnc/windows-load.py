"""Owned synthetic Tk desktop for disposable Windows CI, with consumed-event proof.

No clipboard, keys, screenshots or user data. The new absolute --state-dir belongs
solely to this process. Touch reset.request there; wait for reset_generation to
increase in reset-ack.json before measuring another workload.
"""
import argparse
import json
import os
from pathlib import Path
import time


def pattern_row(y, width=1600):
    return "{" + " ".join("#%02x%02x%02x" % (
        (x * 13 + y * 3) % 256, (x * 3 + y * 7) % 256, (x ^ y) % 256
    ) for x in range(width)) + "}"


def atomic_json(path, data):
    temporary = path.with_name(path.name + ".tmp")
    temporary.write_text(json.dumps(data, sort_keys=True) + "\n", encoding="utf-8")
    # Windows readers may briefly deny replacement; retry only sharing/permission
    # failures, retaining a finite deadline so permanent write failures surface.
    deadline = time.monotonic() + .3
    while True:
        try:
            os.replace(temporary, path)
            return
        except PermissionError:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise
            time.sleep(min(.005, remaining))


def run(state_dir):
    import tkinter as tk

    if not state_dir.is_absolute():
        raise ValueError("--state-dir must be absolute and fresh")
    state_dir.mkdir(exist_ok=False)
    started = time.monotonic()
    root = None
    state = {"ready": False, "closed": False, "reset_generation": 0,
             "counts": {key: 0 for key in (
                 "wheel_events", "wheel_units", "button_press", "drag_motion",
                 "button_release", "viewport_changes")}}
    dirty = True
    try:
        root = tk.Tk()
        root.title("Adit public synthetic workload")
        root.overrideredirect(True)
        root.geometry("800x600+0+0")
        canvas = tk.Canvas(root, width=800, height=600, highlightthickness=0,
                           scrollregion=(0, 0, 1600, 1200))
        canvas.pack(fill="both", expand=True)
        state["canaries"] = [
            {"point": [40, 40], "rgb": [224, 48, 32]},
            {"point": [670, 470], "rgb": [32, 80, 224]},
        ]
        # Root-owned patches stay fixed while the canvas scrolls or drags.
        for x, y, color in [(20, 20, "#e03020"), (650, 450, "#2050e0")]:
            tk.Frame(root, background=color, borderwidth=0, highlightthickness=0).place(
                x=x, y=y, width=64, height=64)
        image = tk.PhotoImage(width=1600, height=1200)
        for y in range(1200):
            image.put(pattern_row(y), to=(0, y))
        canvas.create_image(0, 0, anchor="nw", image=image)
        for y in range(0, 1200, 60):
            canvas.create_text(10, y + 15, anchor="nw", fill="white",
                text=f"Synthetic row {y//60:02d} - VNC scroll and drag",
                font=("Courier", 16))

        def viewport():
            return {"x": list(canvas.xview()), "y": list(canvas.yview())}

        def consume(kind, operation, units=0):
            nonlocal dirty
            before = viewport()
            state["counts"][kind] += 1
            state["counts"]["wheel_units"] += abs(units)
            operation()
            if viewport() != before:
                state["counts"]["viewport_changes"] += 1
            dirty = True
            return "break"

        def wheel(event):
            delta = getattr(event, "delta", 0)
            number = getattr(event, "num", 0)
            units = (-max(1, abs(int(delta)) // 120) if delta > 0 else
                     max(1, abs(int(delta)) // 120) if delta < 0 else
                     -1 if number == 4 else 1)
            return consume("wheel_events", lambda: canvas.yview_scroll(units * 3, "units"), units)

        canvas.bind("<MouseWheel>", wheel)
        canvas.bind("<Button-4>", wheel)
        canvas.bind("<Button-5>", wheel)
        canvas.bind("<ButtonPress-1>", lambda e: consume(
            "button_press", lambda: canvas.scan_mark(e.x, e.y)))
        canvas.bind("<B1-Motion>", lambda e: consume(
            "drag_motion", lambda: canvas.scan_dragto(e.x, e.y, gain=1)))
        canvas.bind("<ButtonRelease-1>", lambda _e: consume("button_release", lambda: None))

        def publish():
            state["elapsed_ms"] = round((time.monotonic() - started) * 1000)
            state["viewport"] = viewport()
            state["focused"] = root.focus_get() is canvas
            atomic_json(state_dir / "state.json", state)

        def reset():
            nonlocal dirty
            canvas.xview_moveto(.25)
            canvas.yview_moveto(.25)
            root.update_idletasks()
            state["counts"] = dict.fromkeys(state["counts"], 0)
            state["reset_generation"] += 1
            dirty = True

        def tick():
            nonlocal dirty
            request = state_dir / "reset.request"
            if request.exists():
                reset()
                request.unlink()
                publish()
                atomic_json(state_dir / "reset-ack.json", state)
            if dirty:
                publish()
                dirty = False
            root.after(20, tick)

        root.update()
        canvas.xview_moveto(.25)
        canvas.yview_moveto(.25)
        root.lift()
        canvas.focus_force()
        root.update()
        state.update(ready=True, geometry={"x": root.winfo_rootx(), "y": root.winfo_rooty(),
            "width": root.winfo_width(), "height": root.winfo_height(),
            "screen_width": root.winfo_screenwidth(), "screen_height": root.winfo_screenheight()},
            pid=os.getpid())
        publish()
        atomic_json(state_dir / "ready.json", state)
        tick()
        root.mainloop()
    except Exception as error:
        state["error"] = type(error).__name__
        raise
    finally:
        state["closed"] = True
        atomic_json(state_dir / "state.json", state)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-dir", type=Path, required=True)
    run(parser.parse_args().state_dir)
