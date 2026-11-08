#!/usr/bin/env python3
"""Generate the macOS .icns application icon."""

from pathlib import Path
from tempfile import TemporaryDirectory
import subprocess

from PIL import Image, ImageDraw, ImageFilter


ROOT = Path(__file__).resolve().parents[1]
DESTINATION = ROOT / "resources" / "AppIcon.icns"


def make_icon(size: int = 1024) -> Image.Image:
    scale = size / 1024
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    background = Image.new("RGBA", canvas.size)
    pixels = background.load()
    for y in range(size):
        for x in range(size):
            t = (x * 0.38 + y * 0.62) / size
            pixels[x, y] = (
                int(31 + 19 * t),
                int(35 + 20 * t),
                int(54 + 48 * t),
                255,
            )

    mask = Image.new("L", canvas.size, 0)
    ImageDraw.Draw(mask).rounded_rectangle(
        (56 * scale, 56 * scale, 968 * scale, 968 * scale),
        radius=224 * scale,
        fill=255,
    )
    canvas.alpha_composite(Image.composite(background, Image.new("RGBA", canvas.size), mask))

    glow = Image.new("RGBA", canvas.size, (0, 0, 0, 0))
    ImageDraw.Draw(glow).ellipse(
        (185 * scale, 150 * scale, 839 * scale, 804 * scale),
        fill=(74, 208, 255, 105),
    )
    canvas.alpha_composite(glow.filter(ImageFilter.GaussianBlur(95 * scale)))

    draw = ImageDraw.Draw(canvas)
    draw.rounded_rectangle(
        (276 * scale, 133 * scale, 748 * scale, 891 * scale),
        radius=92 * scale,
        fill=(8, 10, 17, 255),
    )
    draw.rounded_rectangle(
        (301 * scale, 159 * scale, 723 * scale, 865 * scale),
        radius=72 * scale,
        fill=(226, 239, 248, 255),
    )
    for inset, alpha, width in ((0, 245, 24), (57, 170, 20), (112, 95, 16)):
        draw.arc(
            (
                (401 + inset) * scale,
                (311 + inset) * scale,
                (744 - inset) * scale,
                (654 - inset) * scale,
            ),
            start=-62,
            end=62,
            fill=(28, 112, 169, alpha),
            width=max(1, int(width * scale)),
        )
    draw.ellipse(
        (481 * scale, 487 * scale, 543 * scale, 549 * scale),
        fill=(25, 105, 164, 255),
    )
    draw.rounded_rectangle(
        (451 * scale, 173 * scale, 573 * scale, 193 * scale),
        radius=10 * scale,
        fill=(27, 30, 39, 255),
    )
    return canvas


def main() -> None:
    DESTINATION.parent.mkdir(parents=True, exist_ok=True)
    source = make_icon()
    with TemporaryDirectory(prefix="iphone-mirror-icon-") as directory:
        iconset = Path(directory) / "AppIcon.iconset"
        iconset.mkdir()
        for points in (16, 32, 128, 256, 512):
            for multiplier in (1, 2):
                pixels = points * multiplier
                suffix = f"@{multiplier}x" if multiplier == 2 else ""
                source.resize((pixels, pixels), Image.Resampling.LANCZOS).save(
                    iconset / f"icon_{points}x{points}{suffix}.png"
                )
        subprocess.run(
            ["iconutil", "-c", "icns", str(iconset), "-o", str(DESTINATION)],
            check=True,
        )
    print(f"Generated {DESTINATION}")


if __name__ == "__main__":
    main()
