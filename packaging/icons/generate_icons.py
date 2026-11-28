#!/usr/bin/env python3
# SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
"""从用户提供的原始图标生成全平台图标（Pillow）。

用法：python packaging/icons/generate_icons.py
输出：
  assets/icon-512.png
  packaging/windows/dfy.ico              多尺寸 ICO
  packaging/macos/DFY.icns               ICNS
  packaging/linux/dfy-256.png 等         Linux desktop 图标
  android/app/src/main/res/mipmap-*/ic_launcher.png
"""

from pathlib import Path
import struct

from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "assets" / "icon-source.png"


def master() -> Image.Image:
    with Image.open(SOURCE) as source:
        return source.convert("RGBA")


def scaled(source: Image.Image, size: int) -> Image.Image:
    return source.resize((size, size), Image.Resampling.LANCZOS)


def write_ico(source: Image.Image, out: Path, sizes=(16, 32, 48, 64, 128, 256)) -> None:
    import io

    entries = []
    for size in sizes:
        image = scaled(source, size)
        buf = io.BytesIO()
        image.save(buf, format="PNG")
        entries.append((size, buf.getvalue()))
    header = struct.pack("<HHH", 0, 1, len(entries))
    offset = 6 + 16 * len(entries)
    directory = b""
    payload = b""
    for size, data in entries:
        directory += struct.pack(
            "<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(data), offset
        )
        payload += data
        offset += len(data)
    out.write_bytes(header + directory + payload)


def write_icns(source: Image.Image, out: Path) -> None:
    import io

    chunk_types = [(b"ic07", 128), (b"ic08", 256), (b"ic09", 512), (b"ic10", 1024)]
    chunks = b""
    for kind, size in chunk_types:
        buf = io.BytesIO()
        scaled(source, size).save(buf, format="PNG")
        data = buf.getvalue()
        chunks += kind + struct.pack(">I", len(data) + 8) + data
    out.write_bytes(b"icns" + struct.pack(">I", len(chunks) + 8) + chunks)


def main() -> None:
    source = master()
    assets = ROOT / "assets"
    scaled(source, 512).save(assets / "icon-512.png")
    write_ico(source, ROOT / "packaging/windows/dfy.ico")
    write_icns(source, ROOT / "packaging/macos/DFY.icns")
    for size in (16, 32, 48, 64, 128, 256):
        scaled(source, size).save(ROOT / f"packaging/linux/dfy-{size}.png")
    android = ROOT / "android/app/src/main/res"
    for density, size in [
        ("mdpi", 48), ("hdpi", 72), ("xhdpi", 96), ("xxhdpi", 144), ("xxxhdpi", 192)
    ]:
        target = android / f"mipmap-{density}"
        target.mkdir(parents=True, exist_ok=True)
        scaled(source, size).save(target / "ic_launcher.png")
        scaled(source, size).save(target / "ic_launcher_round.png")
    print("icons generated")


if __name__ == "__main__":
    main()
