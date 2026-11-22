# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
"""Read-only validation of bounded snapshots and closed continuous NTFS logs."""
import json
import re


def require(value, message):
    if not value:
        raise ValueError(message)


def unique(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate log JSON field")
        result[key] = value
    return result


def invalid_constant(value):
    raise ValueError("nonfinite log JSON value")


def decode(data):
    return json.loads(data, object_pairs_hook=unique, parse_constant=invalid_constant)


def verify_log_directory(directory):
    info_path = directory / "log-info.json"
    require(info_path.is_file() and not info_path.is_symlink() and info_path.stat().st_size <= 65536,
            "invalid log metadata file")
    info = decode(info_path.read_bytes())
    require(info.get("schema_version") == 1, "unsupported log metadata")
    cap = info.get("cap_bytes")
    require(isinstance(cap, str) and cap.isascii() and cap.isdigit() and 65536 <= int(cap) <= 32 * 1024**2,
            "invalid RAM log capacity")
    cap = int(cap)
    paths = sorted(directory.glob("events-*.jsonl"))
    require(1 <= len(paths) <= 10000, "missing or excessive log segments")
    for path in paths:
        require(path.is_file() and not path.is_symlink() and re.fullmatch(r"events-[0-9]{6,}\.jsonl", path.name),
                "invalid log segment")
        require(path.stat().st_size <= cap // 4, "log segment exceeds rotation size")
    total = sum(p.stat().st_size for p in paths)
    if info.get("mode") == "continuous":
        require(info.get("closed") is True and info.get("ntfs_history_bounded") is False,
                "continuous log was not closed before unmount")
        require(info.get("ram_cap_bytes") == str(cap), "continuous RAM capacity mismatch")
        require(type(info.get("dropped_records")) is int and info["dropped_records"] == 0,
                "continuous log records were dropped or the count is missing")
        interval = info.get("sync_interval_ms")
        require(type(interval) is int and 0 <= interval <= 60000, "invalid log synchronization interval")
        numbers = [int(p.stem.split("-")[1]) for p in paths]
        require(numbers == list(range(numbers[0], numbers[0] + len(numbers))), "missing continuous log segment")
    else:
        require(info.get("mode") is None and len(paths) <= 4 and total <= cap,
                "snapshot logs exceed RAM capacity")
    records = 0
    for path in paths:
        with path.open("rb") as stream:
            while line := stream.readline(32769):
                require(len(line) <= 32768 and line.endswith(b"\n"), "oversized or incomplete log record")
                require(isinstance(decode(line), dict), "invalid log record")
                records += 1
    return {"mode": info.get("mode", "snapshot"), "bytes": total, "records": records}
