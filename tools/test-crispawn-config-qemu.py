#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Real runtime-v2 copy/restore/override tests on new ordinary synthetic images.

Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
No physical disk, host mount, Hyper-V or Windows operation is performed.
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

from backup_qemu_common import run_guest
from backup_log_evidence import verify_log_directory

ROOT = Path(__file__).resolve().parents[1]


def module(name, file):
    spec = importlib.util.spec_from_file_location(name, ROOT / 'tools' / file)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


def digest(file):
    with file.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--iso', type=Path, required=True)
    parser.add_argument('--work', type=Path, required=True)
    parser.add_argument('--secure-boot-profile', type=Path, default=ROOT / 'build-perfect-backup-secure-profile-debian-v2')
    args = parser.parse_args()
    output = args.work.resolve(); output.mkdir(parents=True, exist_ok=False)
    assert args.iso.is_file() and not args.iso.is_symlink()
    assert shutil.disk_usage(output).free > 12 * 1024**3
    usb = module('usb', 'create-backup-usb-image.py')
    host, env = usb.tools()
    state = dict(phase='preparing', physical_devices=False, hyperv_tested=False,
                 qemu_real_guest=True, secure_boot=True, cpus=2, memory_mib=4096, checks=[])

    def save(): (output / 'state.json').write_text(json.dumps(state, indent=2) + '\n')
    def check(name):
        state['checks'].append(name); save(); print('CHECK', name, flush=True)
    def config():
        return json.loads((ROOT / 'config/crispawn/examples/live-v2-usb.json').read_text())
    def create(name, runtime):
        work = output / name; work.mkdir()
        settings = work / 'runtime.json'; settings.write_text(json.dumps(runtime, indent=2) + '\n'); settings.chmod(0o600)
        media = usb.create(args.iso, settings, work / 'usb.img', work / 'media', size_mib=2048,
                           secure_profile=args.secure_boot_profile)
        return work, media
    def guest(work, readonly=True, expected=None, scenario=None):
        evidence = work / 'guest'; evidence.mkdir()
        sockets = Path(tempfile.mkdtemp(prefix='.crispawn-config-'))
        probes = run_guest(sockets, evidence, work/'usb.img', source if readonly else target,
                           timeout=300, readonly=readonly, secure_boot=True, cpus=2, memory_mib=4096,
                           capture_gui=True, source_read_bps=32<<20, expected_failure_code=expected,
                           ui_scenario=scenario)
        security = [p['secure_boot'] for p in probes if p.get('secure_boot')]
        assert security and all(s['enabled'] and not s['setup_mode'] and not s['shim_validation_disabled']
                                and ('[integrity]' in s['lockdown'] or '[confidentiality]' in s['lockdown']) for s in security)
        return probes
    def replace_override(work, media, runtime):
        override=work/'override.json'; override.write_text(json.dumps(runtime)+'\n'); override.chmod(0o600)
        esp=work/'override-esp.img'
        with esp.open('xb') as stream:stream.truncate(media['esp_bytes'])
        usb.sparse_copy(work/'usb.img',esp,length=media['esp_bytes'],source_offset=media['esp_offset'])
        subprocess.run([ROOT/'build/os/host/bin/mcopy','-o','-i',esp,override,'::/perfect-backup.json'],check=True)
        with esp.open('rb') as src,(work/'usb.img').open('r+b') as dst:
            dst.seek(media['esp_offset']);shutil.copyfileobj(src,dst);dst.flush();os.fsync(dst.fileno())
    def ntfs(work, media):
        image = work / 'ntfs.img'
        with image.open('xb') as stream: stream.truncate(media['ntfs_bytes'])
        usb.sparse_copy(work/'usb.img', image, length=media['ntfs_bytes'], source_offset=media['ntfs_offset'])
        def cat(path):
            r = subprocess.run([host/'usr/bin/ntfscat',image,path],env=env,capture_output=True,timeout=60)
            assert r.returncode == 0, (path,r.stderr.decode(errors='replace'))
            return r.stdout
        def listing(path):
            r = subprocess.run([host/'usr/bin/ntfsls','-p',path,image],env=env,capture_output=True,timeout=30)
            assert r.returncode == 0, (path,r.stderr.decode(errors='replace'))
            return [s for s in r.stdout.decode().splitlines() if s not in ('.','..')]
        return cat, listing

    save()
    try:
        source = output / 'source.img'
        with source.open('xb') as stream:
            stream.truncate(512<<20)
            stream.write(random.Random(81721).randbytes(16<<20))
            stream.seek(128<<20); stream.write(b'CRISPAWN config v2 source\n'*100_000)
        original = digest(source)
        runtime = config()
        runtime['startup']['delay_seconds'] = 3
        runtime['completion']['delay_seconds'] = 2
        runtime['ui']['language'] = 'en'
        # The production adapter prefers the NVMe WWID over its serial fallback.
        # These are the fixed vendor/model/serial/namespace of this QEMU fixture.
        source_id = 'wwid:nvme.1b36-' + b'PB-SOURCE-0001'.hex() + '-' + b'QEMU NVMe Ctrl'.hex() + '-00000001'
        runtime['sources'] = {'mode':'selected_ids','ids':[source_id]}
        runtime['destination']['backup_directory'] = 'custom/copies'
        runtime['processing']['initial_profile'] = 'incompressible'
        runtime['logging']['sync_interval_ms'] = 250
        copy, media = create('gui-auto',runtime)
        probes = guest(copy)
        assert any(p.get('gui_ready') and p['phase']=='COPYING' and p['language']=='en' for p in probes)
        assert any(p.get('countdown_seconds') is not None and 0<=p['countdown_seconds']<=3 for p in probes)
        assert any(p.get('finalization_complete') and p.get('shutdown_countdown_seconds') is not None for p in probes)
        cat, listing = ntfs(copy,media)
        groups=listing('/crispawn/sessions'); assert len(groups)==1
        saved=json.loads(cat('/crispawn/sessions/'+groups[0])); assert len(saved['disks'])==1
        disk=saved['disks'][0]; assert disk['archive'].startswith('custom/copies/')
        archive=copy/'archive'; archive.mkdir(); (archive/'data').mkdir()
        for name in ['metadata.json','plan.json','files.csv','checksums.sha256','COMMITTED','journal.jsonl','.lock']:
            (archive/name).write_bytes(cat('/'+disk['archive']+'/'+name))
        with (archive/'files.csv').open(newline='') as stream:
            for row in csv.DictReader(stream):
                name=row['relative_path']; assert re.fullmatch(r'data/data-[0-9]{6}\.pbk',name)
                (archive/name).write_bytes(cat('/'+disk['archive']+'/'+name))
        verify_env=dict(os.environ,PYTHONPATH=str(ROOT/'build/verifier-libs'))
        r=subprocess.run(['python3',ROOT/'tools/check-backup-format.py',archive,'--source-file',source],env=verify_env,capture_output=True,text=True,timeout=120)
        (copy/'independent-verification.log').write_text(r.stdout+r.stderr); assert r.returncode==0,r.stderr
        for directory in listing('/crispawn/logs'):
            folder=copy/'logs'/directory; folder.mkdir(parents=True)
            for name in listing('/crispawn/logs/'+directory):
                assert re.fullmatch(r'(events-[0-9]+\.jsonl|log-info\.json|probe-final\.json)',name)
                (folder/name).write_bytes(cat('/crispawn/logs/'+directory+'/'+name))
            verify_log_directory(folder)
        records=[json.loads(line) for file in (copy/'logs').glob('*/events-*.jsonl') for line in file.read_text().splitlines()]
        applied=next(r['value'] for r in records if r.get('kind')=='runtime_configured')
        assert applied['runtime_version']==2 and applied['ui_mode']=='gui' and applied['startup']['delay_seconds']==3
        check('v2-gui-selected-source-language-delay-custom-path-continuous-logs-and-independent-blake3-verification')

        # Restore uses the exact same USB data, but a new owned copy of the
        # image. The external override requests selection, never silent writes.
        restore=output/'restore-select'; restore.mkdir()
        with (restore/'usb.img').open('xb') as stream: stream.truncate((copy/'usb.img').stat().st_size)
        usb.sparse_copy(copy/'usb.img',restore/'usb.img')
        runtime['startup']={'mode':'restore_select'}
        replace_override(restore,media,runtime)
        target=output/'restored.img'
        with target.open('xb') as stream:stream.truncate(source.stat().st_size);stream.write(b'not restored yet')
        restored=guest(restore,readonly=False,scenario='restore')
        assert any(p['phase']=='MENU' and p['session']['page']=='restore_selection' for p in restored)
        assert any(p['phase']=='RESTORING' for p in restored)
        assert digest(target)==original
        check('configured-restore-selection-explicit-confirmation-inline-verification-and-exact-restored-bytes')

        runtime=config();runtime['startup']['delay_seconds']=0;runtime['ui']['mode']='headless'
        runtime['completion']['delay_seconds']=0;runtime['logging']['policy']='never'
        runtime['processing']['initial_profile']='incompressible'
        headless,headless_media=create('headless',runtime)
        runtime['destination']['volume']={'mode':'uuid','uuid':headless_media['data_uuid']}
        replace_override(headless,headless_media,runtime)
        probes=guest(headless)
        assert not any(p.get('gui_ready') or p.get('logs_persisted') for p in probes)
        assert any(p['data_uuid']==headless_media['data_uuid'] and p['phase']=='COPYING' for p in probes)
        check('headless-explicit-usb-volume-uuid-no-input-no-countdown-no-persistent-logs-and-normal-poweroff')

        runtime=config();runtime['ui']['mode']='headless';runtime['startup']['delay_seconds']=0
        runtime['sources']={'mode':'selected_ids','ids':['wwid:DELIBERATELY-ABSENT']}
        runtime['processing']['io_policy']['failure_grace_seconds']=1
        missing,missing_media=create('missing-source',runtime)
        probes=guest(missing,expected='SOURCE_NOT_FOUND')
        cat_missing,list_missing=ntfs(missing,missing_media)
        directories=list_missing('/crispawn/logs'); assert directories
        error_records=[json.loads(line) for directory in directories for name in list_missing('/crispawn/logs/'+directory)
                       if name.startswith('events-') for line in cat_missing('/crispawn/logs/'+directory+'/'+name).decode().splitlines()]
        assert any('SOURCE_NOT_FOUND' in json.dumps(record) for record in error_records)
        check('missing-configured-source-does-not-fallback-logs-failure-and-shuts-down-without-hanging')
        assert digest(source)==original
        state.update(phase='complete',iso=str(args.iso.resolve()),iso_sha256=digest(args.iso),source_sha256=original)
    except BaseException as error:
        state.update(phase='failed',error=repr(error));raise
    finally:
        save();print('Report:',output/'state.json',flush=True)


if __name__=='__main__':main()
