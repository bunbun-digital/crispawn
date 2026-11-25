#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Real optical boot, USB-NTFS copy/restore and USB regression in owned guests.

Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
Only new ordinary image files; no physical drive, host keys or network changes.
"""
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


def module(name, file):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'tools' / file)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--iso', required=True, type=Path)
    parser.add_argument('--work', required=True, type=Path)
    parser.add_argument('--images-dir', type=Path,
                        help='new directory for ordinary backing files, separate from evidence')
    parser.add_argument('--cases', nargs='+', default=['secure', 'restore', 'uefi', 'bios', 'missing-uuid', 'usb'],
                        choices=['secure', 'restore', 'uefi', 'bios', 'missing-uuid', 'usb'])
    args = parser.parse_args()
    assert 'restore' not in args.cases or 'secure' in args.cases
    output = args.work.resolve()
    output.mkdir(parents=True, exist_ok=False)
    images = args.images_dir or output
    if args.images_dir:
        images.mkdir(parents=True, exist_ok=False)
    assert shutil.disk_usage(output).free > 12 << 30
    state = dict(phase='preparing', iso_sha256=digest(args.iso), results=[],
                 hyperv_tested=False, physical_cd_tested=False, custom_keys_enrolled=False)
    def save():
        (output / 'state.json').write_text(json.dumps(state, indent=2) + '\n')
    save()
    usb = module('usb', 'create-crispawn-usb-image.py')
    settings = module('settings', 'configure-crispawn.py')
    host, env = usb.tools()
    xorriso = str(ROOT / 'build/os/host/bin/xorriso')
    def command(argv, **kw):
        result = subprocess.run(list(map(str, argv)), env=env, capture_output=True, timeout=120, **kw)
        assert result.returncode == 0, result.stderr.decode(errors='replace')
        return result.stdout
    try:
        marker = output / 'crispawn-media.json'
        settings.iso.extract(xorriso, args.iso, '/boot/crispawn-media.json', marker)
        state['media_layout'] = json.loads(marker.read_text())
        assert state['media_layout'] == {
            'kind': 'crispawn.media_layout', 'schema_version': 2,
            'boot_iso_path': '/crispawn.iso', 'data_volume_label': 'CRISPAWN',
            'data_directory': 'crispawn'}
        state['platforms'] = settings.iso.boot_platforms(xorriso, args.iso)
        assert set(state['platforms']) == {'BIOS', 'UEFI'}
        # This USB disk deliberately has no ESP and no boot loader: an optical
        # success cannot be explained by firmware falling back to USB boot.
        data_disk = images / 'data-only-usb.img'
        offset, total = 1 << 20, 2048 << 20
        length = total - 2 * offset
        with data_disk.open('xb') as stream:
            stream.truncate(total)
        command(['sfdisk', data_disk], input=f'label: gpt\nunit: sectors\nstart=2048,size={length//512},type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\n'.encode())
        ntfs = images / 'initial-ntfs.img'
        with ntfs.open('xb') as stream:
            stream.truncate(length)
        command([host / 'usr/sbin/mkntfs', '-Q', '-F', '-L', 'CRISPAWN', '-s', '512', '-c', '4096',
                 '-p', '2048', '-H', '255', '-S', '63', ntfs])
        with ntfs.open('rb') as stream:
            sector = stream.read(512)
        volume = f'{int.from_bytes(sector[72:80], "little"):016X}'
        state['destination_uuid'] = volume
        usb.sparse_copy(ntfs, data_disk, offset=offset)
        source = images / 'source.img'
        with source.open('xb') as stream:
            stream.truncate(128 << 20)
            stream.write(random.Random(221610).randbytes(8 << 20))
            stream.seek((128 << 20) - 512)
            stream.write(b'CRISPAWN OPTICAL SOURCE END'.ljust(512, b'\0'))
        source_hash = digest(source)
        for case in args.cases:
            state['phase'] = case
            save()
            folder = output / case
            folder.mkdir()
            runtime = json.loads((ROOT / 'config/crispawn/examples/live-v2-usb.json').read_text())
            runtime['startup']['delay_seconds'] = 3 if case in ('secure', 'restore') else 0
            runtime['ui']['mode'] = 'gui' if case in ('secure', 'restore') else 'headless'
            runtime['completion']['delay_seconds'] = 2
            runtime['processing']['io_policy']['failure_grace_seconds'] = 1
            if case != 'usb' and case != 'missing-uuid':
                runtime['destination']['volume'] = {'mode': 'uuid', 'uuid': volume}
            if case == 'restore':
                runtime['startup'] = {'mode': 'restore_select'}
                with source.open('r+b') as stream:
                    stream.seek(4096)
                    stream.write(b'INTENTIONALLY DAMAGED BEFORE RESTORE' * 32)
                assert digest(source) != source_hash
            configuration = folder / 'live.json'
            configuration.write_text(json.dumps(runtime) + '\n')
            configuration.chmod(0o600)
            image = folder / 'live.iso'
            settings.configure(args.iso, image, configuration.read_bytes(), xorriso)
            image_hash = digest(image)
            if case == 'usb':
                media = usb.create(image, None, folder / 'boot-usb.img', folder / 'media', 2048,
                                   ROOT / usb._boot.DEFAULT_SECURE_PROFILE_NAME)
                boot_disk = folder / 'boot-usb.img'
            else:
                media = {'ntfs_offset': offset, 'ntfs_bytes': length}
                boot_disk = data_disk
            before_failure = digest(boot_disk) if case == 'missing-uuid' else None
            evidence = folder / 'guest'
            evidence.mkdir()
            sockets = Path(tempfile.mkdtemp(prefix='.crispawn-optical-'))
            failure = 'OPTICAL_DESTINATION_UUID_REQUIRED' if case == 'missing-uuid' else None
            secure = case not in ('uefi', 'bios')
            probes = run_guest(sockets, evidence, boot_disk, source, timeout=360,
                               readonly=case != 'restore', secure_boot=secure, cpus=2, memory_mib=4096,
                               capture_gui=case in ('secure', 'restore', 'bios'),
                               ui_scenario='restore' if case == 'restore' else None,
                               expected_failure_code=failure, optical_iso=None if case == 'usb' else image,
                               legacy_bios=case == 'bios')
            observations = [p['secure_boot'] for p in probes if p.get('secure_boot')]
            assert observations
            if secure:
                assert all(s['enabled'] and not s['setup_mode'] and not s['shim_validation_disabled']
                           and ('[integrity]' in s['lockdown'] or '[confidentiality]' in s['lockdown'])
                           for s in observations)
            elif case == 'uefi':
                assert all(s['enabled'] is False for s in observations)
            else:
                assert all(s['enabled'] is None for s in observations)
            serial = (evidence / 'serial.log').read_text(errors='replace')
            if case != 'usb':
                if case != 'bios':
                    assert 'CRISPAWN_OPTICAL_GRUB_START' in serial
                assert 'pb.optical_label=CRISPAWN_' in serial
                assert any(p.get('excluded_disk', '').startswith('sr') for p in probes)
            if failure:
                assert digest(boot_disk) == before_failure
                assert not any(p['phase'] in ('COPYING', 'RESTORING') for p in probes)
            else:
                recovered = folder / 'ntfs.img'
                with recovered.open('xb') as stream:
                    stream.truncate(media['ntfs_bytes'])
                usb.sparse_copy(boot_disk, recovered, source_offset=media['ntfs_offset'], length=media['ntfs_bytes'])
                def cat(path):
                    return command([host / 'usr/bin/ntfscat', recovered, path])
                def listing(path):
                    return [v for v in command([host / 'usr/bin/ntfsls', '-p', path, recovered]).decode().splitlines()
                            if v not in ('.', '..')]
                logs = folder / 'logs'
                logs.mkdir()
                records = []
                for directory in listing('/crispawn/logs'):
                    saved = logs / directory
                    saved.mkdir()
                    for name in listing('/crispawn/logs/' + directory):
                        if name == 'io-observer':
                            extract_io_trace(listing, cat, '/crispawn/logs/' + directory + '/' + name, saved / name)
                            continue
                        assert re.fullmatch(r'(events-[0-9]+\.jsonl|log-info\.json|probe-final\.json)', name)
                        data = cat('/crispawn/logs/' + directory + '/' + name)
                        (saved / name).write_bytes(data)
                        if name.startswith('events-'):
                            records.extend(json.loads(line) for line in data.splitlines())
                    verify_log_directory(saved)
                if case != 'usb':
                    assert any(r.get('kind') == 'optical_boot' for r in records)
                    assert not any(r.get('kind') == 'usb_boot_iso_verified' for r in records)
                else:
                    assert any(r.get('kind') == 'usb_boot_iso_verified'
                               and r['value']['boot_iso_path'] == '/crispawn.iso' for r in records)
                if case == 'restore':
                    assert digest(source) == source_hash
                    assert any(p['phase'] == 'RESTORING' for p in probes)
                else:
                    groups = listing('/crispawn/sessions')
                    assert groups
                    for index, group in enumerate(groups):
                        session = json.loads(cat('/crispawn/sessions/' + group))
                        assert len(session['disks']) == 1
                        archive_path = session['disks'][0]['archive']
                        archive = folder / ('archive-' + str(index))
                        archive.mkdir()
                        (archive / 'data').mkdir()
                        for name in ['metadata.json', 'plan.json', 'files.csv', 'checksums.sha256', 'COMMITTED', 'journal.jsonl', '.lock']:
                            (archive / name).write_bytes(cat('/' + archive_path + '/' + name))
                        with (archive / 'files.csv').open(newline='') as stream:
                            for row in csv.DictReader(stream):
                                name = row['relative_path']
                                assert re.fullmatch(r'data/data-[0-9]{6}\.pbk', name)
                                (archive / name).write_bytes(cat('/' + archive_path + '/' + name))
                        extract_segmented_parts(archive, cat, '/' + archive_path + '/')
                        verify_env = dict(os.environ, PYTHONPATH=str(ROOT / 'build/verifier-libs'))
                        result = subprocess.run(['python3', ROOT / 'tools/check-crispawn-format.py', archive, '--source-file', source],
                                                env=verify_env, capture_output=True, timeout=120)
                        (folder / ('verify-' + str(index) + '.log')).write_bytes(result.stdout + result.stderr)
                        assert result.returncode == 0
            assert digest(image) == image_hash
            assert digest(source) == source_hash
            state['results'].append({'case': case, 'passed': True, 'secure_boot': secure,
                                     'normal_poweroff': True, 'expected_failure': failure,
                                     'read_only_iso_unchanged': True, 'ntfs_logs_verified': failure is None,
                                     'backup_or_restore_verified': failure is None})
            save()
            print('CHECK', case, flush=True)
        assert digest(args.iso) == state['iso_sha256']
        state['phase'] = 'complete'
        save()
    except BaseException as error:
        state['phase'] = 'failed'
        state['error_type'] = type(error).__name__
        save()
        raise


if __name__ == '__main__':
    main()
