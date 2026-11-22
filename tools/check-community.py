#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Check the exported source and edition boundary before building or publishing."""
from pathlib import Path
import json
import subprocess

ROOT=Path(__file__).resolve().parents[1]

def main():
    assert 'Apache License' in (ROOT/'LICENSE').read_text()
    assets=ROOT/'examples/perfect-backup/assets/crispawn'
    assert 'All rights reserved' in (assets/'LICENSE').read_text()
    assert 'not licensed' in (assets/'LICENSE').read_text()
    engine=ROOT/'examples/perfect-backup/rust/crates/engine'
    assert not (engine/'src/live_network.rs').exists()
    manifest=(engine/'Cargo.toml').read_text()
    assert 'pbkdf2' not in manifest and 'sha1' not in manifest
    assert 'community edition has no network support' in (engine/'src/network_config.rs').read_text()
    assert 'network-agent' not in (engine/'src/live_cli.rs').read_text()
    config=json.loads((ROOT/'config/crispawn/examples/live-v2-usb.json').read_text())
    assert config['network']=={'mode':'none'} and config['destination']['kind']=='usb_disk'
    forbidden=('private/','apps/','tools/dev-bridge/','tools/hyperv/')
    tracked=subprocess.check_output(['git','-C',ROOT,'ls-files','-z']).decode().split('\0')
    for path in filter(None,tracked):
        assert not path.startswith(forbidden),path
        assert not path.endswith(('.iso','.vhdx','.exe','.dll','.pfx','.key')),path
    print('COMMUNITY_SOURCE_BOUNDARY_OK')

if __name__=='__main__':main()
