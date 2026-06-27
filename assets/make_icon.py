"""Generate the Adit app icon and all build inputs.

Run from the repository root:  python assets/make_icon.py

Emits:
  assets/icon.png                    master image (docs / reference)
  crates/adit-app/assets/icon.ico    multi-size Windows exe-resource icon
  crates/adit-ui/assets/icon.rgba    raw 256x256 RGBA for iced window::icon

The icon is a blue rounded tile with a white `>_` terminal prompt. It is
rendered at 4x and downsampled for smooth edges.
"""
from PIL import Image, ImageDraw
import os

S = 4               # supersample factor
N = 256             # final size
W = N * S

TOP = (59, 130, 246)     # #3B82F6
BOT = (29, 78, 216)      # #1D4ED8
INK = (255, 255, 255)


def lerp(a, b, t):
    return tuple(round(a[i] + (b[i] - a[i]) * t) for i in range(3))


def build():
    # vertical gradient
    grad = Image.new("RGB", (1, W))
    for y in range(W):
        grad.putpixel((0, y), lerp(TOP, BOT, y / (W - 1)))
    grad = grad.resize((W, W))

    # rounded-tile mask
    margin = 10 * S
    radius = 58 * S
    mask = Image.new("L", (W, W), 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        [margin, margin, W - margin, W - margin], radius=radius, fill=255
    )

    img = Image.new("RGBA", (W, W), (0, 0, 0, 0))
    img.paste(grad, (0, 0), mask)

    # subtle top sheen
    sheen = Image.new("RGBA", (W, W), (0, 0, 0, 0))
    ImageDraw.Draw(sheen).rounded_rectangle(
        [margin, margin, W - margin, margin + 96 * S], radius=radius, fill=(255, 255, 255, 26)
    )
    sheen.putalpha(Image.composite(sheen.getchannel("A"), Image.new("L", (W, W), 0), mask))
    img = Image.alpha_composite(img, sheen)

    # terminal prompt  >_
    d = ImageDraw.Draw(img)
    stroke = 24 * S

    def dot(x, y, r):
        d.ellipse([x - r, y - r, x + r, y + r], fill=INK)

    def thick_line(p0, p1, w):
        d.line([p0, p1], fill=INK, width=w)
        dot(*p0, w // 2)
        dot(*p1, w // 2)

    a, b, c = (82 * S, 92 * S), (132 * S, 128 * S), (82 * S, 164 * S)
    thick_line(a, b, stroke)
    thick_line(b, c, stroke)
    dot(*b, stroke // 2)
    d.rounded_rectangle([142 * S, 150 * S, 198 * S, 170 * S], radius=10 * S, fill=INK)

    return img.resize((N, N), Image.LANCZOS)


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    root = os.path.dirname(here)
    icon = build()

    png = os.path.join(root, "assets", "icon.png")
    ico = os.path.join(root, "crates", "adit-app", "assets", "icon.ico")
    rgba = os.path.join(root, "crates", "adit-ui", "assets", "icon.rgba")
    for p in (png, ico, rgba):
        os.makedirs(os.path.dirname(p), exist_ok=True)

    icon.save(png)
    icon.save(ico, sizes=[(16, 16), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
    with open(rgba, "wb") as f:
        f.write(icon.tobytes())  # raw RGBA, 256*256*4 bytes
    print(f"wrote {png}\n      {ico}\n      {rgba}")


if __name__ == "__main__":
    main()
