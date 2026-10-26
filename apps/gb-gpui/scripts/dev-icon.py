#!/usr/bin/env python3
"""Draw a DEV badge on an app icon and write a .icns.

usage: dev-icon.py <base-1024.png> <out.icns>
"""
import os
import subprocess
import sys
import tempfile

from PIL import Image, ImageDraw, ImageFont

FONTS = [
    "/System/Library/Fonts/Supplemental/Arial Rounded Bold.ttf",
    "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
    "/System/Library/Fonts/SFNSRounded.ttf",
]


def font(size):
    for path in FONTS:
        if os.path.exists(path):
            return ImageFont.truetype(path, size)
    return ImageFont.load_default()


def badge(base):
    """A pill across the lower edge, drawn at 4x and scaled down for smooth edges."""
    side = 1024
    scale = 4
    big = side * scale
    layer = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    draw = ImageDraw.Draw(layer)
    # The icon art spans (75,87)-(949,961) on the 1024 canvas.
    width, height = 560 * scale, 176 * scale
    left = (big - width) // 2
    top = 752 * scale
    draw.rounded_rectangle(
        (left, top + 10 * scale, left + width, top + height + 10 * scale),
        radius=height // 2,
        fill=(0, 0, 0, 90),
    )
    draw.rounded_rectangle(
        (left, top, left + width, top + height),
        radius=height // 2,
        fill=(255, 122, 24, 255),
        outline=(255, 255, 255, 255),
        width=10 * scale,
    )
    label = font(120 * scale)
    draw.text(
        (big // 2, top + height // 2 + 4 * scale),
        "DEV",
        font=label,
        fill=(255, 255, 255, 255),
        anchor="mm",
        stroke_width=2 * scale,
        stroke_fill=(255, 255, 255, 255),
    )
    layer = layer.resize((side, side), Image.LANCZOS)
    # Keep the badge inside the icon silhouette.
    mask = base.getchannel("A")
    alpha = layer.getchannel("A")
    clipped = Image.new("L", (side, side), 0)
    clipped.paste(alpha, (0, 0), mask)
    layer.putalpha(clipped)
    return Image.alpha_composite(base, layer)


def main():
    src, out = sys.argv[1], sys.argv[2]
    base = Image.open(src).convert("RGBA").resize((1024, 1024), Image.LANCZOS)
    art = badge(base)
    with tempfile.TemporaryDirectory() as tmp:
        iconset = os.path.join(tmp, "AppIcon.iconset")
        os.makedirs(iconset)
        for size in (16, 32, 128, 256, 512):
            art.resize((size, size), Image.LANCZOS).save(f"{iconset}/icon_{size}x{size}.png")
            art.resize((size * 2, size * 2), Image.LANCZOS).save(f"{iconset}/icon_{size}x{size}@2x.png")
        subprocess.run(["iconutil", "-c", "icns", iconset, "-o", out], check=True)
        if len(sys.argv) > 3:
            art.save(sys.argv[3])


main()
