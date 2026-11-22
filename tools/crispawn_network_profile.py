# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
"""Compose a driver-free standard overlay without modifying the signed profile.

The Debian kernel/shim/GRUB and retained signed modules stay byte-for-byte intact.
This removes modular network hardware drivers, not the kernel's built-in IP stack.
"""
from pathlib import Path
import shutil
import subprocess


def network_module(path):
    return '/kernel/drivers/net/' in '/' + path or '/kernel/net/' in '/' + path


def overlay(profile, output, include_network):
    source = profile / 'overlay'
    modules = [p for p in source.rglob('*.ko*') if p.is_file()]
    removed = [p for p in modules if network_module(p.relative_to(source).as_posix())]
    if include_network:
        return source, {'profile': 'full', 'network_modules': len(removed), 'modules': len(modules)}
    output.mkdir(parents=True, exist_ok=False)
    for path in sorted(source.rglob('*')):
        if not path.is_file():
            continue
        relative = path.relative_to(source)
        if network_module(relative.as_posix()):
            continue
        destination = output / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(path, destination)
    removed_names = {p.name.split('.ko')[0] for p in removed}
    startup = output / 'etc/init.d/S12perfect-backup-signed'
    lines = startup.read_text().splitlines()
    for i, line in enumerate(lines):
        if line.strip().startswith('for module in '):
            names = line.strip().removeprefix('for module in ').removesuffix('; do').split()
            lines[i] = '    for module in ' + ' '.join(n for n in names if n not in removed_names) + '; do'
    startup.write_text('\n'.join(lines) + '\n')
    for directory in (output / 'lib/modules').iterdir():
        if directory.is_dir():
            result = subprocess.run(['depmod', '-b', output, directory.name], capture_output=True)
            if result.returncode:
                raise ValueError('BASE_PROFILE_DEPMOD_FAILED')
    retained = [p for p in output.rglob('*.ko*') if p.is_file()]
    assert not any(network_module(p.relative_to(output).as_posix()) for p in retained)
    return output, {'profile': 'none', 'network_modules': 0, 'modules': len(retained),
                    'removed_network_modules': len(removed)}


def network_asset(name):
    """The preserved base's firmware directory contains only radio firmware."""
    return (name.startswith('lib/firmware/') or name == 'lib/firmware'
            or name == 'usr/share/bunbun/wifi-firmware.json')
