"""Generate the FreeDM extension icons: a download arrow over a tray.

Pure stdlib PNG writing, so the repo needs no image toolchain to regenerate them.
"""
import struct
import sys
import zlib
from pathlib import Path

ACCENT = (77, 163, 255)   # --accent
TRAY = (139, 147, 165)    # --muted


def png(width, height, pixels):
    """Encode RGBA pixel rows into PNG bytes."""
    raw = b"".join(b"\x00" + bytes(v for px in row for v in px) for row in pixels)

    def chunk(tag, data):
        body = tag + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def coverage(x, y, shapes, samples=4):
    """Supersample a point against the shape predicates, returning (colour, alpha)."""
    hits = {}
    step = 1.0 / samples
    for sy in range(samples):
        for sx in range(samples):
            px = x + (sx + 0.5) * step
            py = y + (sy + 0.5) * step
            for colour, inside in shapes:
                if inside(px, py):
                    hits[colour] = hits.get(colour, 0) + 1
                    break
    if not hits:
        return None, 0
    colour = max(hits, key=hits.get)
    total = sum(hits.values())
    return colour, min(255, round(255 * total / (samples * samples)))


def build(size):
    """Draw the icon at `size` using a 24x24 design grid."""
    s = size / 24.0

    def thick_line(x1, y1, x2, y2, w):
        """Distance-to-segment test, giving a round-capped stroke."""
        dx, dy = x2 - x1, y2 - y1
        length_sq = dx * dx + dy * dy

        def inside(px, py):
            if length_sq == 0:
                t = 0.0
            else:
                t = max(0.0, min(1.0, ((px - x1) * dx + (py - y1) * dy) / length_sq))
            cx, cy = x1 + t * dx, y1 + t * dy
            return (px - cx) ** 2 + (py - cy) ** 2 <= (w / 2) ** 2

        return inside

    # Design in 24x24 units, scaled to the target size.
    def u(v):
        return v * s

    shaft = thick_line(u(12), u(3.2), u(12), u(14.4), u(2.5))
    barb_l = thick_line(u(12), u(14.6), u(7.5), u(10.1), u(2.5))
    barb_r = thick_line(u(12), u(14.6), u(16.5), u(10.1), u(2.5))
    tray_b = thick_line(u(4.6), u(19.6), u(19.4), u(19.6), u(2.4))
    tray_l = thick_line(u(4.0), u(16.6), u(4.0), u(19.0), u(2.4))
    tray_r = thick_line(u(20.0), u(16.6), u(20.0), u(19.0), u(2.4))

    shapes = [
        (ACCENT, lambda x, y: shaft(x, y) or barb_l(x, y) or barb_r(x, y)),
        (TRAY, lambda x, y: tray_b(x, y) or tray_l(x, y) or tray_r(x, y)),
    ]

    rows = []
    for y in range(size):
        row = []
        for x in range(size):
            colour, alpha = coverage(x, y, shapes)
            row.append((*(colour or (0, 0, 0)), alpha))
        rows.append(row)
    return png(size, size, rows)


def main():
    out = Path(sys.argv[1])
    out.mkdir(parents=True, exist_ok=True)
    for size in (16, 32, 48, 128):
        path = out / f"{size}.png"
        path.write_bytes(build(size))
        print(f"{path} ({path.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
