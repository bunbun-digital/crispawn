#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Assemble a pinned Debian signed profile without installing host packages.

Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
No package maintainer scripts, host module installation, firmware or key changes.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import lzma
from pathlib import Path, PurePosixPath
import subprocess
import tarfile
from urllib.parse import unquote, urlsplit
from urllib.request import urlopen

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / 'config/secure-boot/debian-amd64.json'
BASE_MODULES = 'efivarfs vfat nls_cp437 nls_iso8859_1 nls_utf8 ntfs3 nvme vmd ahci sd_mod sr_mod isofs xhci_pci ehci_pci ohci_pci uhci_hcd usb_storage uas hv_vmbus hv_storvsc hyperv_keyboard hyperv_drm hid_hyperv hv_utils virtio_blk virtio_scsi evdev hid_generic usbhid bochs'.split()
EFI_FILES = {
    'usr/lib/shim/shimx64.efi.signed': 'shimx64.efi',
    'usr/lib/grub/x86_64-efi-signed/grubx64.efi.signed': 'grubx64.efi',
    'usr/lib/shim/mmx64.efi.signed': 'mmx64.efi',
}


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def package_filename(item):
    url = urlsplit(item['url'])
    assert url.scheme == 'https' and url.hostname in ('deb.debian.org', 'security.debian.org') and url.username is None and url.password is None and not url.query and not url.fragment
    name = unquote(PurePosixPath(url.path).name)
    assert name.endswith('.deb') and '/' not in name and '\\' not in name
    return name


def packages(directory, fetch=False):
    manifest = json.loads(MANIFEST.read_text())
    found = {}
    for role, item in manifest['packages'].items():
        path = directory / package_filename(item)
        if not path.exists() and fetch:
            directory.mkdir(parents=True, exist_ok=True)
            with urlopen(item['url'], timeout=60) as source, path.open('xb') as target:
                remaining = item['bytes']
                while remaining:
                    data = source.read(min(1 << 20, remaining))
                    assert data, 'short package download'
                    target.write(data); remaining -= len(data)
                assert not source.read(1), 'oversized package download'
        assert path.is_file() and path.stat().st_size == item['bytes'], 'missing or wrong-size package: ' + role
        assert digest(path) == item['sha256'], 'package checksum mismatch: ' + role
        actual = subprocess.check_output(['dpkg-deb', '--field', path, 'Package', 'Version', 'Architecture'], text=True)
        fields = dict(line.split(': ', 1) for line in actual.splitlines())
        assert fields == {'Package': item['package'], 'Version': item['version'], 'Architecture': 'amd64'}
        found[role] = path
    return manifest, found


def decode_module(data, name):
    if name.endswith('.xz'):
        return lzma.decompress(data, memlimit=256 << 20)
    if name.endswith('.zst'):
        return subprocess.check_output(['zstd', '-d', '-q', '-c'], input=data)
    return data


def prepare(directory, output, original_initrd=None, fetch=False):
    manifest, inputs = packages(directory, fetch)
    assert not output.exists(), 'new profile directory required'
    output.mkdir(parents=True)
    overlay = output / 'overlay'; overlay.mkdir()
    staging = output / 'dependency-staging'; staging.mkdir()
    version = manifest['kernel_release']
    state = dict(schema_version=1, phase='preparing', distribution='Debian', efi_vendor='debian',
                 kernel_release=version, created_at=datetime.now(timezone.utc).isoformat(),
                 packages=manifest['packages'], files={}, module_sources={}, host_settings_changed=False,
                 secure_boot_tested=False, manifest_sha256=digest(MANIFEST))

    def save(): (output / 'profile.json').write_text(json.dumps(state, indent=2) + '\n')
    def put(name, data):
        path = output / name; path.parent.mkdir(parents=True, exist_ok=True)
        with path.open('xb') as stream: stream.write(data)
        state['files'][name] = hashlib.sha256(data).hexdigest()

    save()
    try:
        for role, package in inputs.items():
            process = subprocess.Popen(['dpkg-deb', '--fsys-tarfile', package], stdout=subprocess.PIPE)
            try:
                with tarfile.open(fileobj=process.stdout, mode='r|') as archive:
                    for member in archive:
                        if not member.isfile(): continue
                        name = member.name.removeprefix('./')
                        assert not PurePosixPath(name).is_absolute() and '..' not in PurePosixPath(name).parts
                        module_prefix = 'usr/lib/modules/' + version + '/'
                        if role == 'kernel' and name.startswith(module_prefix):
                            target = staging / 'lib/modules' / version / name[len(module_prefix):]
                            target.parent.mkdir(parents=True, exist_ok=True)
                            with target.open('xb') as stream: stream.write(archive.extractfile(member).read())
                        elif role == 'kernel' and name in ('boot/vmlinuz-' + version, 'boot/config-' + version):
                            put('kernel' if '/vmlinuz-' in name else 'config', archive.extractfile(member).read())
                        elif name in EFI_FILES:
                            put(EFI_FILES[name], archive.extractfile(member).read())
                        elif name.startswith('usr/share/doc/') and name.endswith(('/copyright', '/changelog.Debian.gz')):
                            put('overlay/usr/share/licenses/debian-signed-boot/' + role + '/' + Path(name).name, archive.extractfile(member).read())
            finally:
                process.stdout.close()
                assert process.wait() == 0, 'package extraction failed'

        modules = staging / 'lib/modules' / version
        assert modules.is_dir() and (output/'kernel').is_file() and (output/'config').is_file()
        subprocess.run(['depmod', '-b', staging, version], capture_output=True, check=True)
        requested = sorted(set(BASE_MODULES))
        wanted = set()
        for name in requested:
            result = subprocess.run(['modprobe', '--show-depends', '--dirname', staging, '--set-version', version, name], capture_output=True, text=True)
            assert result.returncode == 0, 'module unavailable: ' + name + ': ' + result.stderr
            for line in result.stdout.splitlines():
                if line.startswith('insmod '):
                    path = Path(line.split()[1]); relative = path.relative_to(modules)
                    assert path.is_file(); wanted.add(relative.as_posix())
        state['requested_modules'] = requested
        for relative in sorted(wanted):
            source = modules / relative
            data = decode_module(source.read_bytes(), relative)
            name = relative.removesuffix('.xz').removesuffix('.zst')
            target = 'overlay/lib/modules/' + version + '/' + name
            put(target, data)
            path = output / target
            signer = subprocess.check_output(['modinfo', '-F', 'signer', path], text=True).strip()
            key = subprocess.check_output(['modinfo', '-F', 'sig_key', path], text=True).strip()
            vermagic = subprocess.check_output(['modinfo', '-F', 'vermagic', path], text=True).split()[0]
            assert signer == manifest['module_signer'] and key == manifest['module_signature_key'] and vermagic == version, 'module signer/key/ABI mismatch: ' + relative
            state['module_sources'][relative] = dict(compressed_sha256=digest(source), module_sha256=hashlib.sha256(data).hexdigest(), signer=signer, signature_key=key)
        final = overlay / 'lib/modules' / version
        for name in ('modules.builtin', 'modules.builtin.modinfo'):
            put('overlay/lib/modules/' + version + '/' + name, (modules/name).read_bytes())
        put('overlay/lib/modules/' + version + '/modules.order', ''.join(p.relative_to(final).as_posix()+'\n' for p in sorted(final.rglob('*.ko'))).encode())
        subprocess.run(['depmod', '-b', overlay, version], capture_output=True, check=True)
        for path in final.glob('modules.*'): state['files'][path.relative_to(output).as_posix()] = digest(path)
        script = '''#!/bin/sh
# Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
case "$1" in
start)
    modprobe efivarfs || echo 'PB_OPTIONAL_MODULE_UNAVAILABLE efivarfs'
    if [ -d /sys/firmware/efi/efivars ]; then
        mount -t efivarfs -o ro efivarfs /sys/firmware/efi/efivars || exit 1
    fi
    mount -t securityfs -o ro securityfs /sys/kernel/security || exit 1
    for module in MODULES; do
        modprobe "$module" || echo "PB_OPTIONAL_MODULE_UNAVAILABLE $module"
    done
    echo PB_DEBIAN_SIGNED_PROFILE_READY
    ;;
stop) : ;;
*) exit 1 ;;
esac
'''.replace('MODULES', ' '.join(name for name in BASE_MODULES if name != 'efivarfs'))
        put('overlay/etc/init.d/S12crispawn-signed', script.encode())
        for name in ('kernel', 'shimx64.efi', 'grubx64.efi', 'mmx64.efi'):
            result = subprocess.run(['sbverify', '--list', output/name], capture_output=True, text=True)
            assert result.returncode == 0 and 'signature 1' in result.stdout
            state[name+'_signature'] = result.stdout + result.stderr
        assert 'Debian Secure Boot' in state['kernel_signature'] and 'Debian Secure Boot' in state['grubx64.efi_signature']
        assert 'Microsoft' in state['shimx64.efi_signature']
        state.update(phase='complete', module_count=len(wanted))
    except BaseException as error:
        state.update(phase='failed', error=repr(error)); raise
    finally: save()
    return state


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--packages', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--original-initrd', type=Path, help='legacy argument; no preserved initramfs is needed')
    parser.add_argument('--fetch', action='store_true', help='download missing exact hash-pinned packages')
    args = parser.parse_args()
    state = prepare(args.packages.resolve(), args.output.resolve(), args.original_initrd.resolve() if args.original_initrd else None, args.fetch)
    print(json.dumps({key:state[key] for key in ('phase','distribution','kernel_release','module_count')}))


if __name__ == '__main__': main()
