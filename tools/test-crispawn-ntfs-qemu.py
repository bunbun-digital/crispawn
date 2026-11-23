#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Real Secure Boot NTFS admission, failure evidence and independent copy readback.

Uses ordinary synthetic images only. Optional Windows input must come from the
reviewed Builder virtual-media test adapter; this never formats a physical disk.
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
import struct
import subprocess
import tempfile

from crispawn_qemu import run_guest
from crispawn_log_evidence import verify_log_directory

ROOT = Path(__file__).resolve().parents[1]


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--iso', type=Path, required=True)
    parser.add_argument('--work', type=Path, required=True)
    parser.add_argument('--windows-media', type=Path,
                        help='clean-golden.vhdx, dirty-TEST-CLONE.vhdx and media-evidence.json')
    args = parser.parse_args()
    out = args.work.resolve()
    out.mkdir(parents=True, exist_ok=False)
    assert shutil.disk_usage(out).free >= 12 << 30
    iso = args.iso.resolve(strict=True)
    assert iso.is_file() and not iso.is_symlink()
    spec = importlib.util.spec_from_file_location('usb', ROOT/'tools/create-crispawn-usb-image.py')
    usb = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(usb)
    source = out/'source.img'
    with source.open('xb') as stream:
        stream.truncate(1 << 30)
        stream.write(random.Random(20261124).randbytes(8 << 20))
        stream.seek((1 << 30)-512)
        stream.write(b'CRISPAWN NTFS regression whole disk'.ljust(512, b'\0'))
    source_hash = digest(source)
    state = dict(phase='preparing', real_qemu=True, physical_usb_tested=False,
                 iso=str(iso), iso_sha256=digest(iso), source_bytes=1 << 30,
                 source_sha256=source_hash, cases=[], windows_builder_changed=False)

    def save():
        (out/'report.json').write_text(json.dumps(state, indent=2)+'\n')

    def command(argv):
        result = subprocess.run(list(map(str, argv)), capture_output=True, timeout=180)
        with (out/'commands.log').open('ab') as log:
            log.write(json.dumps(list(map(str, argv))).encode()+b'\n'+result.stderr)
        assert result.returncode == 0, (argv[0], result.stderr.decode(errors='replace'))
        return result.stdout

    def extract_partition(image, folder, kind):
        path = folder/(kind+'.img')
        with path.open('xb') as stream:
            stream.truncate(media[kind+'_bytes'])
        usb.sparse_copy(image, path, source_offset=media[kind+'_offset'], length=media[kind+'_bytes'])
        return path

    def volume_hash(image):
        h = hashlib.sha256()
        with image.open('rb') as stream:
            stream.seek(media['ntfs_offset'])
            left = media['ntfs_bytes']
            while left:
                data = stream.read(min(left, 1 << 20))
                assert data
                h.update(data)
                left -= len(data)
        return h.hexdigest()

    def ntfs_tools(image):
        def cat(path):
            return command(['ntfscat', image, path])

        def listing(path):
            names = command(['ntfsls', '-p', path, image]).decode().splitlines()
            assert all(n and not any(c in n for c in '/\\\0\r') for n in names)
            return [n for n in names if n not in ('.', '..')]
        return cat, listing

    def verify_copy(image, folder):
        ntfs = extract_partition(image, folder, 'ntfs')
        cat, listing = ntfs_tools(ntfs)
        assert command(['ntfslabel', ntfs]).decode().strip() == 'CRISPAWN'
        assert 'perfect-backup' not in listing('/')
        assert set(listing('/crispawn')) == {'backups', 'logs', 'sessions', 'reports', 'work'}
        sessions = listing('/crispawn/sessions')
        assert len(sessions) == 1
        session = json.loads(cat('/crispawn/sessions/'+sessions[0]))
        assert len(session['disks']) == 1
        path = session['disks'][0]['archive']
        assert re.fullmatch(r'crispawn/backups/[A-Za-z0-9_-]+(?:/[A-Za-z0-9_-]+)*', path)
        archive = folder/'archive'
        archive.mkdir()
        (archive/'data').mkdir()
        for name in ('metadata.json', 'plan.json', 'files.csv', 'checksums.sha256', 'COMMITTED', 'journal.jsonl', '.lock'):
            (archive/name).write_bytes(cat('/'+path+'/'+name))
        with (archive/'files.csv').open(newline='') as stream:
            for row in csv.DictReader(stream):
                name = row['relative_path']
                assert re.fullmatch(r'data/data-[0-9]{6}\.pbk', name)
                (archive/name).write_bytes(cat('/'+path+'/'+name))
        checked = subprocess.run(['python3', ROOT/'tools/check-crispawn-format.py', archive, '--source-file', source],
                                 env=dict(os.environ, PYTHONPATH=str(ROOT/'build/verifier-libs')),
                                 capture_output=True, timeout=180)
        (folder/'independent-verification.log').write_bytes(checked.stdout+checked.stderr)
        assert checked.returncode == 0
        records, finals = [], []
        for group in listing('/crispawn/logs'):
            assert re.fullmatch(r'[0-9a-f]{32}-(complete|live)-[0-9a-f]{8}', group)
            target = folder/'logs'/group
            target.mkdir(parents=True)
            for name in listing('/crispawn/logs/'+group):
                assert re.fullmatch(r'events-[0-9]+\.jsonl|log-info\.json|probe-final\.json', name)
                data = cat('/crispawn/logs/'+group+'/'+name)
                (target/name).write_bytes(data)
                if name.startswith('events-'):
                    records.extend(json.loads(line) for line in data.splitlines())
                elif name == 'probe-final.json':
                    finals.append(json.loads(data))
            verify_log_directory(target)
        mounts = [r['value'] for r in records if r.get('kind') == 'ntfs_mount_diagnostics']
        # The final log export also includes the live log's initial records.
        assert mounts and len({json.dumps(m, sort_keys=True) for m in mounts}) == 1
        assert mounts[0]['volume']['dirty'] is False
        assert mounts[0]['mount_attempted'] and mounts[0]['code'] == 'NTFS_MOUNTED'
        assert any(r.get('kind') == 'usb_boot_iso_verified' for r in records)
        assert any(int(f['snapshot']['read_raw_bytes']) == 1 << 30 and
                   int(f['snapshot']['verified_raw_bytes']) == 1 << 30 for f in finals if f.get('snapshot'))
        (folder/'mount-diagnostics.json').write_text(json.dumps(mounts[0], indent=2)+'\n')
        return dict(committed=True, independent_readback=True, ntfs_logs_verified=True,
                    read_raw_bytes=1 << 30, verified_raw_bytes=1 << 30)

    save()
    try:
        base = out/'clean-base.img'
        if args.windows_media:
            origin = args.windows_media.resolve(strict=True)
            evidence_path = origin/'media-evidence.json'
            assert evidence_path.is_file() and evidence_path.stat().st_size <= 65536
            evidence = json.loads(evidence_path.read_text())
            assert evidence['iso_sha256'] == state['iso_sha256']
            assert evidence['builder_commit'] == 'c20f09f1a7b45777b6a4550576d967367a0f69d6'
            assert evidence['builder_helper_sha256'] == '4e458d93eb04257ff7b8d9e140b8431d89084a2cadda7d106324a10485b1c946'
            assert evidence['native_detach_verified'] is True
            assert evidence['virtual_bytes'] == 4 << 30
            parts = evidence['partitions']
            assert len(parts) == 2 and [p['number'] for p in parts] == [1, 2]
            media = dict(esp_offset=int(parts[0]['offset']), esp_bytes=int(parts[0]['bytes']),
                         ntfs_offset=int(parts[1]['offset']), ntfs_bytes=int(parts[1]['bytes']))
            assert media['esp_bytes'] == 256 << 20
            assert 1 << 20 <= media['esp_offset'] < media['ntfs_offset'] < 4 << 30
            assert media['ntfs_offset'] + media['ntfs_bytes'] <= 4 << 30
            for key, filename, output in [('golden', 'clean-golden.vhdx', base),
                                           ('dirty_test_clone', 'dirty-TEST-CLONE.vhdx', out/'windows-dirty-base.img')]:
                image = origin/filename
                assert image.is_file() and not image.is_symlink()
                assert digest(image) == evidence[key]['sha256']
                info = json.loads(command(['qemu-img', 'info', '--output=json', image]))
                assert info['format'] == 'vhdx' and info['virtual-size'] == 4 << 30
                assert not info.get('backing-filename')
                command(['qemu-img', 'convert', '-f', 'vhdx', '-O', 'raw', image, output])
            state['formatter'] = 'Windows Builder Format-Volume, virtual-media adapter'
            state['windows_provenance'] = evidence
        else:
            config = json.loads((ROOT/'config/crispawn/examples/live-v2-usb.json').read_text())
            config['startup']['delay_seconds'] = 0
            config['completion']['delay_seconds'] = 8
            settings = out/'crispawn-config.json'
            settings.write_text(json.dumps(config, indent=2)+'\n')
            media = usb.create(iso, settings, base, out/'media', 2048, ROOT/'build/signed-profile')
            state['formatter'] = 'Linux mkntfs (not Windows Builder)'
        (out/'media.json').write_text(json.dumps(media, indent=2)+'\n')
        state['phase'] = 'running'
        save()

        cases = [('clean', None), ('dirty', 'NTFS_VOLUME_DIRTY')]
        if not args.windows_media:
            cases += [('unreadable-volume-state', 'NTFS_STATE_UNAVAILABLE'), ('kernel-mount-failure', 'NTFS_MOUNT_FAILED')]
        for name, expected in cases:
            folder = out/name
            folder.mkdir()
            image = folder/'usb.img'
            template = out/'windows-dirty-base.img' if args.windows_media and name == 'dirty' else base
            with image.open('xb') as stream:
                stream.truncate(template.stat().st_size)
            usb.sparse_copy(template, image)
            if expected and not args.windows_media:
                # Controlled, explicitly owned synthetic NTFS metadata. No mount
                # or repair is used to prepare the invalid-volume cases.
                with image.open('r+b') as stream:
                    offset = media['ntfs_offset']
                    stream.seek(offset)
                    boot = stream.read(512)
                    cluster = struct.unpack_from('<H', boot, 11)[0] * boot[13]
                    code = struct.unpack_from('b', boot, 64)[0]
                    record_size = 1 << -code if code < 0 else code * cluster
                    mft = struct.unpack_from('<Q', boot, 48)[0] * cluster
                    if name == 'dirty':
                        for lcn in (48, 56):
                            at = offset + struct.unpack_from('<Q', boot, lcn)[0] * cluster + 3 * record_size
                            stream.seek(at)
                            record = stream.read(record_size)
                            attr = struct.unpack_from('<H', record, 20)[0]
                            while struct.unpack_from('<I', record, attr)[0] != 0x70:
                                length = struct.unpack_from('<I', record, attr+4)[0]
                                assert 24 <= length <= record_size-attr
                                attr += length
                            value = struct.unpack_from('<H', record, attr+20)[0]
                            flag = attr+value+10
                            assert flag % 512 <= 508 and record[flag:flag+2] == b'\0\0'
                            stream.seek(at+flag)
                            stream.write(b'\x01\0')
                    elif name == 'kernel-mount-failure':
                        # ntfs3 rejects index-record size zero. The read-only
                        # $Volume parser does not inspect directory indexes.
                        # Change the backup boot sector too, so it cannot mask
                        # this controlled invalid-geometry fixture.
                        for boot_at in (offset, offset+media['ntfs_bytes']-512):
                            stream.seek(boot_at+68)
                            stream.write(b'\0')
                    else:
                        stream.seek(offset+mft)
                        stream.write(b'FAIL')
                    stream.flush()
                    os.fsync(stream.fileno())
            before = volume_hash(image)
            evidence = folder/'guest'
            evidence.mkdir()
            print('CASE', name, flush=True)
            probes = run_guest(Path(tempfile.mkdtemp(prefix='cp-ntfs-')), evidence, image, source,
                               timeout=240, readonly=True, secure_boot=True, cpus=8, memory_mib=8192,
                               capture_gui=True, expected_failure_code=expected, record_frames=True)
            result = dict(name=name, secure_boot=True, physical_usb=False)
            if expected:
                assert all(int((p.get('snapshot') or {}).get('read_raw_bytes', 0)) == 0 for p in probes)
                # An early failure must be recoverable after automatic poweroff.
                listing = command(['mdir', '-b', '-i', f"{image}@@{media['esp_offset']}", '::/crispawn/diagnostics/']).decode().splitlines()
                assert len(listing) == 1
                path = listing[0].replace('\\', '/')
                assert re.fullmatch(r'::/crispawn/diagnostics/ntfs-mount-[0-9a-f]{32}\.json', path)
                report_file = folder/'ntfs-mount-diagnostics.json'
                command(['mcopy', '-i', f"{image}@@{media['esp_offset']}", listing[0], report_file])
                report = json.loads(report_file.read_text())
                assert report['code'] == expected and report['force'] is False
                assert report['device']['path'] == '/dev/sda2' and report['read_only_inspection']
                if name == 'dirty':
                    assert report['volume']['dirty'] is True and report['mount_attempted'] is False
                    assert 'Windows' in report['user_message'] and 'Builder' in report['user_message']
                    assert volume_hash(image) == before, 'dirty NTFS partition changed'
                if name == 'unreadable-volume-state':
                    assert report['inspection_error'] and not report['mount_attempted']
                if name == 'kernel-mount-failure':
                    assert report['mount_attempted'] and report['errno'] is not None
                    assert report['kernel']['available'] and any('ntfs3' in s for s in report['kernel']['messages'])
                # The fallback may add diagnostics only; signed boot files and ISO stay intact.
                for remote, expected_hash in [('crispawn.iso', state['iso_sha256']),
                                              ('EFI/BOOT/BOOTX64.EFI', digest(ROOT/'build/signed-profile/shimx64.efi')),
                                              ('EFI/BOOT/grubx64.efi', digest(ROOT/'build/signed-profile/grubx64.efi'))]:
                    target = folder/Path(remote).name
                    command(['mcopy', '-i', f"{image}@@{media['esp_offset']}", '::/'+remote, target])
                    assert digest(target) == expected_hash
                result.update(code=expected, source_read_bytes=0, diagnostic_persisted_on_esp=True,
                              dirty_ntfs_unchanged=name == 'dirty')
            else:
                result.update(verify_copy(image, folder))
            assert any(p['secure_boot']['enabled'] for p in probes if p.get('secure_boot'))
            assert digest(source) == source_hash
            result['normal_poweroff'] = True
            state['cases'].append(result)
            save()
            print('CASE_COMPLETE', name, flush=True)
        state['phase'] = 'complete'
    except BaseException as error:
        state.update(phase='failed', error=repr(error))
        raise
    finally:
        save()
        print('REPORT', out/'report.json', flush=True)


if __name__ == '__main__':
    main()
