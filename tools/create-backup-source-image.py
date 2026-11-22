#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Create a new synthetic, sparse VHDX source for the owned backup test VM.

Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
No host disks or user files are read. The raw fixture is retained locally.
"""
import argparse
import hashlib
import json
from pathlib import Path
import random
import subprocess


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--raw-file", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--qemu-img", type=Path, required=True)
    p.add_argument("--size-mib", type=int, default=262144)
    a = p.parse_args()
    assert 128 <= a.size_mib <= 1048576
    assert not a.raw_file.exists() and not a.raw_file.is_symlink()
    assert not a.output.exists() and not a.output.is_symlink()
    assert a.raw_file.parent.is_dir() and a.output.parent.is_dir()
    size = a.size_mib * 1024 * 1024
    with a.raw_file.open("xb") as f:
        f.truncate(size)
        f.seek(1024 * 1024)
        f.write(random.Random(983).randbytes(1024 * 1024))
        f.seek(size // 2)
        f.write(b"perfect backup synthetic source fixture\n" * 4096)
    table = f"label: gpt\nunit: sectors\nstart=2048,size={size // 512 - 4096},type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\n"
    subprocess.run(["sfdisk", a.raw_file], input=table.encode(), check=True, capture_output=True)
    tool = a.qemu_img.resolve(strict=True)
    subprocess.run([tool, "convert", "-f", "raw", "-O", "vhdx", "-o", "subformat=dynamic,block_size=2097152", a.raw_file, a.output], check=True)
    subprocess.run([tool, "compare", "-f", "raw", "-F", "vhdx", a.raw_file, a.output], check=True)
    info = json.loads(subprocess.check_output([tool, "info", "-f", "vhdx", "--output=json", a.output]))
    assert info["virtual-size"] == size and info["format"] == "vhdx"
    # Transport hash of the VHDX container, not a hash of the logical source disk.
    with a.output.open("rb") as f:
        transport_hash = hashlib.file_digest(f, "sha256").hexdigest()
    result = {"schema_version": 1, "synthetic": True, "raw_file": str(a.raw_file.resolve()),
              "image": str(a.output), "bytes": size, "stored_bytes": a.output.stat().st_size,
              "image_sha256": transport_hash, "whole_source_hash": None,
              "raw_content_compared": True, "data_model": "zero except GPT headers, 1MiB seeded random at 1MiB, and a repeated marker at half capacity",
              "seed": 983, "qemu_img_info": info}
    metadata = a.output.with_suffix(".vhdx.json")
    with metadata.open("x") as f:
        json.dump(result, f, indent=2);f.write("\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
