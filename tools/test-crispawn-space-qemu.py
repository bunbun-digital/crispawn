#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Exercise compressed copies to smaller USB media and both low-space outcomes.

Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
Only new ordinary synthetic images. No physical USB, host mount or Windows change.
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


def digest(file):
    with file.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def verify_case(folder, source, media, runtime, usb, host, env, *, proof=None):
    """Read-only acceptance of an already stopped synthetic guest and NTFS."""
    case = folder.name
    success = case == 'smaller-destination'
    stay = case == 'low-space-stay'
    guest = folder/'guest'
    image = folder/'usb.img'
    proof = folder if proof is None else proof
    probes = [json.loads(line) for line in (guest/'probes.jsonl').read_text().splitlines()]
    qmp = json.loads((guest/'qmp.json').read_text())
    normal_off = any(e.get('event') == 'SHUTDOWN' and e.get('data',{}).get('guest') is True for e in qmp)
    assert normal_off is not stay
    if stay:
        held = json.loads((guest/'failure-stay.json').read_text())
        assert held['verified'] and held['observed_seconds'] >= 8

    def command(argv, timeout=120):
        result = subprocess.run(list(map(str, argv)), env=env, capture_output=True, timeout=timeout)
        with (proof/'commands.log').open('ab') as log:
            log.write(json.dumps(list(map(str, argv))).encode()+b'\n'+result.stdout+result.stderr)
        assert result.returncode == 0, (argv[0], result.stderr.decode(errors='replace'))
        return result.stdout

    security = [p['secure_boot'] for p in probes if p.get('secure_boot')]
    assert security and all(s['enabled'] and not s['setup_mode'] and not s['shim_validation_disabled']
                            and '[integrity]' in s['lockdown'] for s in security)
    assert any(p['phase'] == 'COPYING' for p in probes)
    ntfs = proof/'ntfs.img'
    with ntfs.open('xb') as stream:
        stream.truncate(media['ntfs_bytes'])
    usb.sparse_copy(image, ntfs, source_offset=media['ntfs_offset'], length=media['ntfs_bytes'])

    def cat(path): return command([host/'usr/bin/ntfscat', ntfs, path])
    def listing(path):
        return [v for v in command([host/'usr/bin/ntfsls','-p',path,ntfs]).decode().splitlines()
                if v not in ('.','..')]

    assert command([host/'usr/sbin/ntfslabel',ntfs]).decode().strip() == 'CRISPAWN'
    reports = listing('/crispawn/reports')
    assert len(reports) == 1
    report = json.loads(cat('/crispawn/reports/'+reports[0]))
    (proof/'job-report.json').write_text(json.dumps(report,indent=2)+'\n')
    archives = listing('/crispawn/backups')
    assert len(archives) == 1 and re.fullmatch(r'[0-9a-f]{32}-00',archives[0])
    prefix = '/crispawn/backups/'+archives[0]+'/'
    if success:
        assert report['status']=='complete' and report['engine']['dataset_committed']
        archive = proof/'archive'
        archive.mkdir()
        (archive/'data').mkdir()
        for name in ['metadata.json','plan.json','files.csv','checksums.sha256','COMMITTED','journal.jsonl','.lock']:
            (archive/name).write_bytes(cat(prefix+name))
        with (archive/'files.csv').open(newline='') as stream:
            for row in csv.DictReader(stream):
                name=row['relative_path']
                assert re.fullmatch(r'data/data-[0-9]{6}\.pbk',name)
                descriptor = cat(prefix+name)
                (archive/name).write_bytes(descriptor)
                # Segmented descriptors name their physical parts under data/.
                if descriptor[:1] == b'{':
                    value = json.loads(descriptor)
                    assert value['kind'] == 'crispawn.segmented_file'
                    for part in value['parts']:
                        relative = part['path']
                        assert re.fullmatch(r'segments/[0-9a-f]{8}/[0-9a-f]{32}\.bin', relative)
                        target = archive/'data'/relative
                        target.parent.mkdir(parents=True, exist_ok=True)
                        target.write_bytes(cat(prefix+'data/'+relative))
        verified=subprocess.run(['python3',ROOT/'tools/check-crispawn-format.py',archive,'--source-file',source],
                                env=dict(os.environ,PYTHONPATH=str(ROOT/'build/verifier-libs')),
                                capture_output=True,timeout=180)
        (proof/'independent-verification.log').write_bytes(verified.stdout+verified.stderr)
        assert verified.returncode == 0,verified.stderr
    else:
        assert report['status']=='failed' and report['error_code']=='DESTINATION_LOW_SPACE'
        assert 'COMMITTED' not in listing(prefix)
        # Progress is published at checkpoints, so a low-space failure can
        # precede the first nonzero public snapshot. Check actual NTFS data.
        saved_bytes = 0
        data_files = []
        for name in listing(prefix+'data'):
            if name == 'segments':
                # Segmented descriptors keep physical parts below data/segments/.
                for stream in listing(prefix+'data/segments'):
                    assert re.fullmatch(r'[0-9a-f]{8}',stream)
                    for part in listing(prefix+'data/segments/'+stream):
                        assert re.fullmatch(r'[0-9a-f]{32}\.bin',part)
                        data_files.append('segments/'+stream+'/'+part)
            else:
                assert re.fullmatch(r'data-[0-9]{6}\.pbk(?:\.part)?',name)
                data_files.append(name)
        assert data_files
        for name in data_files:
            with tempfile.TemporaryFile() as errors:
                proc = subprocess.Popen([str(host/'usr/bin/ntfscat'),str(ntfs),prefix+'data/'+name],
                                        env=env,stdout=subprocess.PIPE,stderr=errors)
                try:
                    while chunk := proc.stdout.read(1 << 20):
                        saved_bytes += len(chunk)
                    assert proc.wait(timeout=30) == 0
                finally:
                    proc.stdout.close()
                    if proc.poll() is None:
                        proc.kill()
                        proc.wait(timeout=10)
        assert saved_bytes > 0
        (proof/'written-data.json').write_text(json.dumps({'saved_bytes':saved_bytes,
            'checkpoint_snapshot_may_still_be_zero':True},indent=2)+'\n')
        serial=(guest/'serial.log').read_text(errors='replace')
        assert '保存先の空き容量が1GB' in serial
        samples=[p for p in probes if p['phase']=='FAILED']
        assert samples and not any(p.get('finalization_complete') for p in probes)
        if not stay:
            countdown=[p for p in samples if p.get('shutdown_countdown_seconds') is not None]
            assert countdown and max(p['shutdown_countdown_seconds'] for p in countdown)==3
            off=next(p for p in samples if p['poweroff_requested'])
            assert off['monotonic_ms']-countdown[0]['monotonic_ms'] >= 2500
        assert (guest/'failure.png').is_file()
    logs=[]
    for directory in listing('/crispawn/logs'):
        assert re.fullmatch(r'[0-9a-f]{32}-(?:live|complete|failure)-[0-9a-f]{8}',directory)
        logdir=proof/'logs'/directory
        logdir.mkdir(parents=True)
        for name in listing('/crispawn/logs/'+directory):
            if name == 'io-observer':
                # The independent I/O observer's own records (see IO-PIPELINE.md).
                (logdir/name).mkdir()
                for record in listing('/crispawn/logs/'+directory+'/'+name):
                    assert re.fullmatch(r'(samples-[0-9]+\.jsonl|latest\.json|final\.json|finished\.json)',record)
                    (logdir/name/record).write_bytes(cat('/crispawn/logs/'+directory+'/'+name+'/'+record))
                continue
            assert re.fullmatch(r'(events-[0-9]+\.jsonl|log-info\.json|probe-final\.json)',name)
            (logdir/name).write_bytes(cat('/crispawn/logs/'+directory+'/'+name))
        logs.append(verify_log_directory(logdir))
    assert logs
    return dict(case=case,passed=True,source_bytes=source.stat().st_size,
                                 destination_bytes=media['ntfs_bytes'],failure_action=runtime['failure']['action'],
                                 normal_guest_poweroff=not stay,configured_stopped_state=stay,log_checks=logs)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--iso', required=True, type=Path)
    parser.add_argument('--work', required=True, type=Path)
    parser.add_argument('--cases', nargs='+', choices=['smaller-destination','low-space-poweroff','low-space-stay'],
                        default=['smaller-destination','low-space-poweroff','low-space-stay'])
    args = parser.parse_args()
    output = args.work.resolve()
    output.mkdir(parents=True, exist_ok=False)
    assert args.iso.is_file() and not args.iso.is_symlink()
    assert shutil.disk_usage(output).free > 12 << 30
    spec = importlib.util.spec_from_file_location('usb', ROOT/'tools/create-crispawn-usb-image.py')
    usb = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(usb)
    host, env = usb.tools()
    state = dict(phase='preparing', iso=str(args.iso.resolve()), iso_sha256=digest(args.iso),
                 simulated=False, qemu_real_guest=True, hyperv_tested=False, physical_devices=False,
                 cpus=2, memory_mib=4096, minimum_free_bytes=1_000_000_000,
                 completion_delay_seconds=3, results=[])

    def save():
        (output/'state.json').write_text(json.dumps(state, indent=2)+'\n')

    save()
    try:
        compressible = output/'compressible-source.img'
        with compressible.open('xb') as stream:
            stream.truncate(3 << 30)
            stream.write(random.Random(23201).randbytes(4 << 20))
            stream.seek((3 << 30)-512)
            stream.write(b'END OF WHOLE DISK'.ljust(512, b'\0'))
        incompressible = output/'incompressible-source.img'
        rng = random.Random(23202)
        with incompressible.open('xb') as stream:
            stream.truncate(1 << 30)
            for _ in range(75):
                stream.write(rng.randbytes(8 << 20))
        hashes = {compressible:digest(compressible), incompressible:digest(incompressible)}
        for case in args.cases:
            state['phase'] = case
            save()
            folder = output/case
            folder.mkdir()
            success = case == 'smaller-destination'
            stay = case == 'low-space-stay'
            source = compressible if success else incompressible
            runtime = json.loads((ROOT/'config/crispawn/examples/live-v2-usb.json').read_text())
            runtime['startup']['delay_seconds'] = 0
            runtime['completion']['delay_seconds'] = 3
            runtime['failure'] = {'action':'stay' if stay else 'poweroff'}
            runtime['processing']['initial_profile'] = 'incompressible'
            settings = folder/'live.json'
            settings.write_text(json.dumps(runtime)+'\n')
            image = folder/'usb.img'
            media = usb.create(args.iso, settings, image, folder/'media', 2048 if success else 1536,
                               ROOT/usb._boot.DEFAULT_SECURE_PROFILE_NAME)
            if success:
                assert source.stat().st_size > media['ntfs_bytes']
            guest = folder/'guest'
            guest.mkdir()
            sockets = Path(tempfile.mkdtemp(prefix='.crispawn-space-'))
            probes = run_guest(sockets, guest, image, source, timeout=900, readonly=True,
                               secure_boot=True, cpus=2, memory_mib=4096, capture_gui=True,
                               expected_failure_code=None if success else 'DESTINATION_LOW_SPACE',
                               expect_failure_stay=stay)
            state['results'].append(verify_case(folder, source, media, runtime, usb, host, env))
            assert digest(source)==hashes[source]
            save()
            print('CHECK',case,flush=True)
        assert digest(args.iso)==state['iso_sha256']
        state['phase']='complete'
    except BaseException as error:
        state.update(phase='failed',error=repr(error))
        raise
    finally:
        save()
        print('Report:',output/'state.json',flush=True)


if __name__=='__main__':
    main()
