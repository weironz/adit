"""Deterministic local-only synthetic desktop for scroll/drag and Tight/JPEG tests."""
import tkinter as tk
import os
from pathlib import Path
root = tk.Tk()
root.title("Adit VNC synthetic workload")
root.geometry("800x600+0+0")
canvas = tk.Canvas(root, width=800, height=600, highlightthickness=0, scrollregion=(0, 0, 1600, 1200))
canvas.pack(fill="both", expand=True)
image = tk.PhotoImage(width=1600, height=1200)
for y in range(1200):
    colors = ["#%02x%02x%02x" % ((x*13+y*3)%256, (x*3+y*7)%256, (x^y)%256) for x in range(1600)]
    image.put("{"+" ".join(colors)+"}", to=(0,y))
canvas.create_image(0, 0, anchor="nw", image=image)
for y in range(0,1200,60):
    canvas.create_text(10,y+15,anchor="nw",text=f"Synthetic text row {y//60:02d} - VNC scroll and drag", fill="white", font=("monospace",16))
canvas.bind("<Button-4>", lambda _: canvas.yview_scroll(-3,"units"))
canvas.bind("<Button-5>", lambda _: canvas.yview_scroll(3,"units"))
canvas.bind("<ButtonPress-1>", lambda e: canvas.scan_mark(e.x,e.y))
canvas.bind("<B1-Motion>", lambda e: canvas.scan_dragto(e.x,e.y,gain=1))
canvas.focus_set()
display = os.environ.get("DISPLAY", ":99").lstrip(":")
reset_path = Path(f"/tmp/adit-reset-{display}")
def reset():
    if reset_path.exists():
        canvas.xview_moveto(0); canvas.yview_moveto(0)
        root.update_idletasks()
        reset_path.unlink()
    root.after(20, reset)
root.update()
Path(f"/tmp/adit-ready-{display}").touch()
reset()
root.mainloop()
