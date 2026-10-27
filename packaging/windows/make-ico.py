"""Packs the app icon PNGs into a Windows .ico (PNG-compressed entries):

    swift packaging/macos/make-icon.swift <dir>.iconset
    python3 packaging/windows/make-ico.py <dir>.iconset packaging/windows/visor.ico
"""
import struct
import sys
from pathlib import Path

iconset, out = Path(sys.argv[1]), Path(sys.argv[2])
# Sizes Windows uses for the taskbar, Explorer and Start menu, all from the
# iconset's renders (the @2x files give 64 and 256).
sources = {16: "icon_16x16.png", 32: "icon_32x32.png", 64: "icon_32x32@2x.png",
           128: "icon_128x128.png", 256: "icon_128x128@2x.png"}
entries = [(size, (iconset / name).read_bytes()) for size, name in sources.items()]
header = struct.pack("<HHH", 0, 1, len(entries))
offset = 6 + 16 * len(entries)
directory, data = b"", b""
for size, png in entries:
    dim = 0 if size >= 256 else size
    directory += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(png), offset + len(data))
    data += png
out.write_bytes(header + directory + data)
print(f"wrote {out} ({len(entries)} sizes)")
