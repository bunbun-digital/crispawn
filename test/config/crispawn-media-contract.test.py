#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Media contract validation is independent of physical disks and host SDKs."""
import importlib.util
import json
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('usb_media', ROOT/'tools/create-crispawn-usb-image.py')
usb = importlib.util.module_from_spec(spec);spec.loader.exec_module(usb)
EXPECTED = {'kind':'crispawn.media_layout', 'schema_version':2,
            'boot_iso_path':'/crispawn.iso', 'data_volume_label':'CRISPAWN',
            'data_directory':'crispawn'}


class MediaContract(unittest.TestCase):
    def test_complete_v2_contract(self):
        self.assertEqual(usb.require_media_layout(json.dumps(EXPECTED).encode()), EXPECTED)

    def test_every_field_and_exact_key_set_are_required(self):
        for field in EXPECTED:
            with self.subTest(field=field):
                value = dict(EXPECTED);del value[field]
                with self.assertRaises(ValueError):usb.require_media_layout(json.dumps(value).encode())
        value = dict(EXPECTED, extra='not permitted')
        with self.assertRaises(ValueError):usb.require_media_layout(json.dumps(value).encode())

    def test_wrong_version_types_labels_and_hash_named_iso_are_rejected(self):
        for key, value in [('schema_version',1),('schema_version',2.0),('schema_version',True),
                           ('boot_iso_path','/crispawn-0123456789abcdef.iso'),
                           ('boot_iso_path','crispawn.iso'),('data_volume_label','OTHER'),
                           ('data_directory','other')]:
            with self.subTest(key=key,value=value):
                invalid=dict(EXPECTED);invalid[key]=value
                with self.assertRaises(ValueError):usb.require_media_layout(json.dumps(invalid).encode())

    def test_duplicate_and_oversized_documents_are_rejected(self):
        duplicate=json.dumps(EXPECTED)[:-1]+', "boot_iso_path": "/crispawn.iso"}'
        for data in [duplicate.encode(),b'',b' '*4097,b'null',b'[]']:
            with self.subTest(data=data[:50]):
                with self.assertRaises(ValueError):usb.require_media_layout(data)


if __name__=='__main__':unittest.main()
