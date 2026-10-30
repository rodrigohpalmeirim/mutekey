"""Generate mutekey's icons. Run from anywhere: python3 icons/make_icons.py

Every size is drawn directly from signed-distance shapes (not scaled down from a big
image), so small sizes stay crisp. Writes into this directory:
  mutekey.ico          app icon: a dark keycap with a red mic-off symbol
  live/muted/none.ico  tray icons: white mic on a green/red/gray circle
  badge-muted.ico      white mic-off glyph drawn on the badge
  badge-keyboard.ico   white keyboard glyph for the "press the new shortcut" prompt
  preview.png          all of the above, enlarged, for eyeballing
"""

import struct
import sys
from io import BytesIO
from pathlib import Path

import numpy as np
from PIL import Image

HERE = Path(__file__).resolve().parent

RED = (196, 43, 28)
BRIGHT_RED = (232, 72, 58)
GREEN = (16, 160, 72)
GRAY = (128, 128, 128)
WHITE = (255, 255, 255)

APP_SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]
TRAY_SIZES = [16, 20, 24, 28, 32, 40, 48, 64]
# The badge glyph is 18 px at 100% scaling; these cover 100% to 250%.
GLYPH_SIZES = [18, 20, 23, 27, 32, 36, 41, 45]


class Canvas:
    """Shapes are given in a [-1, 1] square; distances are converted to pixels for anti-aliasing."""

    def __init__(self, size):
        self.size = size
        c = (np.arange(size) + 0.5) / size * 2 - 1
        self.x, self.y = np.meshgrid(c, c)
        self.px = size / 2  # pixels per unit
        self.rgba = np.zeros((size, size, 4))

    def coverage(self, distance):
        return np.clip(0.5 - distance * self.px, 0, 1)

    def paint(self, distance, color, opacity=1.0):
        a = self.coverage(distance)[..., None] * opacity
        src = np.array(color, dtype=float) / 255
        dst_a = self.rgba[..., 3:4]
        out_a = a + dst_a * (1 - a)
        rgb = (src * a + self.rgba[..., :3] * dst_a * (1 - a)) / np.maximum(out_a, 1e-6)
        self.rgba = np.concatenate([rgb, out_a], axis=-1)

    def paint_gradient(self, distance, top, bottom, y0, y1):
        t = np.clip((self.y - y0) / (y1 - y0), 0, 1)[..., None]
        color = np.array(top, float) * (1 - t) + np.array(bottom, float) * t
        a = self.coverage(distance)[..., None]
        dst_a = self.rgba[..., 3:4]
        out_a = a + dst_a * (1 - a)
        rgb = (color / 255 * a + self.rgba[..., :3] * dst_a * (1 - a)) / np.maximum(out_a, 1e-6)
        self.rgba = np.concatenate([rgb, out_a], axis=-1)

    def image(self):
        return Image.fromarray((self.rgba * 255 + 0.5).clip(0, 255).astype(np.uint8))


# Signed distance functions: negative inside.

def circle(c, cx, cy, r):
    return np.hypot(c.x - cx, c.y - cy) - r


def rounded_rect(c, cx, cy, hw, hh, r):
    qx = np.abs(c.x - cx) - (hw - r)
    qy = np.abs(c.y - cy) - (hh - r)
    return np.hypot(np.maximum(qx, 0), np.maximum(qy, 0)) + np.minimum(np.maximum(qx, qy), 0) - r


def segment(c, x0, y0, x1, y1, width):
    px, py = c.x - x0, c.y - y0
    dx, dy = x1 - x0, y1 - y0
    t = np.clip((px * dx + py * dy) / (dx * dx + dy * dy), 0, 1)
    return np.hypot(px - t * dx, py - t * dy) - width / 2


def lower_arc(c, cx, cy, r, width):
    """The bottom half of a ring, with round ends."""
    ring = np.abs(np.hypot(c.x - cx, c.y - cy) - r) - width / 2
    ends = np.minimum(np.hypot(c.x - (cx - r), c.y - cy), np.hypot(c.x - (cx + r), c.y - cy)) - width / 2
    return np.where(c.y >= cy, ring, ends)


def union(*ds):
    return np.minimum.reduce(ds)


def subtract(d, cut):
    return np.maximum(d, -cut)


def microphone(c, scale, oy=0.0, muted=False, weight=0.09):
    """Capsule, holder and stand, about 1.6 * `scale` units tall. Strokes are whole pixels and
    the stem and base sit on the pixel grid, so small sizes stay sharp. With `muted`, a slash
    with a clear gap on both sides."""
    s = scale
    stroke_px = max(1, round(weight * s * c.size))
    w = stroke_px / c.px

    def snap(v):  # line centers on pixel centers for odd widths, on pixel edges for even ones
        p = (v + 1) * c.px
        p = np.floor(p) + 0.5 if stroke_px % 2 else np.round(p)
        return p / c.px - 1

    ox, base_y = snap(0.0), snap(oy + 0.78 * s)
    body = rounded_rect(c, ox, oy - 0.33 * s, 0.27 * s, 0.5 * s, 0.27 * s)
    holder = lower_arc(c, ox, oy - 0.08 * s, 0.5 * s, w)
    stem = segment(c, ox, oy + 0.4 * s, ox, base_y, w)
    base = segment(c, ox - 0.3 * s, base_y, ox + 0.3 * s, base_y, w)
    mic = union(body, holder, stem, base)
    if muted:
        a, b = (ox - 0.66 * s, oy - 0.74 * s), (ox + 0.66 * s, oy + 0.74 * s)
        slash = segment(c, *a, *b, w)
        # The gap is only on the slash's lower-left side; on both sides it shreds small sizes.
        gap = segment(c, *a, *b, w + 2 * max(0.5 * w, 1 / c.px))
        nx, ny = (a[1] - b[1]), (b[0] - a[0])  # normal pointing up-right
        upper_right = ((c.x - a[0]) * nx + (c.y - a[1]) * ny) / np.hypot(nx, ny)
        mic = union(subtract(mic, np.maximum(gap, upper_right)), slash)
    return mic


def keyboard(c):
    stroke = max(0.16, 2 / c.px)
    outline = np.abs(rounded_rect(c, 0, 0, 0.92, 0.56, 0.16)) - stroke / 2
    keys = []
    for row, y in enumerate((-0.24, 0.02)):
        for i in range(5):
            x = -0.56 + i * 0.28
            keys.append(rounded_rect(c, x, y, 0.07, 0.07, 0.03))
    keys.append(segment(c, -0.42, 0.3, 0.42, 0.3, stroke))
    return union(outline, *keys)


def app_icon(size):
    c = Canvas(size)
    if size <= 24:
        # Too small for the keycap's shading to read: a red tile with a white mic-off.
        c.paint(rounded_rect(c, 0, 0, 0.94, 0.94, 0.3), RED)
        c.paint(microphone(c, 0.64, 0.02, muted=True), WHITE)
        return c.image()
    # Keycap: dark skirt, lighter top face, soft shadow underneath.
    c.paint(rounded_rect(c, 0, 0.06, 0.9, 0.88, 0.26) - 0.03, (0, 0, 0), opacity=0.35)
    c.paint(rounded_rect(c, 0, 0, 0.9, 0.9, 0.24), (30, 32, 35))
    c.paint_gradient(rounded_rect(c, 0, -0.1, 0.74, 0.7, 0.18), (78, 83, 90), (56, 60, 65), -0.8, 0.6)
    c.paint(microphone(c, 0.5, -0.08, muted=True), BRIGHT_RED)
    return c.image()


def tray_icon(size, color, muted):
    c = Canvas(size)
    c.paint(circle(c, 0, 0, 1 - 0.5 / c.px), color)
    c.paint(microphone(c, 0.62, 0.03, muted=muted, weight=0.11), WHITE)
    return c.image()


def glyph_icon(size, draw):
    c = Canvas(size)
    c.paint(draw(c), WHITE)
    return c.image()


def write_ico(path, images):
    """PNG-compressed entries, one per size (supported since Windows Vista)."""
    blobs = []
    for img in images:
        buf = BytesIO()
        img.save(buf, "PNG")
        blobs.append(buf.getvalue())
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries = b""
    for img, blob in zip(images, blobs):
        w = img.width if img.width < 256 else 0
        entries += struct.pack("<BBBBHHII", w, w, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
    path.write_bytes(header + entries + b"".join(blobs))


def main():
    sets = {
        "mutekey": [app_icon(s) for s in APP_SIZES],
        "live": [tray_icon(s, GREEN, False) for s in TRAY_SIZES],
        "muted": [tray_icon(s, RED, True) for s in TRAY_SIZES],
        "none": [tray_icon(s, GRAY, False) for s in TRAY_SIZES],
        "badge-muted": [glyph_icon(s, lambda c: microphone(c, 0.92, 0.0, muted=True, weight=0.12)) for s in GLYPH_SIZES],
        "badge-keyboard": [glyph_icon(s, keyboard) for s in GLYPH_SIZES],
    }
    for name, images in sets.items():
        write_ico(HERE / f"{name}.ico", images)

    # Preview: each set on light and dark backgrounds, small sizes enlarged 4x (nearest).
    rows = []
    for name, images in sets.items():
        for bg in ((243, 243, 243), (32, 32, 32), (196, 43, 28)) if name.startswith("badge") else ((243, 243, 243), (32, 32, 32)):
            cells = []
            for img in images:
                zoom = 4 if img.width <= 64 else 1
                big = img.resize((img.width * zoom, img.height * zoom), Image.NEAREST)
                cell = Image.new("RGBA", (max(big.width, 64) + 16, 272), bg + (255,))
                cell.alpha_composite(big, (8, (272 - big.height) // 2))
                cells.append(cell)
            row = Image.new("RGBA", (sum(c.width for c in cells), 272))
            x = 0
            for cell in cells:
                row.paste(cell, (x, 0))
                x += cell.width
            rows.append(row)
    sheet = Image.new("RGBA", (max(r.width for r in rows), sum(r.height for r in rows)), (255, 255, 255, 255))
    y = 0
    for row in rows:
        sheet.paste(row, (0, y))
        y += row.height
    sheet.save(sys.argv[1] if len(sys.argv) > 1 else HERE / "preview.png")


if __name__ == "__main__":
    main()
