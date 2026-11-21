#!/usr/bin/env python3
"""Generate the macOS .icns application icon from the bundled source PNG."""

from pathlib import Path
from tempfile import TemporaryDirectory
import subprocess

from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "resources" / "AppIcon.png"
DESTINATION = ROOT / "resources" / "AppIcon.icns"


def main() -> None:
    if not SOURCE.exists():
        raise SystemExit(f"missing icon source: {SOURCE}")
    source = Image.open(SOURCE).convert("RGBA")
    if source.size != (1024, 1024):
        source = source.resize((1024, 1024), Image.Resampling.LANCZOS)

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
