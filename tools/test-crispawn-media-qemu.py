#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Verify the CRISPAWN USB layout and moved-session restore in owned guests.

Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
Only new ordinary synthetic images; no host mount, physical USB or Windows change.
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
from crispawn_log_evidence import verify_log_directory

ROOT = Path(__file__).resolve().parents[1]
LAYOUT = {"kind": "crispawn.media_layout", "schema_version": 2,
          "boot_iso_path": "/crispawn.iso",
          "data_volume_label": "CRISPAWN", "data_directory": "crispawn"}


def digest(file):
    with file.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'tools' / filename)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--iso', required=True, type=Path)
    parser.add_argument('--work', required=True, type=Path)
    args = parser.parse_args()
    output = args.work.resolve()
    output.mkdir(parents=True, exist_ok=False)
    assert args.iso.is_file() and not args.iso.is_symlink()
    assert shutil.disk_usage(output).free > 12 << 30
    usb = module('usb', 'create-crispawn-usb-image.py')
    settings = module('settings', 'configure-crispawn.py')
    host, env = usb.tools()
    sdk = ROOT / 'build/os/host/bin'
    profile_path = ROOT / usb._boot.DEFAULT_SECURE_PROFILE_NAME
    profile = json.loads((profile_path / 'profile.json').read_text())
    state = dict(phase='preparing', iso=str(args.iso.resolve()), iso_bytes=args.iso.stat().st_size,
                 iso_sha256=digest(args.iso), simulated=False, qemu_real_guest=True,
                 physical_devices=False, windows_builder_changed=False, hyperv_tested=False,
                 cpus=8, memory_mib=8192, checks=[])

    def save():
        (output / 'state.json').write_text(json.dumps(state, indent=2) + '\n')

    def check(name):
        state['checks'].append(name)
        save()
        print('CHECK', name, flush=True)

    def command(argv, timeout=120):
        result = subprocess.run(list(map(str, argv)), env=env, capture_output=True, timeout=timeout)
        with (output / 'commands.log').open('ab') as log:
            log.write(json.dumps(list(map(str, argv))).encode() + b'\n' + result.stdout + result.stderr)
        assert result.returncode == 0, (argv[0], result.stderr.decode(errors='replace'))
        return result.stdout

    def partition(image, media, kind, folder):
        file = folder / (kind + '.img')
        with file.open('xb') as stream:
            stream.truncate(media[kind + '_bytes'])
        usb.sparse_copy(image, file, source_offset=media[kind + '_offset'], length=media[kind + '_bytes'])
        return file

    def ntfs(image, media, folder):
        file = partition(image, media, 'ntfs', folder)
        label = command([host / 'usr/sbin/ntfslabel', file]).decode().strip()
        assert label == 'CRISPAWN', label

        def cat(path):
            return command([host / 'usr/bin/ntfscat', file, path])

        def listing(path):
            data = command([host / 'usr/bin/ntfsls', '-p', path, file])
            return [v for v in data.decode().splitlines() if v not in ('.', '..')]

        root = listing('/')
        assert 'crispawn' in root and 'perfect-backup' not in root
        children = listing('/crispawn')
        assert set(children) == {'backups', 'logs', 'sessions', 'reports', 'work'}, children
        assert listing('/crispawn/work') == []
        layout = dict(label=label, root=root, crispawn=children, work_empty=True)
        (folder / 'ntfs-layout.json').write_text(json.dumps(layout, indent=2) + '\n')
        return file, cat, listing, layout

    def guest(image, source, folder, restore=False, secure=True, expected_failure=None):
        folder.mkdir()
        sockets = Path(tempfile.mkdtemp(prefix='.crispawn-media-qemu-'))
        probes = run_guest(sockets, folder, image, source, timeout=600, readonly=not restore,
                           secure_boot=secure, cpus=8, memory_mib=8192, capture_gui=True,
                           expected_failure_code=expected_failure,
                           source_read_bps=32 << 20,
                           ui_scenario='restore' if restore else None,
                           source_serial='PB-RESTORE-0001' if restore else 'PB-SOURCE-0001')
        observations = [p['secure_boot'] for p in probes if p.get('secure_boot')]
        assert observations
        if secure:
            assert all(s['enabled'] and not s['setup_mode'] and not s['shim_validation_disabled']
                       and ('[integrity]' in s['lockdown'] or '[confidentiality]' in s['lockdown']) for s in observations)
        else:
            # The ordinary USB firmware has no Secure Boot variable store.
            assert all(s['enabled'] in (False, None) and '[none]' in s['lockdown']
                       for s in observations)
        assert not any('IO_ERROR' in p.get('detail', '') for p in probes)
        if expected_failure:
            assert not any(p['phase'] in ('COPYING', 'RESTORING') for p in probes)
        else:
            assert any(p.get('finalization_complete') for p in probes)
            assert any(p['phase'] == ('RESTORING' if restore else 'COPYING') for p in probes)
        return probes

    save()
    try:
        assert 0 < state['iso_bytes'] < 100_000_000
        marker = output / 'crispawn-media.json'
        settings.iso.extract(str(sdk / 'xorriso'), args.iso, '/boot/crispawn-media.json', marker)
        assert json.loads(marker.read_text()) == LAYOUT
        state['media_layout'] = dict(value=LAYOUT, sha256=digest(marker), extracted=True)
        kernel = output / 'kernel'
        settings.iso.extract(str(sdk / 'xorriso'), args.iso, '/boot/bzImage', kernel)
        assert digest(kernel) == profile['files']['kernel']
        fat = output / 'optical-fat.img'
        settings.iso.extract(str(sdk / 'xorriso'), args.iso, '/boot/fat.efi', fat)
        state['signed_boot_files'] = {'kernel': digest(kernel)}
        for name, path in [('shimx64.efi', 'BOOTX64.EFI'), ('grubx64.efi', 'grubx64.efi'), ('mmx64.efi', 'mmx64.efi')]:
            extracted = output / name
            command([sdk / 'mcopy', '-i', fat, '::/EFI/BOOT/' + path, extracted])
            assert digest(extracted) == profile['files'][name]
            state['signed_boot_files'][name] = digest(extracted)
        check('iso-under-100MB-five-field-v2-media-layout-and-pinned-signed-kernel-efi-readback')

        source = output / 'source.img'
        with source.open('xb') as stream:
            stream.truncate(256 << 20)
            stream.write(random.Random(231129).randbytes(16 << 20))
            stream.seek(128 << 20)
            stream.write(b'CRISPAWN USB media test\n' * 50000)
            stream.seek((256 << 20) - 512)
            stream.write(b'END OF COMPLETE SOURCE DISK'.ljust(512, b'\0'))
        source_hash = digest(source)
        image = output / 'backup-usb.img'
        # No external override: this boots and copies with the new ISO defaults.
        media = usb.create(args.iso, None, image, output / 'media', 2048, profile_path)
        assert media['boot_iso_path'] == '/crispawn.iso' and media['media_layout'] == LAYOUT
        assert media['boot_iso_readback_sha256'] == state['iso_sha256']
        state['media'] = media
        state['phase'] = 'copying'
        save()
        probes = guest(image, source, output / 'copy-guest')
        assert any(p.get('gui_ready') for p in probes)
        copied = output / 'copy-evidence'
        copied.mkdir()
        ntfs_file, cat, listing, layout = ntfs(image, media, copied)
        groups = listing('/crispawn/sessions')
        assert len(groups) == 1 and re.fullmatch(r'[0-9a-f]{32}\.json', groups[0])
        session_path = '/crispawn/sessions/' + groups[0]
        saved = json.loads(cat(session_path))
        assert len(saved['disks']) == 1
        disk = saved['disks'][0]
        assert disk['archive'].startswith('crispawn/backups/') and int(disk['bytes']) == source.stat().st_size
        (copied / 'saved-session.json').write_text(json.dumps(saved, indent=2) + '\n')
        archive = copied / 'archive'
        archive.mkdir()
        (archive / 'data').mkdir()
        prefix = '/' + disk['archive'] + '/'
        for name in ['metadata.json', 'plan.json', 'files.csv', 'checksums.sha256', 'COMMITTED', 'journal.jsonl', '.lock']:
            (archive / name).write_bytes(cat(prefix + name))
        with (archive / 'files.csv').open(newline='') as stream:
            for row in csv.DictReader(stream):
                name = row['relative_path']
                assert re.fullmatch(r'data/data-[0-9]{6}\.pbk', name)
                (archive / name).write_bytes(cat(prefix + name))
        verify_env = dict(os.environ, PYTHONPATH=str(ROOT / 'build/verifier-libs'))
        result = subprocess.run(['python3', ROOT / 'tools/check-crispawn-format.py', archive, '--source-file', source],
                                env=verify_env, capture_output=True, timeout=120)
        (copied / 'independent-verification.log').write_bytes(result.stdout + result.stderr)
        assert result.returncode == 0, result.stderr.decode(errors='replace')
        reports = listing('/crispawn/reports')
        assert len(reports) == 1
        report = json.loads(cat('/crispawn/reports/' + reports[0]))
        assert report['status'] == 'complete' and report['engine']['dataset_committed']
        (copied / 'job-report.json').write_text(json.dumps(report, indent=2) + '\n')
        logs = listing('/crispawn/logs')
        assert logs
        copy_records = []
        for directory in logs:
            assert re.fullmatch(r'[0-9a-f]{32}-(?:complete|live)-[0-9a-f]{8}', directory)
            folder = copied / 'logs' / directory
            folder.mkdir(parents=True)
            for name in listing('/crispawn/logs/' + directory):
                assert re.fullmatch(r'(events-[0-9]+\.jsonl|log-info\.json|probe-final\.json)', name)
                data = cat('/crispawn/logs/' + directory + '/' + name)
                (folder / name).write_bytes(data)
                if name.startswith('events-'):
                    copy_records.extend(json.loads(line) for line in data.splitlines())
            verify_log_directory(folder)
        assert any(r.get('kind') == 'usb_boot_iso_verified'
                   and r['value']['boot_iso_path'] == '/crispawn.iso'
                   and r['value']['path'].endswith('/crispawn.iso') for r in copy_records)
        assert not any(r.get('kind') == 'optical_boot' for r in copy_records)
        assert digest(source) == source_hash
        state['copy_layout'] = layout
        check('embedded-defaults-usb-copy-inline-verification-continuous-logs-sessions-reports-and-clean-work')

        # Also exercise the ordinary unsigned virtual-media path, with its ISO
        # on NTFS. The published ISO's vendor-signed files remain unchanged.
        unsigned = output / 'unsigned-copy'
        unsigned.mkdir()
        runtime = json.loads((ROOT / 'config/crispawn/examples/live-v2-usb.json').read_text())
        runtime['startup']['delay_seconds'] = 0
        runtime['completion']['delay_seconds'] = 1
        override = unsigned / 'settings.json'
        override.write_text(json.dumps(runtime) + '\n')
        unsigned_image = unsigned / 'usb.img'
        unsigned_media = usb.create(args.iso, override, unsigned_image, unsigned / 'media', 2048, None)
        assert unsigned_media['boot_iso_path'] == '/crispawn.iso'
        assert unsigned_media['media_layout'] == LAYOUT
        assert unsigned_media['boot_iso_readback_sha256'] == state['iso_sha256']
        unsigned_probes = guest(unsigned_image, source, unsigned / 'guest', secure=False)
        assert any(p.get('snapshot') and int(p['snapshot']['verified_raw_bytes']) == source.stat().st_size
                   for p in unsigned_probes)
        _, unsigned_cat, unsigned_listing, unsigned_layout = ntfs(unsigned_image, unsigned_media, unsigned)
        assert 'crispawn.iso' in unsigned_listing('/')
        unsigned_session = json.loads(unsigned_cat('/crispawn/sessions/' + unsigned_listing('/crispawn/sessions')[0]))
        unsigned_prefix = '/' + unsigned_session['disks'][0]['archive'] + '/'
        assert unsigned_prefix.startswith('/crispawn/backups/')
        unsigned_archive = unsigned / 'archive'
        unsigned_archive.mkdir(); (unsigned_archive / 'data').mkdir()
        for name in ['metadata.json', 'plan.json', 'files.csv', 'checksums.sha256', 'COMMITTED', 'journal.jsonl', '.lock']:
            (unsigned_archive / name).write_bytes(unsigned_cat(unsigned_prefix + name))
        for row in csv.DictReader((unsigned_archive / 'files.csv').open(newline='')):
            name = row['relative_path']; assert re.fullmatch(r'data/data-[0-9]{6}\.pbk', name)
            (unsigned_archive / name).write_bytes(unsigned_cat(unsigned_prefix + name))
        verified = subprocess.run(['python3', ROOT / 'tools/check-crispawn-format.py', unsigned_archive, '--source-file', source],
                                  env=verify_env, capture_output=True, timeout=120)
        (unsigned / 'independent-verification.log').write_bytes(verified.stdout + verified.stderr)
        assert verified.returncode == 0, verified.stderr.decode(errors='replace')
        state['unsigned_layout'] = unsigned_layout
        check('unsigned-ntfs-crispawn-iso-boot-copy-independent-verification-and-normal-shutdown')

        # Reproduce the reported Builder mismatch: GRUB loads a renamed ISO,
        # while LiveOS must reject the missing formal filename before writes.
        fault = output / 'missing-boot-iso'; fault.mkdir()
        failure_runtime = json.loads(json.dumps(runtime))
        failure_runtime['completion']['delay_seconds'] = 3
        failure_config = fault / 'settings.json'
        failure_config.write_text(json.dumps(failure_runtime) + '\n')
        failure_image = fault / 'usb.img'
        failure_media = usb.create(args.iso, failure_config, failure_image, fault / 'media', 2048, profile_path)
        failure_esp = partition(failure_image, failure_media, 'esp', fault)
        renamed_iso = 'crispawn-0123456789abcdef.iso'
        command([sdk / 'mren', '-i', failure_esp, '::/crispawn.iso', '::/' + renamed_iso])
        failure_grub = fault / 'grub.cfg'
        failure_grub.write_text((fault / 'media/grub.cfg').read_text().replace('crispawn.iso', renamed_iso))
        for name in ['::/EFI/BOOT/grub.cfg', '::/EFI/' + usb._boot.efi_vendor(profile) + '/grub.cfg', '::/boot/grub/grub.cfg']:
            command([sdk / 'mcopy', '-o', '-i', failure_esp, failure_grub, name])
        with failure_esp.open('rb') as src, failure_image.open('r+b') as dst:
            dst.seek(failure_media['esp_offset']); shutil.copyfileobj(src, dst); dst.flush(); os.fsync(dst.fileno())
        failure_hash = digest(failure_image)
        failed = guest(failure_image, source, fault / 'guest', expected_failure='BOOT_ISO_NOT_FOUND')
        failures = json.loads((fault / 'guest/failures.json').read_text())
        assert any(f.get('code') == 'BOOT_ISO_NOT_FOUND' and '/crispawn.iso' in json.dumps(f) for f in failures)
        assert any(p['phase'] == 'FAILED' and '/crispawn.iso' in p['detail'] for p in failed)
        assert digest(failure_image) == failure_hash
        assert digest(source) == source_hash
        check('renamed-iso-mismatch-reports-boot-iso-not-found-and-path-without-backup-writes')

        # Emulate the post-move state produced by Builder: only the saved JSON
        # retains its old archive prefix. The original completed USB stays intact.
        frozen_usb_hash = digest(image)
        legacy = json.loads(json.dumps(saved))
        for old_disk in legacy['disks']:
            old_disk['archive'] = 'perfect-backup/' + old_disk['archive'].removeprefix('crispawn/')
        legacy_bytes = (json.dumps(legacy, indent=2) + '\n').encode()
        legacy_file = output / 'legacy-session.json'
        legacy_file.write_bytes(legacy_bytes)
        migration_ntfs = output / 'migration-ntfs.img'
        with migration_ntfs.open('xb') as stream:
            stream.truncate(ntfs_file.stat().st_size)
        usb.sparse_copy(ntfs_file, migration_ntfs)
        command([host / 'usr/sbin/ntfscp', migration_ntfs, legacy_file, session_path])
        assert command([host / 'usr/bin/ntfscat', migration_ntfs, session_path]) == legacy_bytes
        migrated = output / 'migrated-usb.img'
        with migrated.open('xb') as stream:
            stream.truncate(image.stat().st_size)
        usb.sparse_copy(image, migrated)
        with migration_ntfs.open('rb') as src, migrated.open('r+b') as dst:
            dst.seek(media['ntfs_offset'])
            shutil.copyfileobj(src, dst)
            dst.flush()
            os.fsync(dst.fileno())
        esp = partition(image, media, 'esp', output)
        runtime = json.loads((ROOT / 'config/crispawn/examples/live-v2-usb.json').read_text())
        runtime['startup'] = {'mode': 'restore_select'}
        runtime['completion']['delay_seconds'] = 1
        override = output / 'restore-config.json'
        override.write_text(json.dumps(runtime) + '\n')
        command([sdk / 'mcopy', '-o', '-i', esp, override, '::/crispawn-config.json'])
        with esp.open('rb') as src, migrated.open('r+b') as dst:
            dst.seek(media['esp_offset'])
            shutil.copyfileobj(src, dst)
            dst.flush()
            os.fsync(dst.fileno())
        target = output / 'restored.img'
        with target.open('xb') as stream:
            stream.truncate(source.stat().st_size)
            stream.write(b'NOT RESTORED' * 1024)
        state['phase'] = 'restoring-moved-session'
        save()
        restored = guest(migrated, target, output / 'restore-guest', restore=True)
        assert any(p['session']['page'] == 'restore_selection' and p['can_restore'] for p in restored)
        assert digest(target) == source_hash
        recovered = output / 'restore-evidence'
        recovered.mkdir()
        _, restored_cat, restored_listing, restored_layout = ntfs(migrated, media, recovered)
        # UART probes omit the full catalog by design. The persisted final
        # status retains the actual selected session and normalized paths.
        restore_id = restored[0]['job_id']
        catalogs = []
        for directory in restored_listing('/crispawn/logs'):
            if not directory.startswith(restore_id + '-'):
                continue
            folder = recovered / 'logs' / directory
            folder.mkdir(parents=True)
            for name in restored_listing('/crispawn/logs/' + directory):
                assert re.fullmatch(r'(events-[0-9]+\.jsonl|log-info\.json|probe-final\.json)', name)
                (folder / name).write_bytes(restored_cat('/crispawn/logs/' + directory + '/' + name))
            verify_log_directory(folder)
            catalogs.append(json.loads((folder / 'probe-final.json').read_text())['session']['backups'])
        assert any(len(items) == 1 and items[0]['id'] == saved['id']
                   and items[0]['disks'][0]['archive'] == disk['archive'] for items in catalogs)
        assert restored_cat(session_path) == legacy_bytes
        for file in archive.rglob('*'):
            if file.is_file() and file.name != '.lock':
                assert restored_cat(prefix + file.relative_to(archive).as_posix()) == file.read_bytes()
        assert digest(image) == frozen_usb_hash and digest(source) == source_hash
        assert digest(args.iso) == state['iso_sha256']
        state['restore_layout'] = restored_layout
        state['restored_sha256'] = source_hash
        check('moved-session-selected-once-archive-prefix-remapped-exact-restore-original-json-and-backup-preserved')
        state['phase'] = 'complete'
    except BaseException as error:
        state.update(phase='failed', error=repr(error))
        raise
    finally:
        save()
        print('Report:', output / 'state.json', flush=True)


if __name__ == '__main__':
    main()
