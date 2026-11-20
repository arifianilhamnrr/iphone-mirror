#!/usr/bin/env python3
"""Generate the macOS .icns application icon for iPhone Mirror."""

from pathlib import Path
from tempfile import TemporaryDirectory
import subprocess

from PIL import Image, ImageDraw, ImageFilter


ROOT = Path(__file__).resolve().parents[1]
DESTINATION = ROOT / "resources" / "AppIcon.icns"


def gradient(size: tuple[int, int], top: tuple[int, int, int], bottom: tuple[int, int, int]) -> Image.Image:
    w, h = size
    image = Image.new("RGBA", (w, h))
    pixels = image.load()
    for y in range(h):
        t = y / max(1, h - 1)
        t = t * t * (3 - 2 * t)  # ease: keep highlights vivid at small sizes
        color = tuple(round(top[i] * (1 - t) + bottom[i] * t) for i in range(3))
        for x in range(w):
            pixels[x, y] = (*color, 255)
    return image


def make_icon(size: int = 1024) -> Image.Image:
    s = size / 1024
    p = lambda value: int(value * s)
    box = lambda x0, y0, x1, y1: (p(x0), p(y0), p(x1), p(y1))

    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))

    # Tile: deep indigo -> violet with a soft light bloom.
    tile = gradient((size, size), (45, 32, 96), (13, 16, 48))
    bloom = Image.new("RGBA", canvas.size, (0, 0, 0, 0))
    ImageDraw.Draw(bloom).ellipse(box(120, 40, 790, 700), fill=(124, 92, 255, 150))
    tile.alpha_composite(bloom.filter(ImageFilter.GaussianBlur(p(150))))
    mask = Image.new("L", canvas.size, 0)
    ImageDraw.Draw(mask).rounded_rectangle(box(48, 48, 976, 976), radius=p(228), fill=255)
    canvas.alpha_composite(Image.composite(tile, Image.new("RGBA", canvas.size), mask))

    # Subtle rim light.
    rim = Image.new("RGBA", canvas.size, (0, 0, 0, 0))
    ImageDraw.Draw(rim).rounded_rectangle(
        box(60, 60, 964, 964), radius=p(220),
        outline=(255, 255, 255, 26), width=max(1, p(4)),
    )
    canvas.alpha_composite(rim)

    # Echo screen behind the phone: the "mirrored" copy.
    echo = Image.new("RGBA", canvas.size, (0, 0, 0, 0))
    ImageDraw.Draw(echo).rounded_rectangle(
        box(352, 196, 792, 828), radius=p(86),
        fill=(126, 210, 255, 70), outline=(170, 226, 255, 190),
        width=max(1, p(8)),
    )
    canvas.alpha_composite(echo.filter(ImageFilter.GaussianBlur(p(2))))

    # iPhone body.
    draw = ImageDraw.Draw(canvas)
    draw.rounded_rectangle(box(236, 116, 700, 908), radius=p(112), fill=(7, 10, 22, 255))
    draw.rounded_rectangle(box(252, 132, 684, 892), radius=p(98), fill=(30, 36, 58, 255))

    # Live screen: cyan -> violet with a diagonal sheen.
    screen = gradient((p(400), p(728)), (56, 189, 248), (124, 58, 237))
    sheen = Image.new("RGBA", screen.size, (0, 0, 0, 0))
    ImageDraw.Draw(sheen).polygon(
        [(0, 0), (int(screen.width * 0.62), 0), (0, int(screen.height * 0.72))],
        fill=(255, 255, 255, 46),
    )
    screen.alpha_composite(sheen.filter(ImageFilter.GaussianBlur(p(26))))
    screen_mask = Image.new("L", canvas.size, 0)
    ImageDraw.Draw(screen_mask).rounded_rectangle(box(268, 148, 668, 876), radius=p(84), fill=255)
    canvas.paste(screen, (p(268), p(148)), screen_mask.crop(box(268, 148, 668, 876)))

    # Screen contents: two overlapping screens = mirroring.
    draw = ImageDraw.Draw(canvas)
    draw.rounded_rectangle(
        box(352, 388, 566, 566), radius=p(28),
        fill=(255, 255, 255, 58), outline=(255, 255, 255, 235),
        width=max(1, p(13)),
    )
    draw.rounded_rectangle(box(430, 452, 644, 630), radius=p(28), fill=(255, 255, 255, 240))
    draw.rounded_rectangle(box(452, 474, 622, 608), radius=p(16), fill=(64, 52, 160, 255))

    # Notch and home indicator.
    draw.rounded_rectangle(box(420, 166, 516, 186), radius=p(10), fill=(9, 12, 24, 255))
    draw.rounded_rectangle(box(430, 846, 506, 854), radius=p(4), fill=(255, 255, 255, 210))
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
