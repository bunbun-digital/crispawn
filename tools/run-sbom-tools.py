#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Run the pinned CycloneDX CLI to validate, merge and export existing BOMs.

Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
SBOM serialization, ISO file hashes and CSV conversion belong to the upstream
tool. This runner does not infer inventory completeness or license compliance.
"""
import argparse
import csv
import hashlib
import json
from pathlib import Path
import shutil
import stat
import subprocess

ROOT = Path(__file__).resolve().parents[1]


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def regular_file(path):
    if not stat.S_ISREG(path.lstat().st_mode):
        raise ValueError('ordinary file required: ' + str(path))
    return path.resolve(strict=True)


def components(document):
    for item in document.get('components', []):
        yield item
        yield from components(item)


def scancode_csv(source, destination):
    """Transport ScanCode's normalized declarations in the CLI's CSV format.

    This does not detect licenses or parse copyright text. ScanCode native
    evidence stays intact. Dependency graphs need their native SBOM generator.
    """
    document = json.loads(source.read_text())
    headers = document.get('headers', [])
    if not headers or headers[0].get('tool_name') != 'scancode-toolkit':
        raise ValueError('ScanCode Toolkit result required')
    if headers[0].get('errors') or any(f.get('scan_errors') for f in document.get('files', [])):
        raise ValueError('ScanCode result contains scan errors')
    if document.get('dependencies'):
        raise ValueError('dependency graphs require a native SBOM; CSV transport is declaration-only')
    packages = document.get('packages')
    if not isinstance(packages, list) or not packages:
        raise ValueError('ScanCode result has no assembled packages')
    rows = []
    for package in packages:
        if not isinstance(package.get('name'), str) or not package['name']:
            raise ValueError('ScanCode package name required')
        if package.get('dependencies'):
            raise ValueError('dependency graphs require a native SBOM; CSV transport is declaration-only')
        expression = package.get('declared_license_expression_spdx') or ''
        if any(marker in expression for marker in (' AND ', ' OR ', ' WITH ', '(', ')')):
            raise ValueError('compound expressions require a native SBOM; the CLI CSV importer accepts license IDs')
        custom = expression.startswith('LicenseRef-')
        rows.append({'Type': 'Library', 'Name': package['name'],
                     'Version': package.get('version') or '',
                     'LicenseExpressions': '' if custom else expression,
                     'LicenseNames': expression if custom else '',
                     'Copyright': package.get('copyright') or '', 'Purl': package.get('purl') or ''})
    with destination.open('x', newline='') as stream:
        writer = csv.DictWriter(stream, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)


def assemble(iso, inputs, cli, output, name, version, scans=()):
    locked = json.loads((ROOT / 'config/sbom-tools.lock.json').read_text())
    cli = regular_file(cli)
    if digest(cli) != locked['tools']['cyclonedx-cli']['binary_sha256']:
        raise ValueError('CycloneDX CLI does not match the tool lock')
    iso = regular_file(iso)
    inputs = [regular_file(p) for p in inputs]
    scans = [regular_file(p) for p in scans]
    if not inputs and not scans:
        raise ValueError('at least one tool-generated input BOM is required')
    source_hash = digest(iso)
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    work = output / 'work'
    work.mkdir(mode=0o700)
    commands = []

    def run(*arguments):
        argv = [str(cli), *map(str, arguments)]
        commands.append(argv)
        with (work / ('command-' + str(len(commands)) + '.log')).open('x') as log:
            subprocess.run(argv, stdout=log, stderr=subprocess.STDOUT,
                           check=True, timeout=120)

    def validate(path):
        document = json.loads(path.read_text())
        if document.get('bomFormat') != 'CycloneDX':
            raise ValueError('CycloneDX input required')
        version = document.get('specVersion')
        if version not in ('1.3', '1.4', '1.5', '1.6', '1.7'):
            raise ValueError('unsupported CycloneDX input version')
        run('validate', '--input-file', path, '--input-version',
            'v' + version.replace('.', '_'), '--fail-on-errors')

    try:
        copied = []
        for index, source in enumerate(inputs):
            destination = work / ('input-' + str(index) + '.cdx.json')
            shutil.copyfile(source, destination)
            validate(destination)
            copied.append(destination)
        for index, source in enumerate(scans):
            preserved = work / ('scancode-' + str(index) + '.json')
            shutil.copyfile(source, preserved)
            table = work / ('scancode-' + str(index) + '.csv')
            scancode_csv(preserved, table)
            destination = work / ('scancode-' + str(index) + '.cdx.json')
            run('convert', '--input-file', table, '--input-format', 'csv',
                '--output-file', destination, '--output-format', 'json', '--output-version', 'v1_6')
            validate(destination)
            copied.append(destination)
        run('merge', '--input-files', *copied, '--output-file', work / 'merged.cdx.json',
            '--output-format', 'json', '--output-version', 'v1_6',
            '--name', name, '--version', version)
        artifact = work / 'artifact'
        artifact.mkdir()
        shutil.copyfile(iso, artifact / iso.name)
        run('add', 'files', '--input-file', work / 'merged.cdx.json',
            '--output-file', work / 'with-iso.cdx.json', '--output-format', 'json',
            '--base-path', artifact, '--include', iso.name)
        bom = output / 'sbom.cdx.json'
        run('convert', '--input-file', work / 'with-iso.cdx.json', '--output-file', bom,
            '--output-format', 'json', '--output-version', 'v1_6')
        validate(bom)
        document = json.loads(bom.read_text())
        assert document['specVersion'] == locked['spec_version']
        assert any(c.get('type') == 'file'
                   and c.get('name') == iso.name
                   and any(h.get('alg') == 'SHA-256' and h.get('content') == source_hash
                           for h in c.get('hashes', []))
                   for c in components(document)), 'ISO hash missing from tool output'
        run('convert', '--input-file', bom, '--output-file', output / 'components.csv',
            '--output-format', 'csv')
        assert digest(iso) == source_hash, 'input ISO changed'
        result = {'iso_sha256': source_hash, 'bom_sha256': digest(bom),
                  'tool_sha256': digest(cli), 'format': locked['format'],
                  'spec_version': document['specVersion'], 'schema_validated': True,
                  'inventory_coverage': 'not_assessed', 'license_compliance': 'not_assessed',
                  'dependency_track_imported': False}
        (output / 'generation-result.json').write_text(json.dumps(result, indent=2) + '\n')
        return result
    finally:
        (work / 'commands.json').write_text(json.dumps(commands, indent=2) + '\n')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--iso', required=True, type=Path)
    parser.add_argument('--input-bom', action='append', default=[], type=Path)
    parser.add_argument('--scancode-packages', action='append', default=[], type=Path,
                        help='reviewed ScanCode package declarations, not arbitrary file-license matches')
    parser.add_argument('--cyclonedx-cli', required=True, type=Path)
    parser.add_argument('--output-dir', required=True, type=Path)
    parser.add_argument('--name', default='CRISPAWN-LiveOS')
    parser.add_argument('--product-version', required=True)
    args = parser.parse_args()
    print(json.dumps(assemble(args.iso, args.input_bom, args.cyclonedx_cli,
                              args.output_dir, args.name, args.product_version,
                              args.scancode_packages)))


if __name__ == '__main__':
    main()
