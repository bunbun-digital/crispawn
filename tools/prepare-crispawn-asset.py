#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
"""Build-time PNG conversion; no PNG decoder is needed in the guest."""
import argparse
from pathlib import Path
from PIL import Image


def convert(source, output):
    with Image.open(source) as original:
        assert 0 < original.width <= 4096 and 0 < original.height <= 4096
        image = original.convert("RGBA")
        data = b"CRISRGBA" + image.width.to_bytes(4,"little") + image.height.to_bytes(4,"little") + image.tobytes()
    output.parent.mkdir(parents=True, exist_ok=True)
    if output.exists():
        assert output.is_file() and output.read_bytes() == data, "existing asset differs; use a fresh build tree"
    else:
        with output.open("xb") as f:
            f.write(data)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--input", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    a = p.parse_args()
    if a.input.is_dir():
        convert(a.input / "wordmark.png", a.output / "wordmark.rgba")
        for n in range(10):
            convert(a.input / "crystal" / f"CRISPAWN_{n:02}.png", a.output / f"crystal-{n:02}.rgba")
    else:
        convert(a.input, a.output)


if __name__ == "__main__":
    main()
