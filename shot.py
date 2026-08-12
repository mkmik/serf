#!/usr/bin/env python3
"""Convert an xwd dump to PNG (stdlib only)."""
import struct, sys, zlib
d = open(sys.argv[1], 'rb').read()
h = struct.unpack('>25I', d[:100])
hs, _, _, depth, w, hgt = h[0], h[1], h[2], h[3], h[4], h[5]
bpp, bpl, ncolors = h[11], h[12], h[19]
off = hs + ncolors * 12
rows = []
for y in range(hgt):
    row = bytearray(b'\x00')
    base = off + y * bpl
    for x in range(w):
        i = base + x * (bpp // 8)
        row += bytes((d[i+2], d[i+1], d[i]))   # BGRA -> RGB
    rows.append(bytes(row))
raw = b''.join(rows)
def chunk(t, b):
    c = t + b
    return struct.pack('>I', len(b)) + c + struct.pack('>I', zlib.crc32(c))
png = (b'\x89PNG\r\n\x1a\n'
       + chunk(b'IHDR', struct.pack('>IIBBBBB', w, hgt, 8, 2, 0, 0, 0))
       + chunk(b'IDAT', zlib.compress(raw, 6))
       + chunk(b'IEND', b''))
open(sys.argv[2], 'wb').write(png)
print(f"{sys.argv[2]}  {w}x{hgt}")
