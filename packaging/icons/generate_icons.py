#!/usr/bin/env python3
# SPDX-License-Identifier: LicenseRef-PolyForm-Noncommercial-1.0.0
"""从 64x64 像素母版生成全平台图标（Pillow，无专有资源）。

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

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parents[2]
NAVY = "#0B1A78"
SCREEN = "#07145C"
MINT = "#5FE6A0"
MINT_SHADE = "#1F8C68"
ORANGE = "#FF6A00"
ORANGE_SHADE = "#C2410C"
CREAM = "#FFF6DC"
GREY = "#74809B"
BLACK = "#0A0A0A"
GRID = 64


def master() -> Image.Image:
    img = Image.new("RGB", (GRID, GRID), "white")
    d = ImageDraw.Draw(img)
    # 阶梯圆角深蓝底
    margins = [11, 8, 6, 5, 4, 3, 2, 2, 1, 1] + [0] * 44
    for y in range(GRID):
        m = margins[min(y, GRID - 1 - y)]
        d.rectangle((m, y, GRID - 1 - m, y), fill=NAVY)
    # 屏幕暗底
    d.rectangle((10, 10, 53, 40), fill=SCREEN)
    d.rectangle((12, 12, 51, 38), fill=SCREEN)
    # 绿色显示器框（阶梯圆角）
    d.rectangle((12, 8, 51, 10), fill=MINT)
    d.rectangle((9, 11, 11, 38), fill=MINT)
    d.rectangle((52, 11, 54, 38), fill=MINT)
    d.rectangle((12, 39, 51, 41), fill=MINT)
    d.rectangle((10, 8, 11, 9), fill=MINT)
    d.rectangle((52, 8, 53, 9), fill=MINT)
    # 支架与底座
    d.rectangle((27, 42, 36, 48), fill=MINT)
    d.rectangle((18, 49, 45, 52), fill=MINT)
    d.rectangle((18, 45, 25, 48), fill=MINT_SHADE)
    d.rectangle((38, 45, 45, 48), fill=MINT_SHADE)
    # 鱼身：逐行像素块，带大眼睛与尾巴
    rows = [
        (18, 28, 39), (19, 25, 43), (20, 23, 46), (21, 21, 48),
        (22, 20, 49), (23, 19, 50), (24, 18, 51), (25, 18, 51),
        (26, 18, 51), (27, 19, 50), (28, 19, 50), (29, 20, 49),
        (30, 21, 48), (31, 22, 47), (32, 23, 46), (33, 25, 44),
        (34, 27, 42), (35, 29, 40),
    ]
    for y, x0, x1 in rows:
        d.rectangle((x0, y, x1, y), fill=ORANGE)
    # 尾巴（上下两片）
    d.rectangle((13, 21, 20, 25), fill=ORANGE)
    d.rectangle((15, 25, 21, 28), fill=ORANGE)
    d.rectangle((13, 28, 20, 32), fill=ORANGE)
    d.rectangle((13, 25, 15, 29), fill=ORANGE_SHADE)
    # 鳍与嘴暗部
    d.rectangle((25, 34, 32, 35), fill=ORANGE_SHADE)
    d.rectangle((48, 31, 51, 33), fill=ORANGE_SHADE)
    # 大眼睛
    d.rectangle((41, 23, 47, 29), fill=CREAM)
    d.rectangle((43, 24, 46, 27), fill=BLACK)
    # 鼠标指针：灰阴影 + 奶油主体，覆盖右下
    pointer = [
        (34, 41, 45), (35, 41, 48), (36, 41, 51), (37, 42, 54),
        (38, 43, 57), (39, 44, 59), (40, 45, 61), (41, 46, 62),
        (42, 47, 62), (43, 46, 57), (44, 45, 53), (45, 44, 49),
        (46, 43, 45), (47, 42, 42),
    ]
    for y, x0, x1 in pointer:
        if y <= 38:
            d.rectangle((x0 - 2, y - 2, x1 - 2, y - 2), fill=GREY)
        d.rectangle((x0, y, x1, y), fill=CREAM)
    return img


def scaled(size: int) -> Image.Image:
    return master().resize((size, size), Image.Resampling.NEAREST)


def write_ico(src: Image.Image, out: Path, sizes=(16, 32, 48, 64, 128, 256)) -> None:
    import io

    entries = []
    for size in sizes:
        image = scaled(size)
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


def write_icns(src: Image.Image, out: Path) -> None:
    import io

    chunk_types = [(b"ic07", 128), (b"ic08", 256), (b"ic09", 512), (b"ic10", 1024)]
    chunks = b""
    for kind, size in chunk_types:
        buf = io.BytesIO()
        scaled(size).save(buf, format="PNG")
        data = buf.getvalue()
        chunks += kind + struct.pack(">I", len(data) + 8) + data
    out.write_bytes(b"icns" + struct.pack(">I", len(chunks) + 8) + chunks)


def main() -> None:
    assets = ROOT / "assets"
    assets.mkdir(exist_ok=True)
    scaled(512).save(assets / "icon-512.png")
    write_ico(scaled(256), ROOT / "packaging/windows/dfy.ico")
    write_icns(scaled(1024), ROOT / "packaging/macos/DFY.icns")
    for size in (16, 32, 48, 64, 128, 256):
        scaled(size).save(ROOT / f"packaging/linux/dfy-{size}.png")
    android = ROOT / "android/app/src/main/res"
    for density, size in [
        ("mdpi", 48), ("hdpi", 72), ("xhdpi", 96), ("xxhdpi", 144), ("xxxhdpi", 192)
    ]:
        target = android / f"mipmap-{density}"
        target.mkdir(parents=True, exist_ok=True)
        scaled(size).save(target / "ic_launcher.png")
        scaled(size).save(target / "ic_launcher_round.png")
    print("icons generated")


if __name__ == "__main__":
    main()
