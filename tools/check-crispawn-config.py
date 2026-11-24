#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Read-only configuration contract checker and lossless legacy serializer.

Reference implementation for developers; not an ISO writer or release verifier.
Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
"""
import argparse
import base64
import binascii
import ipaddress
import json
from pathlib import Path
import re
import sys
import unicodedata
from urllib.parse import urlsplit

try:
    from jsonschema import Draft202012Validator
    from referencing import Registry, Resource
except ImportError:
    raise SystemExit('CONFIG_VALIDATOR_DEPENDENCY_MISSING: install tools/requirements-config-contract.txt')

ROOT = Path(__file__).resolve().parent.parent
SCHEMAS = ROOT / 'config/crispawn/schemas'
KINDS = {
    'crispawn.builder.project': ('builder-project-v1.schema.json', 1, 1024**2),
    'crispawn.live': ('live-v2.schema.json', 2, 65536),
    'crispawn.credentials': ('credentials-v1.schema.json', 1, 65536),
    'crispawn.release_capabilities': ('release-capabilities-v1.schema.json', 1, 65536),
}


class ContractError(ValueError):
    def __init__(self, code, path=''):
        self.code, self.path = code, path
        super().__init__(code)


def require(condition, code, path=''):
    if not condition:
        raise ContractError(code, path)


def _pairs(items):
    result = {}
    for key, value in items:
        require(key not in result, 'DUPLICATE_KEY')
        result[key] = value
    return result


def _float(_):
    raise ContractError('FLOAT_NOT_ALLOWED')


def _scan(value, depth=0, count=None):
    if count is None:
        count = [0]
    count[0] += 1
    require(depth <= 32 and count[0] <= 32768, 'DOCUMENT_COMPLEXITY_LIMIT')
    require(not isinstance(value, float), 'FLOAT_NOT_ALLOWED')
    if isinstance(value, str):
        try:
            value.encode('utf-8', errors='strict')
        except UnicodeError:
            raise ContractError('INVALID_UTF8') from None
        require('\0' not in value, 'NUL_NOT_ALLOWED')
    elif isinstance(value, dict):
        for key, child in value.items():
            _scan(key, depth + 1, count)
            _scan(child, depth + 1, count)
    elif isinstance(value, list):
        for child in value:
            _scan(child, depth + 1, count)


def read(data):
    require(1 <= len(data) <= 1024**2, 'DOCUMENT_SIZE_LIMIT')
    require(not data.startswith(b'\xef\xbb\xbf'), 'BOM_NOT_ALLOWED')
    try:
        result = json.loads(data.decode('utf-8'), object_pairs_hook=_pairs,
                            parse_float=_float, parse_constant=_float)
    except ContractError:
        raise
    except UnicodeError:
        raise ContractError('INVALID_UTF8') from None
    except (ValueError, RecursionError):
        raise ContractError('INVALID_JSON') from None
    require(isinstance(result, dict), 'SCHEMA_MISMATCH')
    require(isinstance(result.get('kind'), str) and result['kind'] in KINDS, 'UNKNOWN_DOCUMENT_KIND')
    require(len(data) <= KINDS[result['kind']][2], 'DOCUMENT_SIZE_LIMIT')
    validate(result)
    return result


def _registry():
    resources, schemas = [], {}
    for path in SCHEMAS.glob('*.schema.json'):
        schema = json.loads(path.read_text(encoding='utf-8'))
        Draft202012Validator.check_schema(schema)
        resources.append((schema['$id'], Resource.from_contents(schema)))
        schemas[path.name] = schema
    return Registry().with_resources(resources), schemas


REGISTRY, DEFINITIONS = _registry()


def _bytes(value, maximum, path):
    require(0 < len(value.encode('utf-8')) <= maximum, 'STRING_BYTE_LIMIT', path)


def _path(value, maximum, path):
    _bytes(value, maximum, path)
    parts = value.split('/')
    require(all(part and part not in ('.', '..') for part in parts), 'INVALID_PATH', path)
    for part in parts:
        require(not any(unicodedata.category(c) == 'Cc' or c in '\\:<>"|?*' for c in part), 'INVALID_PATH', path)
        require(not part.endswith((' ', '.')) and len(part.encode('utf-8')) <= 255, 'INVALID_PATH', path)
        reserved = part.split('.')[0].upper()
        require(not re.fullmatch(r'CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9]', reserved), 'INVALID_PATH', path)


def _stable_id(value, path):
    _bytes(value, 512, path)
    require(not any(unicodedata.category(c) == 'Cc' for c in value), 'INVALID_SOURCE_ID', path)
    if value.startswith('wwid:'):
        body = value[5:]
        require(bool(body) and body == body.strip(), 'INVALID_SOURCE_ID', path)
    else:
        require(value.startswith('serial:') and ':ns:' in value[7:], 'INVALID_SOURCE_ID', path)
        body, namespace = value[7:].rsplit(':ns:', 1)
        require(bool(body) and body == body.strip() and (namespace == '' or re.fullmatch(r'0|[1-9][0-9]*', namespace)), 'INVALID_SOURCE_ID', path)


def _wifi_profiles(network):
    if network['mode'] == 'wifi':
        return [network['wifi_profile']]
    return network.get('wifi_profiles', [])


def _network(network):
    explicit_wifi = network['mode'] == 'wifi'
    profile_path = '/network/wifi_profile' if explicit_wifi else '/network/wifi_profiles'
    ids = []
    for profile in _wifi_profiles(network):
        ids.append(profile['id'])
        ssid = profile['ssid']
        if ssid['encoding'] == 'utf8':
            encoded = ssid['value'].encode('utf-8')
        else:
            try:
                encoded = base64.b64decode(ssid['value'], validate=True)
            except (ValueError, binascii.Error):
                raise ContractError('INVALID_SSID', profile_path + '/ssid') from None
            require(base64.b64encode(encoded).decode('ascii') == ssid['value'], 'INVALID_SSID', profile_path + '/ssid')
        require(1 <= len(encoded) <= 32 and (not explicit_wifi or b'\0' not in encoded),
                'INVALID_SSID', profile_path + '/ssid')
    require(len(ids) == len(set(ids)), 'DUPLICATE_WIFI_ID', profile_path)
    ipv4 = network.get('ipv4')
    if ipv4 is None or ipv4['method'] != 'static':
        return
    try:
        address = ipaddress.IPv4Address(ipv4['address'])
        netmask = ipaddress.IPv4Address(ipv4['netmask'])
        router = ipaddress.IPv4Address(ipv4['router'])
        subnet = ipaddress.IPv4Network(f'{address}/{netmask}', strict=False)
    except ValueError:
        raise ContractError('INVALID_STATIC_IPV4', '/network/ipv4') from None
    require(1 <= subnet.prefixlen <= 31 and subnet.netmask == netmask
            and address != router and router in subnet
            and all(1 <= ip.packed[0] <= 223 and ip.packed[0] != 127 for ip in (address, router)),
            'INVALID_STATIC_IPV4', '/network/ipv4')
    require(subnet.prefixlen == 31 or {address, router}.isdisjoint({subnet.network_address, subnet.broadcast_address}),
            'INVALID_STATIC_IPV4', '/network/ipv4')


def _runtime(value):
    if value['sources']['mode'] == 'selected_ids':
        for source in value['sources']['ids']:
            _stable_id(source, '/sources/ids')
    destination = value['destination']
    key = 'prefix' if destination['kind'] == 's3' else 'backup_directory'
    _path(destination[key], 512, '/destination/' + key)
    if destination['kind'] == 'smb':
        host = destination['server']
        try:
            ipaddress.ip_address(host)
        except ValueError:
            require(bool(re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]{0,252}', host)), 'INVALID_SERVER', '/destination/server')
        _path(destination['share'], 255, '/destination/share')
        require('/' not in destination['share'], 'INVALID_SHARE', '/destination/share')
    elif destination['kind'] == 's3':
        try:
            endpoint = urlsplit(destination['endpoint'])
            valid_port = endpoint.port is None or 1 <= endpoint.port <= 65535
        except ValueError:
            raise ContractError('INVALID_ENDPOINT', '/destination/endpoint') from None
        require(endpoint.scheme == 'https' and bool(endpoint.hostname) and endpoint.username is None and endpoint.password is None
                and endpoint.path in ('', '/') and not endpoint.query and not endpoint.fragment and valid_port
                and not any(c.isspace() for c in destination['endpoint']), 'INVALID_ENDPOINT', '/destination/endpoint')
        bucket = destination['bucket']
        require('..' not in bucket and '.-' not in bucket and '-.' not in bucket, 'INVALID_BUCKET', '/destination/bucket')
        try:
            ipaddress.ip_address(bucket)
        except ValueError:
            pass
        else:
            raise ContractError('INVALID_BUCKET', '/destination/bucket')
    _network(value['network'])


def validate(value):
    _scan(value)
    require(isinstance(value, dict) and isinstance(value.get('kind'), str) and value['kind'] in KINDS, 'UNKNOWN_DOCUMENT_KIND')
    name, version, _ = KINDS[value['kind']]
    require(type(value.get('schema_version')) is int and value['schema_version'] == version, 'UNKNOWN_CONFIG_VERSION', '/schema_version')
    error = next(Draft202012Validator(DEFINITIONS[name], registry=REGISTRY).iter_errors(value), None)
    if error is not None:
        path = '/' + '/'.join(str(p).replace('~', '~0').replace('/', '~1') for p in error.absolute_path)
        if value['kind'] == 'crispawn.credentials':
            path = '/credentials/*'
        raise ContractError('SCHEMA_MISMATCH', path)
    if value['kind'] == 'crispawn.live':
        _runtime(value)
    elif value['kind'] == 'crispawn.builder.project':
        validate(value['runtime'])
        if value['target_pc']['mode'] == 'import_profile':
            _path(value['target_pc']['profile_file'], 4096, '/target_pc/profile_file')
        if value['base']['source'] == 'local_bundle':
            _path(value['base']['directory'], 4096, '/base/directory')
        if 'credentials_file' in value['provisioning']:
            _path(value['provisioning']['credentials_file'], 4096, '/provisioning/credentials_file')
        if value['output']['kind'] == 'iso':
            _path(value['output']['file'], 4096, '/output/file')
        elif value['output']['device']['selection'] == 'identity':
            device = value['output']['device']
            require(1 <= int(device['capacity_bytes']) <= 2**64 - 1, 'BYTE_COUNT_RANGE', '/output/device/capacity_bytes')
            for key in ['serial', 'model']:
                require(device[key] == device[key].strip() and not any(unicodedata.category(c) == 'Cc' for c in device[key]), 'INVALID_DEVICE_HINT', '/output/device/' + key)
    elif value['kind'] == 'crispawn.credentials':
        for secret in value['credentials'].values():
            if secret['type'] == 'wifi_passphrase':
                require(8 <= len(secret['passphrase'].encode('utf-8')) <= 63, 'WIFI_PASSPHRASE_BYTES', '/credentials/*')
            else:
                limits = {'username': 256, 'password': 1024, 'access_key_id': 256, 'secret_access_key': 1024, 'session_token': 8192}
                for key, limit in limits.items():
                    if key in secret:
                        _bytes(secret[key], limit, '/credentials/*')
                if 'domain' in secret:
                    require(len(secret['domain'].encode('utf-8')) <= 255, 'STRING_BYTE_LIMIT', '/credentials/*')
    return value


def credential_refs(runtime):
    result = []
    destination = runtime['destination']
    if destination['kind'] == 'smb' and destination['authentication']['mode'] == 'credential_ref':
        result.append((destination['authentication']['credential_ref'], 'smb_password'))
    if destination['kind'] == 's3':
        result.append((destination['credential_ref'], 's3_access_key'))
    for wifi in _wifi_profiles(runtime['network']):
        if wifi['security'] != 'open':
            result.append((wifi['credential_ref'], 'wifi_passphrase'))
    return result


def build_credentials(project, credentials):
    validate(project)
    require(project['kind'] == 'crispawn.builder.project', 'PROJECT_REQUIRED')
    refs = credential_refs(project['runtime'])
    if not refs:
        return {}
    require(project['provisioning']['embed_credentials'] is True, 'CREDENTIALS_EMBED_NOT_APPROVED', '/provisioning/embed_credentials')
    return runtime_credentials(project['runtime'], credentials)


def runtime_credentials(runtime, credentials):
    """Select only referenced secrets for an explicitly requested personalized ISO."""
    validate(runtime)
    refs = credential_refs(runtime)
    if not refs:
        return {}
    require(credentials is not None, 'CREDENTIALS_REQUIRED', '/provisioning/credentials_file')
    validate(credentials)
    require(credentials['kind'] == 'crispawn.credentials', 'CREDENTIALS_REQUIRED')
    selected = {}
    for key, expected_type in refs:
        require(key in credentials['credentials'], 'CREDENTIAL_REF_NOT_FOUND', '/runtime')
        secret = credentials['credentials'][key]
        require(secret['type'] == expected_type, 'CREDENTIAL_TYPE_MISMATCH', '/runtime')
        selected[key] = secret
    network = runtime['network']
    if network['mode'] == 'wifi' and network['wifi_profile']['security'] != 'open':
        profile = network['wifi_profile']
        passphrase = selected[profile['credential_ref']]['passphrase']
        require(not any(c in passphrase for c in '\0\r\n')
                and (profile['security'] != 'wpa2_psk' or all(' ' <= c <= '~' for c in passphrase)),
                'INVALID_WIFI_PASSPHRASE', '/credentials/*')
    return selected


def required_capabilities(runtime):
    result = {'sources.' + runtime['sources']['mode'], 'startup.' + runtime['startup']['mode'],
              'destination.' + runtime['destination']['kind'], 'completion.' + runtime['completion']['action'],
              'ui.' + runtime['ui']['mode'], 'ui.lang.' + runtime['ui']['language'],
              'logging.' + runtime['logging']['policy'], 'processing.auto',
              'processing.profile.' + runtime['processing']['initial_profile']}
    if 'failure' in runtime:
        result.add('failure.' + runtime['failure']['action'])
    if 'verify_after_write' in runtime['processing']:
        result.add('processing.verify_after_write')
    destination = runtime['destination']
    if destination['kind'] == 'usb_disk' and destination['volume']['mode'] == 'uuid':
        result.add('destination.usb_disk.uuid')
    network = runtime['network']
    if network['mode'] == 'auto' or network['mode'] == 'ethernet' and network['ipv4']['method'] == 'dhcp':
        result.add('network.ethernet_dhcp')
    result.update('network.wifi.' + p['security'] for p in _wifi_profiles(network))
    if network['mode'] in ('ethernet', 'wifi'):
        result.add('network.explicit_' + network['mode'])
        if network['ipv4']['method'] == 'static':
            result.add('network.ipv4_static')
    return result


def legacy_v1(runtime):
    validate(runtime)
    require(runtime['kind'] == 'crispawn.live', 'RUNTIME_REQUIRED')
    require('failure' not in runtime, 'NOT_REPRESENTABLE_IN_V1')
    require(runtime['processing'].get('verify_after_write', True), 'NOT_REPRESENTABLE_IN_V1')
    require(runtime['destination']['kind'] == 'usb_disk'
            and runtime['destination']['volume']['mode'] == 'boot_disk_data'
            and runtime['startup']['mode'] == 'auto_copy' and runtime['network']['mode'] == 'none'
            and runtime['ui']['mode'] in ('gui', 'headless'),
            'NOT_REPRESENTABLE_IN_V1')
    return {'schema_version': 1, 'operation': 'backup',
            'source_ids': runtime['sources'].get('ids', []), 'language': runtime['ui']['language'],
            'backup_directory': runtime['destination']['backup_directory'], 'gui': runtime['ui']['mode'] == 'gui',
            'menu_seconds': runtime['startup']['delay_seconds'],
            'completion_seconds': runtime['completion'].get('delay_seconds', 0),
            'poweroff_on_success': runtime['completion']['action'] == 'poweroff',
            'log_policy': runtime['logging']['policy'], 'log_sync_interval_ms': runtime['logging']['sync_interval_ms'],
            'probe_interval_ms': runtime['diagnostics']['probe_interval_ms'],
            'tuning': {'mode': 'auto', 'profile': runtime['processing']['initial_profile'], 'io_policy': runtime['processing']['io_policy']}}


def select_runtime(runtime, capabilities):
    validate(runtime)
    require(runtime['kind'] == 'crispawn.live', 'RUNTIME_REQUIRED')
    validate(capabilities)
    require(capabilities['kind'] == 'crispawn.release_capabilities', 'INVALID_CAPABILITIES')
    require(required_capabilities(runtime) <= set(capabilities['capabilities']), 'UNSUPPORTED_CAPABILITY')
    versions = capabilities['live_config_versions']
    if 2 in versions:
        return runtime
    require(1 in versions, 'UNSUPPORTED_RUNTIME_VERSION')
    return legacy_v1(runtime)


def project_runtime(project, capabilities, credentials=None):
    validate(project)
    require(project['kind'] == 'crispawn.builder.project', 'PROJECT_REQUIRED')
    validate(capabilities)
    require(capabilities['kind'] == 'crispawn.release_capabilities', 'INVALID_CAPABILITIES')
    require(project['base']['architecture'] == capabilities['architecture'], 'ARCHITECTURE_MISMATCH')
    pinned = project['base'].get('release_id')
    require(pinned is None or pinned == capabilities['release_id'], 'RELEASE_ID_MISMATCH', '/base/release_id')
    build_credentials(project, credentials)
    return select_runtime(project['runtime'], capabilities)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('document', type=Path)
    parser.add_argument('--capabilities', type=Path)
    parser.add_argument('--credentials', type=Path)
    parser.add_argument('--emit-runtime', type=Path)
    args = parser.parse_args()
    document = read(args.document.read_bytes())
    result = {'valid': True, 'kind': document['kind'], 'schema_version': document['schema_version'],
              'release_signature_checked': False, 'iso_or_device_operation': False}
    runtime = document.get('runtime') if document['kind'] == 'crispawn.builder.project' else document if document['kind'] == 'crispawn.live' else None
    if args.credentials:
        require(document['kind'] == 'crispawn.builder.project', 'PROJECT_REQUIRED')
        result['credentials_selected'] = len(build_credentials(document, read(args.credentials.read_bytes())))
    if args.capabilities:
        require(runtime is not None, 'RUNTIME_REQUIRED')
        capabilities = read(args.capabilities.read_bytes())
        credentials = read(args.credentials.read_bytes()) if args.credentials else None
        emitted = project_runtime(document, capabilities, credentials) if document['kind'] == 'crispawn.builder.project' else select_runtime(runtime, capabilities)
        result['emitted_schema_version'] = emitted['schema_version']
        if args.emit_runtime:
            if document['kind'] == 'crispawn.builder.project':
                build_credentials(document, read(args.credentials.read_bytes()) if args.credentials else None)
            require(not credential_refs(runtime), 'REFERENCE_TOOL_CANNOT_PROVISION_SECRETS')
            encoded = (json.dumps(emitted, ensure_ascii=False, indent=2) + '\n').encode('utf-8')
            require(len(encoded) <= 65536, 'DOCUMENT_SIZE_LIMIT')
            with args.emit_runtime.open('xb') as out:
                out.write(encoded)
            result['runtime_written'] = True
    else:
        require(args.emit_runtime is None, 'CAPABILITIES_REQUIRED')
    print(json.dumps(result, ensure_ascii=False))


if __name__ == '__main__':
    try:
        main()
    except ContractError as error:
        print(json.dumps({'valid': False, 'code': error.code, 'path': error.path}))
        raise SystemExit(2)
    except FileExistsError:
        print(json.dumps({'valid': False, 'code': 'OUTPUT_EXISTS'}))
        raise SystemExit(2)
    except FileNotFoundError:
        print(json.dumps({'valid': False, 'code': 'FILE_NOT_FOUND'}))
        raise SystemExit(2)
    except PermissionError:
        print(json.dumps({'valid': False, 'code': 'ACCESS_DENIED'}))
        raise SystemExit(2)
    except OSError:
        print(json.dumps({'valid': False, 'code': 'FILE_IO_ERROR'}))
        raise SystemExit(2)
