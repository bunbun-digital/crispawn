#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Real Secure Boot copy with and without a UART; verify results from NTFS."""
import argparse
import csv
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import random
import re
import shutil
import subprocess
import tempfile

from crispawn_qemu import run_guest
from crispawn_log_evidence import verify_log_directory, extract_io_trace, extract_segmented_parts

ROOT = Path(__file__).resolve().parents[1]


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--iso', type=Path, required=True)
    parser.add_argument('--work', type=Path, required=True)
    args = parser.parse_args()
    out = args.work.resolve()
    out.mkdir(parents=True, exist_ok=False)
    assert shutil.disk_usage(out).free >= 12 << 30
    spec = importlib.util.spec_from_file_location('usb', ROOT/'tools/create-crispawn-usb-image.py')
    usb = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(usb)
    iso = args.iso.resolve(strict=True)
    iso_hash = digest(iso)
    source = out/'source.img'
    with source.open('xb') as stream:
        stream.truncate(1 << 30)
        stream.write(random.Random(20261123).randbytes(64 << 20))
        stream.seek((1 << 30)-512)
        stream.write(b'CRISPAWN whole disk console regression'.ljust(512, b'\0'))
    source_hash = digest(source)
    config = json.loads((ROOT/'config/crispawn/examples/live-v2-usb.json').read_text())
    config['startup']['delay_seconds'] = 0
    config['completion']['delay_seconds'] = 8
    settings = out/'crispawn-config.json'
    settings.write_text(json.dumps(config, indent=2)+'\n')
    report = dict(phase='running', iso=str(iso), iso_sha256=iso_hash,
                  source_bytes=1 << 30, source_sha256=source_hash,
                  physical_devices=False, real_qemu=True, cases=[])

    def save():
        (out/'report.json').write_text(json.dumps(report, indent=2)+'\n')

    def command(folder, argv):
        result = subprocess.run(list(map(str, argv)), capture_output=True, timeout=120)
        with (folder/'commands.log').open('ab') as log:
            log.write(json.dumps(list(map(str, argv))).encode()+b'\n'+result.stderr)
        assert result.returncode == 0, (argv[0], result.stderr.decode(errors='replace'))
        return result.stdout

    save()
    try:
        for has_uart in (False, True):
            name = 'with-serial' if has_uart else 'no-serial'
            folder = out/name
            folder.mkdir()
            image = folder/'usb.img'
            media = usb.create(iso, settings, image, folder/'media', 2048,
                               ROOT/usb._boot.DEFAULT_SECURE_PROFILE_NAME)
            evidence = folder/'guest'
            evidence.mkdir()
            print('CASE', name, flush=True)
            probes = run_guest(Path(tempfile.mkdtemp(prefix='cp-console-')), evidence,
                               image, source, timeout=240, readonly=True, secure_boot=True,
                               cpus=8, memory_mib=8192, serial_port=has_uart,
                               record_frames=True)
            # UART-less success is proved below, not inferred from poweroff.
            ntfs = folder/'result-ntfs.img'
            with ntfs.open('xb') as stream:
                stream.truncate(media['ntfs_bytes'])
            usb.sparse_copy(image, ntfs, source_offset=media['ntfs_offset'], length=media['ntfs_bytes'])

            def cat(path):
                return command(folder, ['ntfscat', ntfs, path])

            def listing(path):
                return [name for name in command(folder, ['ntfsls', '-p', path, ntfs]).decode().splitlines()
                        if name not in ('.', '..')]

            assert command(folder, ['ntfslabel', ntfs]).decode().strip() == 'CRISPAWN'
            root = listing('/')
            assert 'crispawn' in root and 'perfect-backup' not in root
            assert set(listing('/crispawn')) == {'backups', 'logs', 'sessions', 'reports', 'work'}
            names = listing('/crispawn/sessions')
            assert len(names) == 1 and re.fullmatch(r'[0-9a-f]{32}\.json', names[0])
            session = json.loads(cat('/crispawn/sessions/'+names[0]))
            assert len(session['disks']) == 1
            disk = session['disks'][0]
            assert disk['archive'].startswith('crispawn/backups/')
            assert int(disk['bytes']) == source.stat().st_size
            archive = folder/'archive'
            archive.mkdir()
            (archive/'data').mkdir()
            prefix = '/'+disk['archive']+'/'
            for filename in ['metadata.json', 'plan.json', 'files.csv', 'checksums.sha256', 'COMMITTED', 'journal.jsonl', '.lock']:
                (archive/filename).write_bytes(cat(prefix+filename))
            with (archive/'files.csv').open(newline='') as stream:
                for row in csv.DictReader(stream):
                    filename = row['relative_path']
                    assert re.fullmatch(r'data/data-[0-9]{6}\.pbk', filename)
                    (archive/filename).write_bytes(cat(prefix+filename))
            extract_segmented_parts(archive, cat, prefix)
            result = subprocess.run(['python3', ROOT/'tools/check-crispawn-format.py', archive, '--source-file', source],
                                    env=dict(os.environ, PYTHONPATH=str(ROOT/'build/verifier-libs')),
                                    capture_output=True, timeout=120)
            (folder/'independent-verification.log').write_bytes(result.stdout+result.stderr)
            assert result.returncode == 0
            reports = listing('/crispawn/reports')
            assert len(reports) == 1
            job = json.loads(cat('/crispawn/reports/'+reports[0]))
            assert job['status'] == 'complete' and job['engine']['dataset_committed']
            (folder/'job-report.json').write_text(json.dumps(job, indent=2)+'\n')
            records, finals = [], []
            for group in listing('/crispawn/logs'):
                assert re.fullmatch(r'[0-9a-f]{32}-(complete|live)-[0-9a-f]{8}', group)
                target = folder/'logs'/group
                target.mkdir(parents=True)
                for filename in listing('/crispawn/logs/'+group):
                    if filename == 'io-observer':
                        extract_io_trace(listing, cat, '/crispawn/logs/'+group+'/'+filename, target/filename,
                                         require_copy=True, source_bytes=source.stat().st_size)
                        continue
                    assert re.fullmatch(r'events-[0-9]+\.jsonl|log-info\.json|probe-final\.json', filename)
                    data = cat('/crispawn/logs/'+group+'/'+filename)
                    (target/filename).write_bytes(data)
                    if filename.startswith('events-'):
                        records.extend(json.loads(line) for line in data.splitlines())
                    elif filename == 'probe-final.json':
                        finals.append(json.loads(data))
                verify_log_directory(target)
            secure = [r['value'] for r in records if r.get('kind') == 'secure_boot']
            assert secure and all(s['enabled'] and not s['setup_mode'] and not s['shim_validation_disabled'] for s in secure)
            assert any(r.get('kind') == 'usb_boot_iso_verified' and r['value']['boot_iso_path'] == '/crispawn.iso' for r in records)
            snapshots = [f['snapshot'] for f in finals if f.get('snapshot')]
            assert snapshots
            assert any(int(s['read_raw_bytes']) == 1 << 30 and int(s['verified_raw_bytes']) == 1 << 30 for s in snapshots)
            assert digest(source) == source_hash and digest(iso) == iso_hash
            report['cases'].append(dict(name=name, serial_port=has_uart,
                committed=True, read_raw_bytes=1 << 30, verified_raw_bytes=1 << 30,
                independent_verification=True, ntfs_logs_verified=True,
                secure_boot_verified=True, guest_requested_normal_poweroff=True,
                screenshot_frames=str(evidence/'frames.json'), live_probe_count=len(probes)))
            save()
            print('CASE_COMPLETE', name, flush=True)
        report['phase'] = 'complete'
    except BaseException as error:
        report.update(phase='failed', error=repr(error))
        raise
    finally:
        save()
        print('REPORT', out/'report.json', flush=True)


if __name__ == '__main__':
    main()
