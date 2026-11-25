# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
"""Read-only validation of bounded snapshots and closed continuous NTFS logs."""
import csv
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


def verify_io_trace(directory, require_copy=False, source_bytes=0):
    """Validate exported samples without inferring physical USB performance."""
    paths = sorted(directory.glob('samples-*.jsonl'))
    require(1 <= len(paths) <= 4, 'missing or excessive I/O trace segments')
    require(sum(p.stat().st_size for p in paths) <= 8 << 20, 'I/O trace exceeds bounded RAM history')
    samples = []
    for path in paths:
        require(path.is_file() and not path.is_symlink(), 'invalid I/O trace file')
        for line in path.read_bytes().splitlines():
            sample = decode(line)
            require(sample.get('kind') == 'crispawn.io_sample' and sample.get('schema_version') == 1,
                    'invalid I/O trace sample')
            require(type(sample.get('worker_pid')) is int and type(sample.get('observer_pid')) is int
                    and sample['worker_pid'] != sample['observer_pid'], 'I/O observer is not a separate process')
            samples.append(sample)
    require(samples, 'empty I/O trace')
    latest = decode((directory / 'final.json').read_bytes())
    finished = decode((directory / 'finished.json').read_bytes())
    require(finished.get('worker_pid') == latest.get('worker_pid') and finished.get('samples', 0) >= len(samples),
            'I/O observer did not finish normally')
    require(latest.get('observer_pid') != latest.get('worker_pid'), 'final observer identity mismatch')
    operations = {row['operation'] for row in latest.get('operations', []) if row.get('calls', 0)}
    if require_copy:
        require({'source_read', 'blake3', 'backup_data_write', 'descriptor_write', 'log_append'} <= operations,
                'missing measured source/hash/data/descriptor/log operations')
        requested = sum(row.get('requested_bytes', 0) for row in latest.get('operations', [])
                        if row.get('operation') == 'source_read')
        require(requested >= source_bytes, 'copy source reads were not classified as source I/O')
    return {'samples': len(samples), 'operations': sorted(operations), 'independent_process': True,
            'physical_usb': False}


def extract_io_trace(listing, cat, remote, target, **checks):
    """Recover only known diagnostic files from an untrusted guest filesystem."""
    target.mkdir()
    for name in listing(remote):
        require(re.fullmatch(r'samples-[0-9]{6,}\.jsonl|latest\.json|final\.json|finished\.json', name),
                'unexpected I/O diagnostic file')
        with (target / name).open('xb') as stream:
            stream.write(cat(remote + '/' + name))
    return verify_io_trace(target, **checks)


def extract_segmented_parts(archive, cat, remote):
    """Fetch physical parts for the independent checker, retaining legacy support."""
    if not decode((archive / 'plan.json').read_bytes()).get('segmented_storage', False):
        return
    with (archive / 'files.csv').open(newline='') as stream:
        for row in csv.DictReader(stream):
            relative = row['relative_path']
            require(re.fullmatch(r'data/data-[0-9]{6}\.pbk', relative), 'unsafe archive descriptor path')
            descriptor = decode((archive / relative).read_bytes())
            parts = descriptor.get('parts')
            require(isinstance(parts, list) and len(parts) <= 32768, 'invalid physical part list')
            for part in parts:
                name = part['path']
                require(re.fullmatch(r'segments/[0-9a-f]{8}/[0-9a-f]{32}\.bin', name),
                        'unsafe physical part path')
                target = archive / 'data' / name
                target.parent.mkdir(parents=True, exist_ok=True)
                with target.open('xb') as output:
                    output.write(cat(remote + 'data/' + name))
