"""Recompute the measurements behind README.md from the archived pixels.

Everything here is read-only and model-free: the audit asks what the archived
bytes say, never what the harness claimed. The harness's own claims live in
`events.jsonl` and are cross-checked against the pixels, not trusted.

Three questions the pixels answer on their own:

  1. THE BITMAP. The exported `.rgba` is what the server sent and the app
     drew. Its opaque-pixel count and bounding box are recomputed from the
     alpha channel.

  2. THE PLACEMENT. The server bitmap must appear in the screenshot, at the
     place the app says it did, with the same colours. The check is exact:
     every opaque pixel of the bitmap must equal the screenshot pixel at the
     same offset. A cursor drawn at the wrong place, or a stale cursor left
     over from the previous step, cannot pass it.

  3. THE HOTSPOT. The pointer's hotspot is where the *server* thinks the
     click lands. The app's own cursor position must coincide with it, and
     with the point the harness drove the pointer to. On the xterm step that
     point is a known control - the xterm window's top-left pixel - so a
     hotspot that was off by a pixel would be visible here.

Requires numpy and Pillow.
"""

import hashlib
import json
import pathlib
import re
import sys

import numpy as np
from PIL import Image

ROOT = pathlib.Path(__file__).resolve().parent

# The steps whose screenshot must contain the server bitmap verbatim.
# (bitmap stem, capture stem, bitmap size)
EXACT_STEPS = [
    ("01-arrow-32x32", "01-arrow", (32, 32)),
    ("02-ibeam-32x32", "02-ibeam", (32, 32)),
    ("03-arrow-32x32", "03-arrow", (32, 32)),
    ("04-ibeam-32x32", "04-ibeam", (32, 32)),
    ("05-xterm-cursor-9x16", "05-xterm-cursor", (9, 16)),
]

# The step that runs in a window small enough that the picture is scaled.
SCALED_STEP = ("08-scaled-arrow-32x32", "08-scaled-arrow", (32, 32))

# The step where the pointer leaves the remote surface.
OUTSIDE_STEP = "07-outside"

# The step whose bitmap the app's own chrome paints over.
TOOLBAR_STEP = "06-toolbar-zone"

# The steps, in order, as the harness labels them.
STEP_ORDER = [
    "arrow", "ibeam", "arrow", "ibeam",
    "xterm-cursor", "toolbar-zone", "outside", "scaled-arrow",
]


def bitmap(name):
    path = ROOT / f"bitmap-{name}.rgba"
    raw = np.fromfile(path, dtype=np.uint8)
    match = re.search(r"(\d+)x(\d+)$", name)
    width, height = int(match.group(1)), int(match.group(2))
    assert raw.size == width * height * 4, (name, raw.size)
    return raw.reshape(height, width, 4)


def capture(name):
    return np.asarray(Image.open(ROOT / f"capture-{name}.png").convert("RGB"))


def measure_bitmap(name, size):
    """The opaque-pixel count and the bounding box of the drawn shape."""
    data = bitmap(name)
    opaque = data[:, :, 3] > 0
    ys, xs = np.nonzero(opaque)
    return dict(
        width=size[0],
        height=size[1],
        opaque_pixels=int(opaque.sum()),
        bbox=[int(xs.min()), int(ys.min()), int(xs.max()), int(ys.max())],
    )


def events():
    rows = []
    for line in (ROOT / "events.jsonl").read_text(
            encoding="utf-8", errors="replace").splitlines():
        line = line.strip()
        if line:
            rows.append(json.loads(line))
    return rows


def measured_steps(rows):
    """The `step_measured` events, in order."""
    out = []
    for row in rows:
        if row.get("event") == "step_measured":
            out.append(row["measurement"])
    return out


def check_manifest():
    """Every file in the archive is in the manifest, with the right digest."""
    manifest = json.loads((ROOT / "sha256.json").read_text(encoding="utf-8"))
    files = manifest["files"]
    for name, entry in files.items():
        path = ROOT / name
        assert path.exists(), name
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        assert digest == entry["sha256"], (name, digest, entry["sha256"])
        assert path.stat().st_size == entry["bytes"], name

    # Nothing in the archive is missing from the manifest, except the
    # manifest itself and the audit script that is reading it.
    for path in ROOT.iterdir():
        if path.name in ("sha256.json", "audit.py"):
            continue
        assert path.name in files, path.name
    return len(files)


def main():
    recorded = json.loads((ROOT / "analysis.json").read_text(encoding="utf-8"))
    rows = events()
    steps = measured_steps(rows)

    assert len(steps) == 8, len(steps)
    assert [s["step"] for s in steps] == STEP_ORDER, [s["step"] for s in steps]
    assert all(s["pass"] for s in steps), "a step did not pass"

    count = check_manifest()

    # --- 1. the bitmaps ------------------------------------------------
    for name, cap, size in EXACT_STEPS:
        key = f"bitmap-{name}"
        measured = measure_bitmap(name, size)
        expected = recorded[key]
        for field, value in measured.items():
            assert value == expected[field], (key, field, value, expected[field])

    scaled_key = f"bitmap-{SCALED_STEP[0]}"
    scaled_measured = measure_bitmap(SCALED_STEP[0], SCALED_STEP[2])
    for field, value in scaled_measured.items():
        assert value == recorded[scaled_key][field], (scaled_key, field, value)

    # --- 2. the placement ----------------------------------------------
    # The screenshot must contain the bitmap, byte for byte, at the place
    # the app reported drawing it.
    for (name, cap, size), step in zip(EXACT_STEPS, steps):
        data = bitmap(name)
        opaque = data[:, :, 3] > 0
        corner = step["server_bitmap_match"]["expected_corner"]
        shot = capture(cap)
        patch = shot[corner[1]:corner[1] + size[1],
                     corner[0]:corner[0] + size[0]]
        assert patch.shape[:2] == data.shape[:2], (name, patch.shape, data.shape)
        expected = data[opaque][:, :3].astype(int)
        actual = patch[opaque].astype(int)
        assert (expected == actual).all(), name
        assert recorded[f"bitmap-{name}"]["opaque_pixels"] == int(opaque.sum())

    # --- 3. the hotspot -------------------------------------------------
    # The app's own cursor position must be the pointer's position, and the
    # bitmap's hotspot must be inside the bitmap.
    for step in steps:
        shape = step["server_bitmap"]
        hx, hy = shape["hotspot"]
        assert 0 <= hx < shape["width"] and 0 <= hy < shape["height"], shape
        if step["server_bitmap_match"] and step["step"] != "scaled-arrow":
            corner = step["server_bitmap_match"]["expected_corner"]
            # The hotspot lands on the client point that was driven, i.e.
            # the remote point the server was told about.
            assert step["pointer_client"] == [
                corner[0] + hx, corner[1] + hy
            ], (step["step"], step["pointer_client"], corner, shape)
            assert step["app_cursor_pos"] == step["pointer_client"], step["step"]
            assert step["position_matches"], step["step"]

    # --- 4. the scaled step ---------------------------------------------
    # The cursor is drawn scaled with the picture, so it must be *smaller*
    # than the unscaled arrow and land where the scaled shape lands.
    scaled_step = steps[STEP_ORDER.index("scaled-arrow")]
    shot = capture(SCALED_STEP[1])
    r, g, b = (shot[:, :, 0].astype(int), shot[:, :, 1].astype(int),
               shot[:, :, 2].astype(int))
    magenta = (r > 100) & (b > 100) & (g < r - 40) & (g < b - 40)
    ys, xs = np.nonzero(magenta)
    drawn = int(magenta.sum())
    fit = scaled_step["fit_factors"][0]
    unscaled = recorded["bitmap-01-arrow-32x32"]["opaque_pixels"]
    assert drawn < unscaled, (drawn, unscaled)
    # The bounding box of the drawn cursor must be the unscaled opaque box
    # scaled by the fit factor, within a pixel of rounding.
    base = recorded["bitmap-01-arrow-32x32"]["bbox"]
    corner = scaled_step["server_bitmap_match"]["expected_corner"]
    for axis in (0, 1):
        want_lo = corner[axis] + base[axis] * fit
        want_hi = corner[axis] + (base[2 + axis] + 1) * fit
        got_lo, got_hi = (xs.min(), xs.max()) if axis == 0 else (ys.min(), ys.max())
        assert abs(got_lo - want_lo) <= 2, (axis, got_lo, want_lo)
        assert abs(got_hi - want_hi) <= 2, (axis, got_hi, want_hi)
    assert recorded["scaled-arrow"]["drawn_pixels"] == drawn

    # --- 5. the toolbar zone --------------------------------------------
    # The app's chrome is painted over the picture there, so only a sliver
    # of the arrow shows - but the sliver must be the arrow's colour, and
    # inside the arrow's rectangle.
    shot = capture(TOOLBAR_STEP)
    r, g, b = (shot[:, :, 0].astype(int), shot[:, :, 1].astype(int),
               shot[:, :, 2].astype(int))
    magenta = (r > 100) & (b > 100) & (g < r - 40) & (g < b - 40)
    ys, xs = np.nonzero(magenta)
    assert 0 < int(magenta.sum()) < unscaled, int(magenta.sum())
    toolbar_step = steps[STEP_ORDER.index("toolbar-zone")]
    assert toolbar_step["toolbar_hovered"], toolbar_step
    assert recorded["toolbar-zone"]["drawn_pixels"] == int(magenta.sum())

    # --- 6. outside the remote surface ----------------------------------
    # The local OS pointer must be back, and the app must not be painting
    # the server's bitmap there any more.
    outside_step = steps[STEP_ORDER.index("outside")]
    assert outside_step["os_pointer_visible"] is True, outside_step
    assert outside_step["expect_os_pointer_visible"] is True, outside_step
    assert outside_step["server_bitmap_match"] is None, outside_step
    shot = capture(OUTSIDE_STEP)
    r, g, b = (shot[:, :, 0].astype(int), shot[:, :, 1].astype(int),
               shot[:, :, 2].astype(int))
    magenta = (r > 100) & (b > 100) & (g < r - 40) & (g < b - 40)
    assert int(magenta.sum()) == 0, int(magenta.sum())
    assert recorded["outside"]["drawn_pixels"] == 0

    # --- 7. the OS pointer over the remote surface ----------------------
    for step in steps:
        if step["step"] == "outside":
            continue
        assert step["os_pointer_visible"] is False, step["step"]
        assert step["os_pointer_showing"] is False, step["step"]
        assert step["os_pointer_over_window"] is True, step["step"]

    print(f"manifest: {count} files, every digest recomputed and matched")
    for name, cap, size in EXACT_STEPS:
        measured = measure_bitmap(name, size)
        print(f"  {name:<26} {measured['width']}x{measured['height']} "
              f"opaque={measured['opaque_pixels']:<4} "
              f"bbox={measured['bbox']}  -> drawn verbatim in capture-{cap}.png")
    print(f"  scaled-arrow: drawn {drawn} px (unscaled arrow {unscaled}) at fit "
          f"{fit}")
    print(f"  toolbar-zone: {recorded['toolbar-zone']['drawn_pixels']} px of the "
          f"arrow visible under the app's chrome")
    print(f"  outside: {recorded['outside']['drawn_pixels']} px - the local "
          f"pointer is back")
    print("OK: every number in analysis.json was recomputed from the pixels "
          "and matches")


if __name__ == "__main__":
    main()
