#!/bin/sh
# Renders the PNGs, .icns and .ico from the two SVGs. Needs resvg
# (cargo install resvg) and, for the .icns, macOS's iconutil.
set -e
cd "$(dirname "$0")"
render() { resvg -w "$2" -h "$2" "$1" "$3"; }
mkdir -p png
for s in 16 24 32; do render icon-small.svg $s png/$s.png; done
for s in 48 64 128 256 512 1024; do render icon.svg $s png/$s.png; done

set_dir=$(mktemp -d)/sql-honcho.iconset
mkdir -p "$set_dir"
for s in 16 32 128 256 512; do
  cp png/$s.png "$set_dir/icon_${s}x${s}.png"
  cp png/$((s * 2)).png "$set_dir/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$set_dir" -o sql-honcho.icns

python3 - png/16.png png/24.png png/32.png png/48.png png/64.png png/256.png <<'PY'
import struct, sys
pngs = [open(p, "rb").read() for p in sys.argv[1:]]
head = struct.pack("<HHH", 0, 1, len(pngs))
offset = 6 + 16 * len(pngs)
entries = b""
for data in pngs:
    w, h = struct.unpack(">II", data[16:24])
    entries += struct.pack("<BBBBHHII", w % 256, h % 256, 0, 0, 1, 32, len(data), offset)
    offset += len(data)
open("sql-honcho.ico", "wb").write(head + entries + b"".join(pngs))
PY
