"""Draws Lumen's icon (black rounded tile, white Beam L mark) and writes res/lumen.ico (16..256 px PNG frames).
Pure standard library: python scripts/make_icon.py"""
import struct, zlib, os

def rrect(x, y, rx, ry, w, h, r):
    qx = abs(x - (rx + w / 2)) - (w / 2 - r)
    qy = abs(y - (ry + h / 2)) - (h / 2 - r)
    return (max(qx, 0) ** 2 + max(qy, 0) ** 2) ** 0.5 + min(max(qx, qy), 0) - r <= 0

def beam(x, y):
    # Beam L in a 64-unit box: stem, foot, and a dot (matches island orb()).
    return (rrect(x, y, 15, 10, 9.5, 44, 4.75) or rrect(x, y, 15, 44.5, 34, 9.5, 4.75)
            or (x - 44) ** 2 + (y - 21) ** 2 <= 5.25 ** 2)

def render(size, ss=4):
    n = size * ss
    px = bytearray(size * size * 4)
    r_tile = size * 0.24
    c = size / 2
    for y in range(size):
        for x in range(size):
            cov_tile = cov_orb = 0
            for sy in range(ss):
                for sx in range(ss):
                    fx, fy = x + (sx + 0.5) / ss, y + (sy + 0.5) / ss
                    qx = abs(fx - c) - (c - r_tile)
                    qy = abs(fy - c) - (c - r_tile)
                    d = (max(qx, 0) ** 2 + max(qy, 0) ** 2) ** 0.5 + min(max(qx, qy), 0) - r_tile
                    if d <= 0:
                        cov_tile += 1
                        if beam(fx * 64 / size, fy * 64 / size):
                            cov_orb += 1
            a = cov_tile / (ss * ss)
            v = cov_orb / (ss * ss)
            i = (y * size + x) * 4
            px[i:i + 4] = bytes([int(255 * v), int(255 * v), int(255 * v), int(255 * a)])  # RGBA, unpremultiplied white
    return bytes(px)

def png(size, rgba):
    raw = b"".join(b"\x00" + rgba[y * size * 4:(y + 1) * size * 4] for y in range(size))
    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")

sizes = [16, 24, 32, 48, 64, 128, 256]
frames = [(s, png(s, render(s))) for s in sizes]
out = struct.pack("<HHH", 0, 1, len(frames))
offset = 6 + 16 * len(frames)
body = b""
for s, data in frames:
    out += struct.pack("<BBBBHHII", 0 if s == 256 else s, 0 if s == 256 else s, 0, 0, 1, 32, len(data), offset + len(body))
    body += data
os.makedirs("crates/lumen/res", exist_ok=True)
open("crates/lumen/res/lumen.ico", "wb").write(out + body)
print("wrote crates/lumen/res/lumen.ico", len(out + body), "bytes")
