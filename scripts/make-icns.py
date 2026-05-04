#!/usr/bin/env python3
# Build a multi-resolution macOS .icns file from a set of PNGs.
#
# ImageMagick's icns writer is broken (it just writes a single PNG with a
# .icns extension), so we emit the format ourselves. The ICNS format is
# trivial: a fixed 8-byte header followed by chunks, where each chunk is a
# 4-byte type code, 4-byte big-endian length (including the chunk header),
# and the payload (a complete PNG file).
#
# Usage:
#   make-icns.py <out.icns> <png_dir>
#
# Reads <png_dir>/icon-{16,32,64,128,256,512}.png and packs them with the
# corresponding type codes that macOS 10.7+ recognises.

from __future__ import annotations

import struct
import sys
from pathlib import Path

# Type codes per Apple ICNS spec. We use the PNG-payload variants (10.7+).
SIZE_TO_TYPE: dict[int, bytes] = {
    16: b"icp4",
    32: b"icp5",
    64: b"icp6",
    128: b"ic07",
    256: b"ic08",
    512: b"ic09",
}


def build_icns(png_dir: Path, out_path: Path) -> None:
    chunks: list[bytes] = []
    for size, type_code in SIZE_TO_TYPE.items():
        png_path = png_dir / f"icon-{size}.png"
        if not png_path.exists():
            print(f"warning: missing {png_path}, skipping", file=sys.stderr)
            continue
        png_bytes = png_path.read_bytes()
        # Chunk header: 4-byte type + 4-byte big-endian total chunk size
        # (including the 8-byte header itself).
        chunk_size = 8 + len(png_bytes)
        chunks.append(type_code + struct.pack(">I", chunk_size) + png_bytes)

    if not chunks:
        raise SystemExit("error: no input PNGs found, refusing to write empty .icns")

    body = b"".join(chunks)
    # File header: "icns" magic + 4-byte big-endian total file size
    # (including the 8-byte file header).
    file_size = 8 + len(body)
    header = b"icns" + struct.pack(">I", file_size)
    out_path.write_bytes(header + body)


def main() -> None:
    if len(sys.argv) != 3:
        print(f"usage: {sys.argv[0]} <out.icns> <png_dir>", file=sys.stderr)
        raise SystemExit(2)
    out_path = Path(sys.argv[1])
    png_dir = Path(sys.argv[2])
    build_icns(png_dir, out_path)


if __name__ == "__main__":
    main()
