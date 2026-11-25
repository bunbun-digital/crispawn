#!/usr/bin/env python3
"""Cross-language contract cases; no ISO, network, VM or device operations.

Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
"""
import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
EXAMPLES = ROOT / 'config/crispawn/examples'
spec = importlib.util.spec_from_file_location('crispawn_contract', ROOT / 'tools/check-crispawn-config.py')
contract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contract)


def example(name='builder-usb.json'):
    return json.loads((EXAMPLES / name).read_text(encoding='utf-8'))


class ConfigContract(unittest.TestCase):
    def test_optional_io_observer_is_typed_and_capability_gated(self):
        value = example()['runtime']
        for enabled in [True, False]:
            value['diagnostics']['io_observer'] = enabled
            contract.validate(value)
            self.assertIn('diagnostics.io_observer', contract.required_capabilities(value))
            with self.assertRaisesRegex(contract.ContractError, 'NOT_REPRESENTABLE_IN_V1'):
                contract.legacy_v1(value)
        for invalid in [None, 0, 'true']:
            value['diagnostics']['io_observer'] = invalid
            self.reject(value)
    def test_copy_write_verification_is_optional_typed_and_capability_gated(self):
        value = example()['runtime']
        self.assertNotIn('verify_after_write', value['processing'])
        for enabled in [True, False]:
            value['processing']['verify_after_write'] = enabled
            contract.validate(value)
            self.assertIn('processing.verify_after_write', contract.required_capabilities(value))
        with self.assertRaisesRegex(contract.ContractError, 'NOT_REPRESENTABLE_IN_V1'):
            contract.legacy_v1(value)
        for invalid in [0, 1, 'false', None]:
            value['processing']['verify_after_write'] = invalid
            self.reject(value)

    def test_failure_action_is_optional_closed_and_requires_its_runtime_capability(self):
        value = example()['runtime']
        contract.validate(value)
        for action in ['stay', 'poweroff']:
            value['failure'] = {'action': action}
            contract.validate(value)
            self.assertIn('failure.' + action, contract.required_capabilities(value))
            with self.assertRaises(contract.ContractError):
                contract.legacy_v1(value)
        for invalid in [None, {'action':'unknown'}, {'action':'stay','delay_seconds':3}]:
            value['failure'] = invalid
            self.reject(value)

    def reject(self, value, code=None):
        with self.assertRaises(contract.ContractError) as caught:
            contract.validate(value)
        if code:
            self.assertEqual(caught.exception.code, code)

    def test_every_normative_example(self):
        for path in sorted(EXAMPLES.glob('*.json')):
            if path.name != 'live-v1-usb.generated.json':
                with self.subTest(path=path.name):
                    contract.read(path.read_bytes())

    def test_v1_export_matches_complete_golden(self):
        runtime = example()['runtime']
        self.assertEqual(contract.legacy_v1(runtime), example('live-v1-usb.generated.json'))
        self.assertEqual(contract.select_runtime(runtime, example('capabilities-legacy-v1.fixture.json')), example('live-v1-usb.generated.json'))

    def test_explicit_network_modes_require_runtime_support(self):
        runtime = example('live-v2-wifi-static.json')
        caps = example('capabilities-future-v2.fixture.json')
        self.assertEqual(contract.select_runtime(runtime, caps), runtime)
        self.assertEqual(contract.credential_refs(runtime), [('wifi-demo', 'wifi_passphrase')])
        for capability in ['network.explicit_wifi', 'network.ipv4_static']:
            reduced = copy.deepcopy(caps)
            reduced['capabilities'].remove(capability)
            with self.assertRaisesRegex(contract.ContractError, 'UNSUPPORTED_CAPABILITY'):
                contract.select_runtime(runtime, reduced)
        project = example()
        project['runtime'] = runtime
        project['provisioning'] = {'embed_credentials': True, 'credentials_file': 'private/credentials.json'}
        credentials = example('credentials.example.json')
        self.assertEqual(set(contract.build_credentials(project, credentials)), {'wifi-demo'})
        credentials['credentials']['wifi-demo']['passphrase'] = 'パスフレーズ'
        with self.assertRaisesRegex(contract.ContractError, 'INVALID_WIFI_PASSPHRASE'):
            contract.build_credentials(project, credentials)
        with self.assertRaisesRegex(contract.ContractError, 'NOT_REPRESENTABLE_IN_V1'):
            contract.legacy_v1(runtime)
        runtime['network'] = {'mode': 'ethernet', 'ipv4': {'method': 'dhcp'}}
        self.assertEqual(contract.select_runtime(runtime, caps), runtime)
        self.assertTrue({'network.explicit_ethernet', 'network.ethernet_dhcp'}
                        <= contract.required_capabilities(runtime))
        self.assertEqual(contract.credential_refs(runtime), [])

    def test_static_ipv4_rejects_invalid_mask_and_router(self):
        runtime = example('live-v2-wifi-static.json')
        for field, value in [
            ('netmask', '255.0.255.0'), ('netmask', '0.0.0.255'),
            ('netmask', '255.255.255.255'), ('router', '198.51.100.1'),
            ('router', '192.0.2.255'), ('address', '192.0.2.1'),
        ]:
            invalid = copy.deepcopy(runtime)
            invalid['network']['ipv4'][field] = value
            self.reject(invalid, 'INVALID_STATIC_IPV4')
        runtime['network']['ipv4'] = {
            'method': 'static', 'address': '192.0.2.0', 'netmask': '255.255.255.254', 'router': '192.0.2.1',
        }
        contract.validate(runtime)
        runtime['network']['wifi_profile']['ssid'] = {'encoding': 'base64', 'value': 'AA=='}
        self.reject(runtime, 'INVALID_SSID')

    def test_project_cannot_be_treated_as_runtime(self):
        with self.assertRaises(contract.ContractError) as caught:
            contract.legacy_v1(example())
        self.assertEqual(caught.exception.code, 'RUNTIME_REQUIRED')
        with self.assertRaises(contract.ContractError) as caught:
            contract.select_runtime(example(), example('capabilities-legacy-v1.fixture.json'))
        self.assertEqual(caught.exception.code, 'RUNTIME_REQUIRED')

    def test_v1_export_preserves_english_ids_policy_and_time(self):
        runtime = example()['runtime']
        runtime.update(sources={'mode': 'selected_ids', 'ids': ['wwid:naa.6000000000000001', 'serial:DEMO 02:ns:']},
                       completion={'action': 'stay'}, logging={'policy': 'on_error', 'sync_interval_ms': 1000})
        runtime['ui']['language'] = 'en'
        runtime['startup']['delay_seconds'] = 12
        result = contract.legacy_v1(runtime)
        self.assertEqual(result['source_ids'], runtime['sources']['ids'])
        self.assertEqual((result['language'], result['menu_seconds'], result['completion_seconds']), ('en', 12, 0))
        self.assertFalse(result['poweroff_on_success'])
        self.assertEqual((result['log_policy'], result['log_sync_interval_ms']), ('on_error', 1000))
        self.assertNotIn('source_id', result)

    def test_zero_delay_is_auto_copy_not_menu(self):
        runtime = example()['runtime']
        runtime['startup']['delay_seconds'] = 0
        self.assertEqual(contract.legacy_v1(runtime)['menu_seconds'], 0)
        self.assertEqual(runtime['startup']['mode'], 'auto_copy')

    def test_menu_and_restore_cannot_be_lowered_to_v1(self):
        for mode in ['menu', 'restore_select']:
            runtime = example()['runtime']
            runtime['startup'] = {'mode': mode}
            contract.validate(runtime)
            with self.assertRaises(contract.ContractError) as caught:
                contract.legacy_v1(runtime)
            self.assertEqual(caught.exception.code, 'NOT_REPRESENTABLE_IN_V1')
            with self.assertRaises(contract.ContractError) as caught:
                contract.select_runtime(runtime, example('capabilities-legacy-v1.fixture.json'))
            self.assertEqual(caught.exception.code, 'UNSUPPORTED_CAPABILITY')

    def test_menu_and_restore_have_no_delay_or_write_target(self):
        for startup in [{'mode': 'menu', 'delay_seconds': 0}, {'mode': 'restore_select', 'target_id': 'serial:DEMO:ns:'},
                        {'mode': 'auto_restore', 'delay_seconds': 0}]:
            runtime = example()['runtime']
            runtime['startup'] = startup
            self.reject(runtime, 'SCHEMA_MISMATCH')

    def test_headless_has_no_countdown_or_interactive_mode(self):
        runtime = example()['runtime']
        runtime['ui']['mode'] = 'headless'
        self.reject(runtime)
        runtime['startup']['delay_seconds'] = 0
        contract.validate(runtime)
        for mode in ['menu', 'restore_select']:
            runtime['startup'] = {'mode': mode}
            self.reject(runtime)

    def test_console_can_wait_or_select_restore_but_has_no_auto_countdown(self):
        runtime = example()['runtime']
        runtime['ui']['mode'] = 'console'
        self.reject(runtime)
        runtime['startup']['delay_seconds'] = 0
        contract.validate(runtime)
        with self.assertRaises(contract.ContractError) as caught:
            contract.legacy_v1(runtime)
        self.assertEqual(caught.exception.code, 'NOT_REPRESENTABLE_IN_V1')
        for mode in ['menu', 'restore_select']:
            runtime['startup'] = {'mode': mode}
            self.assertEqual(contract.select_runtime(runtime, example('capabilities-future-v2.fixture.json')), runtime)

    def test_required_and_unknown_fields(self):
        project = example()
        del project['runtime']['logging']
        self.reject(project)
        project = example()
        project['runtime']['startup']['seconds'] = 30
        self.reject(project)
        project = example()
        project['password'] = 'DEMO_VALUE_NOT_A_SECRET'
        self.reject(project)
        project = example()
        project['$schema'] = 'https://untrusted.example/schema'
        self.reject(project)

    def test_unknown_versions_and_wrong_version_types(self):
        for value in [0, 2, None, True, '1']:
            project = example()
            project['schema_version'] = value
            self.reject(project, 'UNKNOWN_CONFIG_VERSION')

    def test_numeric_limits(self):
        for name, value in [('delay_seconds', -1), ('delay_seconds', 301), ('delay_seconds', True)]:
            project = example()
            project['runtime']['startup'][name] = value
            self.reject(project)
        for seconds in [0, 60]:
            project = example()
            project['runtime']['completion']['delay_seconds'] = seconds
            contract.validate(project)
        project['runtime']['completion']['delay_seconds'] = 61
        self.reject(project)

    def test_io_retry_bounds_match_existing_policy(self):
        for field, bad in [('transient_retries', 9), ('retry_delay_ms', 0), ('retry_delay_ms', 2001),
                           ('no_progress_seconds', 0), ('failure_grace_seconds', 121), ('shutdown_grace_seconds', 0)]:
            project = example()
            project['runtime']['processing']['io_policy'][field] = bad
            self.reject(project)

    def test_no_disable_verification_or_whole_disk_hash_setting(self):
        for field in ['verify', 'hash_algorithm', 'whole_disk_hash', 'workers', 'codec']:
            project = example()
            project['runtime']['processing'][field] = False
            self.reject(project)

    def test_unknown_source_order_and_duplicate_ids(self):
        for ids in [[], ['PhysicalDrive0'], ['/dev/sda'], ['C:'], ['wwid:a', 'wwid:a']]:
            project = example()
            project['runtime']['sources'] = {'mode': 'selected_ids', 'ids': ids}
            self.reject(project)

    def test_serial_namespace_and_whitespace(self):
        for item in ['serial:DEMO', 'serial: DEMO:ns:', 'serial:DEMO:ns:01', 'wwid: ']:
            project = example()
            project['runtime']['sources'] = {'mode': 'selected_ids', 'ids': [item]}
            self.reject(project, 'INVALID_SOURCE_ID')
        project['runtime']['sources']['ids'] = ['serial:日本語 DEMO:ns:1']
        contract.validate(project)

    def test_source_limits(self):
        project = example()
        project['runtime']['sources'] = {'mode': 'selected_ids', 'ids': ['wwid:x' + str(i) for i in range(64)]}
        contract.validate(project)
        project['runtime']['sources']['ids'].append('wwid:extra')
        self.reject(project)
        project['runtime']['sources']['ids'] = ['wwid:' + 'あ' * 170]
        self.reject(project, 'STRING_BYTE_LIMIT')

    def test_portable_relative_paths(self):
        for bad in ['../output.iso', '/output.iso', 'C:/output.iso', 'a\\b', 'a//b', 'a/./b', 'a/../b', 'a/b/']:
            project = example('builder-iso.json')
            project['output']['file'] = bad
            self.reject(project)
        for bad in ['a/CON.txt', 'a/NUL', 'a/end.', 'a/end ', 'a/' + 'あ' * 86]:
            project['output']['file'] = bad
            self.reject(project, 'INVALID_PATH')

    def test_usb_hint_is_not_a_disk_number_or_authorization(self):
        project = example()
        project['output']['device'] = {'selection': 'interactive', 'disk_number': 3}
        self.reject(project)
        project['output']['device'] = {'selection': 'interactive', 'erase_confirmed': True}
        self.reject(project)
        project['output']['device'] = {'selection': 'identity', 'serial': 'DEMO-USB', 'model': 'DEMO MODEL', 'capacity_bytes': '1000000000000'}
        contract.validate(project)
        for bad in ['0', '001', '1GB', '1\n', 1000000]:
            project['output']['device']['capacity_bytes'] = bad
            self.reject(project)
        project['output']['device']['capacity_bytes'] = '18446744073709551616'
        self.reject(project, 'BYTE_COUNT_RANGE')

    def test_ntfs_uuid_is_not_windows_volume_guid(self):
        runtime = example()['runtime']
        runtime['destination']['volume'] = {'mode': 'uuid', 'uuid': '47F59EFC06CB39D3'}
        contract.validate(runtime)
        with self.assertRaises(contract.ContractError):
            contract.legacy_v1(runtime)
        for bad in ['{47F59EFC-06CB-39D3-ABCD-000000000000}', '47f59efc06cb39d3', '0000000000000000']:
            runtime['destination']['volume']['uuid'] = bad
            self.reject(runtime)

    def test_no_independent_log_destination(self):
        project = example()
        project['runtime']['logging']['destination'] = 'other-disk'
        self.reject(project)

    def test_network_destination_requires_network(self):
        for file in ['builder-smb-v2.json', 'builder-s3-v2.json']:
            project = example(file)
            project['runtime']['network'] = {'mode': 'none'}
            self.reject(project)

    def test_s3_endpoint_cannot_contain_credentials_or_path(self):
        for endpoint in ['http://s3.example', 'https://user:DEMO_PASSWORD@s3.example', 'https://s3.example/path',
                         'https://s3.example?token=DEMO_TOKEN', 'https://s3.example#fragment', 'https://s3.example:0']:
            project = example('builder-s3-v2.json')
            project['runtime']['destination']['endpoint'] = endpoint
            self.reject(project)

    def test_bucket_invalid_names(self):
        for bucket in ['ab..cd', 'ab.-cd', 'ab-.cd', '127.0.0.1', 'A_BUCKET']:
            project = example('builder-s3-v2.json')
            project['runtime']['destination']['bucket'] = bucket
            self.reject(project)

    def test_ssid_uses_decoded_byte_length(self):
        project = example('builder-smb-v2.json')
        ssid = project['runtime']['network']['wifi_profiles'][0]['ssid']
        ssid['value'] = 'あ' * 10
        contract.validate(project)
        ssid['value'] += 'あ'
        self.reject(project, 'INVALID_SSID')
        project['runtime']['network']['wifi_profiles'][0]['ssid'] = {'encoding': 'base64', 'value': 'AP8='}
        contract.validate(project)
        project['runtime']['network']['wifi_profiles'][0]['ssid']['value'] = 'YQ='
        self.reject(project, 'INVALID_SSID')

    def test_wifi_ids_and_open_auth(self):
        project = example('builder-smb-v2.json')
        profiles = project['runtime']['network']['wifi_profiles']
        profiles.append(copy.deepcopy(profiles[0]))
        self.reject(project, 'DUPLICATE_WIFI_ID')
        profiles.pop()
        profiles[0]['security'] = 'open'
        self.reject(project)
        del profiles[0]['credential_ref']
        contract.validate(project)

    def test_private_passphrase_byte_limit(self):
        credentials = example('credentials.example.json')
        credentials['credentials']['wifi-demo']['passphrase'] = 'あいうえ'
        contract.validate(credentials)
        credentials['credentials']['wifi-demo']['passphrase'] = 'あ' * 22
        self.reject(credentials, 'WIFI_PASSPHRASE_BYTES')

    def test_secret_references_are_typed_and_only_selected_are_returned(self):
        project = example('builder-smb-v2.json')
        credentials = example('credentials.example.json')
        selected = contract.build_credentials(project, credentials)
        self.assertEqual(set(selected), {'wifi-demo', 'smb-demo'})
        self.assertNotIn('s3-demo', selected)
        project['runtime']['destination']['authentication']['credential_ref'] = 'wifi-demo'
        with self.assertRaises(contract.ContractError) as caught:
            contract.build_credentials(project, credentials)
        self.assertEqual(caught.exception.code, 'CREDENTIAL_TYPE_MISMATCH')
        project = example('builder-smb-v2.json')
        project['runtime']['destination']['authentication']['credential_ref'] = 'smb-demo\n'
        self.reject(project, 'SCHEMA_MISMATCH')

    def test_missing_secret_or_permission_blocks_build_not_project_save(self):
        project = example('builder-smb-v2.json')
        project['provisioning']['embed_credentials'] = False
        contract.validate(project)
        with self.assertRaises(contract.ContractError) as caught:
            contract.build_credentials(project, example('credentials.example.json'))
        self.assertEqual(caught.exception.code, 'CREDENTIALS_EMBED_NOT_APPROVED')
        project['provisioning']['embed_credentials'] = True
        with self.assertRaises(contract.ContractError) as caught:
            contract.build_credentials(project, None)
        self.assertEqual(caught.exception.code, 'CREDENTIALS_REQUIRED')
        credentials = example('credentials.example.json')
        del credentials['credentials']['smb-demo']
        with self.assertRaises(contract.ContractError) as caught:
            contract.build_credentials(project, credentials)
        self.assertEqual(caught.exception.code, 'CREDENTIAL_REF_NOT_FOUND')

    def test_v2_selection_preserves_all_fields(self):
        for file in ['builder-usb.json', 'builder-menu-v2.json', 'builder-restore-select-v2.json', 'builder-smb-v2.json', 'builder-s3-v2.json']:
            runtime = example(file)['runtime']
            self.assertEqual(contract.select_runtime(runtime, example('capabilities-future-v2.fixture.json')), runtime)

    def test_missing_capability_or_unknown_version_is_not_fallback(self):
        caps = example('capabilities-future-v2.fixture.json')
        caps['capabilities'].remove('ui.lang.ja')
        with self.assertRaises(contract.ContractError) as caught:
            contract.select_runtime(example()['runtime'], caps)
        self.assertEqual(caught.exception.code, 'UNSUPPORTED_CAPABILITY')
        caps = example('capabilities-future-v2.fixture.json')
        caps['live_config_versions'] = [99]
        with self.assertRaises(contract.ContractError) as caught:
            contract.select_runtime(example()['runtime'], caps)
        self.assertEqual(caught.exception.code, 'UNSUPPORTED_RUNTIME_VERSION')

    def test_pinned_release_must_match_selected_metadata(self):
        project = example()
        capabilities = example('capabilities-legacy-v1.fixture.json')
        project['base']['release_id'] = capabilities['release_id']
        self.assertEqual(contract.project_runtime(project, capabilities), example('live-v1-usb.generated.json'))
        project['base']['release_id'] = 'different-release'
        with self.assertRaises(contract.ContractError) as caught:
            contract.project_runtime(project, capabilities)
        self.assertEqual(caught.exception.code, 'RELEASE_ID_MISMATCH')
        for bad in ['../release', 'release\nheader', 'release\n']:
            project['base']['release_id'] = bad
            self.reject(project, 'SCHEMA_MISMATCH')

    def test_strict_json_tokens(self):
        cases = [(b'{"kind":"crispawn.live","kind":"crispawn.live"}', 'DUPLICATE_KEY'),
                 (b'{"kind":"crispawn.live","schema_version":2.0}', 'FLOAT_NOT_ALLOWED'),
                 (b'{"kind":"crispawn.live","schema_version":2e0}', 'FLOAT_NOT_ALLOWED'),
                 (b'{"kind":"crispawn.live","x":NaN}', 'FLOAT_NOT_ALLOWED'),
                 (b'\xef\xbb\xbf{}', 'BOM_NOT_ALLOWED'), (b'\xff', 'INVALID_UTF8'),
                 (b'{"kind":[],"schema_version":2}', 'UNKNOWN_DOCUMENT_KIND'),
                 (b'{"kind":"crispawn.live","x":"\\ud800"}', 'INVALID_UTF8'),
                 (b'{"kind":"crispawn.live","x":"\\u0000"}', 'NUL_NOT_ALLOWED')]
        for data, code in cases:
            with self.subTest(code=code):
                with self.assertRaises(contract.ContractError) as caught:
                    contract.read(data)
                self.assertEqual(caught.exception.code, code)

    def test_size_and_depth_limits(self):
        raw = (EXAMPLES / 'credentials.example.json').read_bytes()
        with self.assertRaises(contract.ContractError) as caught:
            contract.read(raw + b' ' * (65537 - len(raw)))
        self.assertEqual(caught.exception.code, 'DOCUMENT_SIZE_LIMIT')
        project = example()
        child = project
        for _ in range(40):
            child['nested'] = {}
            child = child['nested']
        self.reject(project, 'DOCUMENT_COMPLEXITY_LIMIT')

    def test_cli_redacts_schema_error_and_preserves_existing_output(self):
        with tempfile.TemporaryDirectory(prefix='crispawn-config-') as directory:
            root = Path(directory)
            path = root / 'private.json'
            credentials = example('credentials.example.json')
            credentials['credentials']['wifi-demo']['passphrase'] = 'PASSWORD_MUST_NOT_APPEAR' * 4
            path.write_text(json.dumps(credentials), encoding='utf-8')
            result = subprocess.run([sys.executable, str(ROOT / 'tools/check-crispawn-config.py'), str(path)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertNotIn('PASSWORD_MUST_NOT_APPEAR', result.stdout + result.stderr)
            self.assertNotIn('Traceback', result.stdout + result.stderr)
            output = root / 'live.json'
            output.write_bytes(b'PRESERVE')
            result = subprocess.run([sys.executable, str(ROOT / 'tools/check-crispawn-config.py'), str(EXAMPLES / 'builder-usb.json'),
                '--capabilities', str(EXAMPLES / 'capabilities-legacy-v1.fixture.json'), '--emit-runtime', str(output)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(output.read_bytes(), b'PRESERVE')
            self.assertEqual(json.loads(result.stdout)['code'], 'OUTPUT_EXISTS')


if __name__ == '__main__':
    unittest.main(verbosity=2)
