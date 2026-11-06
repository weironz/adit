"""Read-only cross-check of the #230 glyph-ghosting evidence.

Every number in README.md is recomputed here from the archived PNGs, so a
claim in the write-up cannot drift away from the pixels it describes.

The measurement is deliberately model-free. Nothing here asks what sampler
produced a capture; it asks two questions the pixels answer on their own:

  1. COLOURS. Nearest-neighbour copies texels, so every colour it draws was
     already a pixel of the framebuffer. A resampling filter blends
     neighbours, so it invents colours the framebuffer never had. The
     framebuffer's colour set is computed from the fixture itself, so the
     test does not depend on any rendering.

  2. DOUBLED COLUMNS. Nearest-neighbour maps one source column to ceil(f) or
     floor(f) device columns, so at a fractional magnification a share of
     the device columns are byte-identical copies of the column to their
     left. A resampling filter invents a value per column, so essentially
     none are. A copied column means the stroke was drawn twice, which is
     the reported 「字体有重影」.

The surface is located by the fixture's own cyan border (0,240,220), which
is three source pixels thick: the border's outer edge IS the surface's
edge, so the last column that is part of the border is the surface's right
edge and the rest follows from the fit factor.

Requires numpy and Pillow.
"""
import json
import os
import pathlib
import sys

import numpy as np
from PIL import Image

ROOT = pathlib.Path(__file__).resolve().parent
FIXTURE = pathlib.Path(__file__).resolve().parents[2]

SOURCE_WIDTH = 1200
SOURCE_HEIGHT = 800
# The fixture's text canvas, in source pixels: x 40..599, y 150..245.
CANVAS_SOURCE = dict(left=40, top=150, right=600, bottom=246)


def load(name):
    return np.asarray(Image.open(ROOT / name).convert("RGB")).astype(np.int64)


def framebuffer_colours():
    """The fixture's own colour set - the ground truth for the test."""
    os.environ.setdefault("ADIT_FIXTURE_TEXT", "1")
    sys.path.insert(0, str(FIXTURE))
    import native_rfb_fixture as fixture

    data = fixture.grid(SOURCE_WIDTH, SOURCE_HEIGHT, fixture.PF, 1)
    values = np.frombuffer(data, dtype="<u4").reshape(SOURCE_HEIGHT, SOURCE_WIDTH)
    rgb = np.stack([values & 255, (values >> 8) & 255, (values >> 16) & 255],
                   axis=2).astype(np.uint8)
    return {tuple(int(c) for c in row) for row in
            np.unique(rgb.reshape(-1, 3), axis=0)}


def cyan_mask(array):
    """The fixture's border colour, (0, 240, 220)."""
    r, g, b = array[:, :, 0], array[:, :, 1], array[:, :, 2]
    return (r < 70) & (g > 195) & (g < 255) & (b > 175) & (b < 245)


def surface_rect(array, factor):
    """The surface's edges, from the fixture's cyan border."""
    cyan = cyan_mask(array)
    column_counts = cyan.sum(axis=0)
    row_counts = cyan.sum(axis=1)
    peak_column = int(column_counts.max())
    peak_row = int(row_counts.max())
    border_columns = [int(x) for x in
                      np.nonzero(column_counts >= 0.9 * peak_column)[0]]
    border_rows = [int(y) for y in
                   np.nonzero(row_counts >= 0.9 * peak_row)[0]]
    right = max(border_columns)
    bottom = max(border_rows)
    return dict(left=right - round(SOURCE_WIDTH * factor) + 1,
                top=bottom - round(SOURCE_HEIGHT * factor) + 1,
                right=right, bottom=bottom)


def duplication(crop):
    """The share of columns that are byte-identical copies of their left."""
    identical = np.all(crop[:, 1:, :] == crop[:, :-1, :], axis=2)
    copied = np.zeros(crop.shape[1], dtype=bool)
    copied[1:] = identical.all(axis=0)
    ink = np.any(crop != 0, axis=2).any(axis=0)
    ink_count = int(ink.sum())
    copy_count = int((copied & ink).sum())
    return ink_count, copy_count


def analyse(name, factor, framebuffer_set):
    array = load(name)
    surface = surface_rect(array, factor)
    canvas = dict(
        left=surface["left"] + round(CANVAS_SOURCE["left"] * factor),
        top=surface["top"] + round(CANVAS_SOURCE["top"] * factor),
        right=surface["left"] + round(CANVAS_SOURCE["right"] * factor),
        bottom=surface["top"] + round(CANVAS_SOURCE["bottom"] * factor))
    inset = max(3, int(round(factor)) + 1)
    crop = array[canvas["top"] + inset:canvas["bottom"] - inset,
                 canvas["left"] + inset:canvas["right"] - inset]
    colours = {tuple(int(c) for c in row) for row in
               np.unique(crop.reshape(-1, 3), axis=0)}
    ink_count, copy_count = duplication(crop)
    return dict(
        distinct_colours=len(colours),
        colours_not_in_the_framebuffer=len(colours - framebuffer_set),
        ink_columns=ink_count,
        copied_ink_columns=copy_count,
        copy_fraction=round(copy_count / ink_count, 5) if ink_count else 0.0)


def main():
    framebuffer_set = framebuffer_colours()
    recorded = json.loads((ROOT / "analysis.json").read_text(encoding="utf-8"))

    # The magnified fit factor the run itself recorded.
    factor = None
    for line in (ROOT / "before-events.jsonl").read_text(
            encoding="utf-8", errors="replace").splitlines():
        event = json.loads(line)
        if event.get("event") == "capture" and event.get("label") == "fit":
            factor = float(event["fit_factor"][0])
    assert factor is not None, "the archived log records no magnified capture"

    cases = [
        ("before-fit.png", factor, "fit, nearest-neighbour"),
        ("after-fit.png", factor, "fit, resampled"),
        ("before-1x1.png", 1.0, "source (1:1)"),
    ]
    results = {}
    for name, scale, key in cases:
        measured = analyse(name, scale, framebuffer_set)
        results[key] = measured
        expected = recorded[key]
        for field, value in measured.items():
            assert value == expected[field], (key, field, value, expected[field])

    nearest = results["fit, nearest-neighbour"]
    resampled = results["fit, resampled"]
    source = results["source (1:1)"]

    # The colour-set test: nearest copied, resampling invented.
    assert nearest["colours_not_in_the_framebuffer"] == 0, nearest
    assert resampled["colours_not_in_the_framebuffer"] > 0, resampled

    # The doubling test: the source has none, nearest has some, resampling
    # has none.
    assert source["copy_fraction"] == 0.0, source
    assert nearest["copy_fraction"] > resampled["copy_fraction"], results

    # The 1:1 control: the surface itself must not change between the two
    # binaries. What does differ is the status bar, which carries the
    # session's own text and changes from run to run of the SAME binary.
    before_1x1 = load("before-1x1.png")
    after_1x1 = load("after-1x1.png")
    difference = np.abs(before_1x1 - after_1x1).max(axis=2)
    count = int((difference > 0).sum())
    ys, xs = np.nonzero(difference > 0)
    box = [int(xs.min()), int(ys.min()), int(xs.max()), int(ys.max())]
    assert count == recorded["one_to_one_differing_pixels"], count
    assert box == recorded["one_to_one_difference_box"], box
    # The difference is BELOW the surface: the surface ends at y 970 in the
    # 1:1 capture, so nothing inside it moved.
    assert ys.min() > 970, ys.min()

    print(f"the framebuffer holds {len(framebuffer_set)} distinct colours")
    for key, value in results.items():
        print(f"  {key:<26} colours={value['distinct_colours']:<5} "
              f"not-in-framebuffer={value['colours_not_in_the_framebuffer']:<5} "
              f"copied glyph columns={value['copied_ink_columns']}/"
              f"{value['ink_columns']} ({value['copy_fraction']})")
    print(f"the 1:1 control differs by {count} px at x {box[0]}..{box[2]} "
          f"y {box[1]}..{box[3]} - below the surface, in the status bar")
    print("OK: every number in analysis.json was recomputed and matches")


if __name__ == "__main__":
    main()
