#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Build the USB-only CRISPAWN ISO from pinned public sources."""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
from urllib.request import urlopen

ROOT = Path(__file__).resolve().parents[1]

def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()

def fetch(item, cache):
    name = item['url'].rsplit('/', 1)[1]
    target = cache / name
    cache.mkdir(parents=True, exist_ok=True)
    if target.exists():
        assert digest(target) == item['sha256'], 'cached input hash mismatch: ' + name
        return target
    temporary = target.with_name(name + '.partial')
    with urlopen(item['url'], timeout=60) as source, temporary.open('xb') as output:
        shutil.copyfileobj(source, output, 1 << 20)
    assert digest(temporary) == item['sha256'], 'download hash mismatch: ' + name
    temporary.rename(target)
    return target

def run(argv, log, env, cwd=ROOT):
    print('RUN', Path(str(argv[0])).name, ' '.join(str(x) for x in argv[1:3]), flush=True)
    with log.open('ab') as stream:
        stream.write((json.dumps(list(map(str, argv)))+'\n').encode())
        result = subprocess.run(list(map(str, argv)), env=env, cwd=cwd,
                                stdout=stream, stderr=subprocess.STDOUT)
    if result.returncode:
        raise RuntimeError('build command failed; see ' + str(log))

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--work-dir', type=Path, default=ROOT/'build')
    parser.add_argument('--download-cache', type=Path)
    parser.add_argument('--buildroot-source', type=Path, help='optional clean upstream source cache')
    parser.add_argument('--jobs', type=int, default=min(os.cpu_count() or 1, 8))
    parser.add_argument('--release-id', default='0.1.0-alpha.3')
    args = parser.parse_args()
    assert re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]{0,127}", args.release_id), "invalid release ID"
    work = args.work_dir.resolve()
    assert work != ROOT and not ROOT.is_relative_to(work)
    work.mkdir(parents=True, exist_ok=True)
    assert shutil.disk_usage(work).free > 6 << 30, 'at least 6 GiB free is required'
    lock = json.loads((ROOT/'sources.lock.json').read_text())
    cache = (args.download_cache or work/'downloads').resolve()
    cache.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    toolchain = work/'toolchain'
    owner = work/'owner.json'
    identity = {'kind':'crispawn.community.build','schema_version':1,'source':str(ROOT),
                'lock_sha256':digest(ROOT/'sources.lock.json')}
    if owner.exists():
        assert json.loads(owner.read_text()) == identity, 'work directory belongs to another build'
    else:
        assert not any(work.iterdir()) or set(p.name for p in work.iterdir()) == {'downloads'}
        owner.write_text(json.dumps(identity,indent=2)+'\n')
    state = {'phase':'preparing','release_id':args.release_id,'started_at':datetime.now(timezone.utc).isoformat(),
             'preserved_development_iso_used':False,'private_sdk_used':False,'network_enabled':False}
    report = work/'build.json'
    def save(): report.write_text(json.dumps(state,indent=2)+'\n')
    save()
    try:
        if not (toolchain/'crispawn-ready.json').exists():
            for item in lock['rust']['components']:
                archive = fetch(item, cache/'rust')
                with tempfile.TemporaryDirectory(prefix='rust-component-', dir=work) as folder:
                    with tarfile.open(archive,'r:xz') as package:
                        package.extractall(folder, filter='data')
                    roots = list(Path(folder).iterdir())
                    assert len(roots)==1 and roots[0].is_dir()
                    run(['sh', roots[0]/'install.sh', '--prefix='+str(toolchain), '--disable-ldconfig'],
                        work/'toolchain.log',env,cwd=roots[0])
            (toolchain/'crispawn-ready.json').write_text(json.dumps(lock['rust'],indent=2)+'\n')
        env.update(PATH=str(toolchain/'bin')+os.pathsep+env['PATH'],
                   CARGO_HOME=str(work/'cargo-home'), CARGO_TARGET_DIR=str(work/'cargo-target'))
        run([toolchain/'bin/rustc','--version'],work/'toolchain.log',env)
        run([toolchain/'bin/cargo','fetch','--locked','--manifest-path',ROOT/'examples/crispawn/rust/Cargo.toml'],
            work/'cargo-fetch.log',env)
        upstream = lock['buildroot']
        source = args.buildroot_source.resolve() if args.buildroot_source else work/'buildroot-source'
        if not source.exists():
            source.mkdir()
            run(['git','init',source],work/'buildroot-source.log',env)
            run(['git','-C',source,'fetch','--depth=1',upstream['git_url'],upstream['revision']],
                work/'buildroot-source.log',env)
            run(['git','-C',source,'checkout','--detach','FETCH_HEAD'],work/'buildroot-source.log',env)
        assert subprocess.check_output(['git','-C',source,'rev-parse','HEAD'],text=True).strip()==upstream['revision']
        assert not subprocess.check_output(['git','-C',source,'status','--porcelain'],text=True).strip()
        sdk = work/'os'
        common = ['make','-C',source,'O='+str(sdk),'BR2_EXTERNAL='+str(ROOT/'br2-external'),
                  'BR2_DL_DIR='+str(cache/'buildroot')]
        if not (sdk/'.config').exists():
            run(common+['crispawn_community_x86_64_defconfig'],work/'userspace.log',env)
        state['phase']='building-userspace';save()
        run(common+['-j'+str(args.jobs)],work/'userspace.log',env)
        run(common+['legal-info'],work/'legal-info.log',env)
        profile = work/'signed-profile'
        if not profile.exists():
            run(['python3',ROOT/'tools/prepare-debian-secure-boot-profile.py',
                 '--packages',cache/'debian','--output',profile,'--fetch'],work/'signed-profile.log',env)
        state['phase']='composing-iso';save()
        run(['python3',ROOT/'tools/build-crispawn-live.py','--sdk',sdk,'--rootfs',sdk/'images/rootfs.cpio.xz',
             '--toolchain',toolchain,'--target-dir',work/'cargo-target',
             '--secure-boot-profile',profile,'--release-id',args.release_id,'--work-dir',work/'compose'],
             work/'compose.log',env)
        state['phase']='complete'
        state['artifact_report']=str(ROOT/'artifacts/crispawn-live/latest-build.json')
        state['userspace_config_sha256']=digest(sdk/'.config')
        state['private_sdk_used']=False
    except BaseException as error:
        state.update(phase='failed',error=type(error).__name__+': '+str(error))
        raise
    finally: save()
    print('BUILD_REPORT',report,flush=True)

if __name__=='__main__': main()

