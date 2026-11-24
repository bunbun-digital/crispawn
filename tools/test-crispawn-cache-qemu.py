#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
"""Observe a fast source and throttled destination using ordinary VM images.

This measures pipeline/cache behavior, not physical USB performance. Independent
NTFS archive readback remains a separate acceptance step.
"""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import random
import tempfile
import time

from crispawn_qemu import run_guest

ROOT = Path(__file__).resolve().parents[1]


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--iso', type=Path, required=True)
    parser.add_argument('--work', type=Path, required=True)
    parser.add_argument('--images-dir', type=Path, required=True)
    parser.add_argument('--source-file', type=Path)
    parser.add_argument('--source-bytes', type=int, default=1 << 30)
    parser.add_argument('--write-bps', type=int, default=8 << 20)
    parser.add_argument('--memory-mib', type=int, default=4096)
    parser.add_argument('--cpus', type=int, default=2)
    parser.add_argument('--skip-verification', action='store_true')
    args = parser.parse_args()
    assert args.iso.is_file() and args.write_bps > 0
    assert 16 << 20 <= args.source_bytes <= 1 << 30
    args.work.mkdir(parents=True, exist_ok=False)
    args.images_dir.mkdir(parents=True, exist_ok=False)
    source = args.source_file or args.images_dir / 'source.img'
    if args.source_file:
        assert source.is_file() and not source.is_symlink()
    else:
        rng = random.Random(20261125)
        with source.open('xb') as stream:
            remaining = args.source_bytes
            while remaining:
                size = min(1 << 20, remaining)
                stream.write(rng.randbytes(size))
                remaining -= size
    source_hash = digest(source)
    spec = importlib.util.spec_from_file_location('usb', ROOT / 'tools/create-crispawn-usb-image.py')
    usb = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(usb)
    config = json.loads((ROOT / 'config/crispawn/examples/live-v2-usb.json').read_text())
    config['startup']['delay_seconds'] = 0
    config['completion']['delay_seconds'] = 3
    # Omission preserves the enabled default and permits old-ISO comparisons.
    config['processing'].pop('verify_after_write', None)
    if args.skip_verification:
        config['processing']['verify_after_write'] = False
    settings = args.work / 'crispawn-config.json'
    settings.write_text(json.dumps(config) + '\n')
    image = args.images_dir / 'usb.img'
    media = usb.create(args.iso, settings, image, args.work / 'media', 4096, ROOT / 'build/signed-profile')
    (args.work / 'media.json').write_text(json.dumps(media, indent=2) + '\n')
    sockets = Path(tempfile.mkdtemp(prefix='cp-cache-qemu-'))
    guest = args.work / 'guest'
    guest.mkdir()
    started = time.monotonic()
    probes = run_guest(sockets, guest, image, source, timeout=360, readonly=True, secure_boot=True,
                       cpus=args.cpus, memory_mib=args.memory_mib, capture_gui=True,
                       destination_write_bps=args.write_bps)
    seconds = time.monotonic() - started
    (args.work / 'guest-runtime.json').write_text(json.dumps({'seconds': seconds, 'normal_return': True}) + '\n')
    assert digest(source) == source_hash
    assert any(p.get('gui_ready') for p in probes)
    assert any(p.get('finalization_complete') and p.get('logs_persisted') for p in probes)
    assert any((p.get('secure_boot') or {}).get('enabled') is True for p in probes)
    assert any(int((p.get('snapshot') or {}).get('read_raw_bytes', 0)) == source.stat().st_size
               and int((p.get('snapshot') or {}).get('verified_raw_bytes', 0)) == (0 if args.skip_verification else source.stat().st_size) for p in probes)
    samples = [p for p in probes if p['phase'] in ('COPYING', 'COMMITTING') and p.get('snapshot')]
    assert samples
    memory = [p['memory'] for p in samples if p.get('memory')]
    result = {
        'kind': 'crispawn.cache-regression', 'real_qemu': True, 'physical_usb': False,
        'iso_sha256': digest(args.iso), 'source_bytes': source.stat().st_size,
        'source_sha256': source_hash, 'source_unchanged': True, 'secure_boot': True,
        'cpus': args.cpus, 'ram_mib': args.memory_mib, 'destination_write_bps': args.write_bps,
        'normal_poweroff': True, 'guest_finalization_and_logs_reported': True,
        'verify_after_write': not args.skip_verification,
        'wall_seconds_including_boot_and_shutdown': seconds,
        'copy_elapsed_seconds': max((p.get('snapshot') or {}).get('elapsed_seconds', 0) for p in probes),
        'max_process_rss_kib': max(p['rss_kib'] for p in samples),
        'min_available_kib': min(p['memory_available_kib'] for p in samples),
        'max_dirty_kib': max((p['dirty_kib'] for p in memory), default=None),
        'max_writeback_kib': max((p['writeback_kib'] for p in memory), default=None),
        'max_queued_bytes_sampled': max((p['encoded_queue_bytes'] for p in memory), default=None),
        'queue_byte_limit': max((p['encoded_queue_limit_bytes'] for p in memory), default=None),
        'independent_ntfs_readback': 'separate step, not inferred from poweroff',
        'source': str(source), 'usb_image': str(image),
        'settings_overrides': {'startup.delay_seconds': 0, 'completion.delay_seconds': 3},
    }
    (args.work / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
