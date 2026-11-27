#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Host reference-reader checks; only temporary ordinary files."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

try:
    from blake3 import blake3
except ImportError:
    blake3 = None

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('reference', ROOT / 'tools/check-crispawn-format.py')
reference = importlib.util.module_from_spec(spec)
spec.loader.exec_module(reference)


@unittest.skipUnless(blake3 is not None, 'optional host blake3 package is unavailable')
class SegmentedReference(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.directory = self.root / 'data/segments/00000001'
        self.directory.mkdir(parents=True)
        self.parts = []
        position = 0
        for index, (data, used) in enumerate([(b'abcdefTAIL', 6), (b'ghijklmn', 8)]):
            name = f'segments/00000001/{index:032x}.bin'
            (self.root / 'data' / name).write_bytes(data)
            self.parts.append(dict(path=name, stream_offset=position, bytes=len(data), used=used,
                                   blake3=blake3(data).hexdigest()))
            position += used
        self.path = self.root / 'data/data-000001.pbk'

    def reader(self, parts=None, schema=1):
        descriptor = dict(kind='crispawn.segmented_file', schema_version=schema, backup_id='a' * 32,
                          file_id=1, bytes=14, parts=self.parts if parts is None else parts)
        data = json.dumps(descriptor).encode()
        self.path.write_bytes(data)
        return reference.SegmentedStream(self.root, 'data/data-000001.pbk', 'a' * 32, 1, 14,
                                         blake3(data).hexdigest(), blake3)

    def test_cross_part_reads_and_unused_tail(self):
        with self.reader() as stream:
            self.assertEqual(stream.read(3), b'abc')
            self.assertEqual(stream.read(7), b'defghij')
            self.assertEqual(stream.read(100), b'klmn')
            self.assertEqual(stream.read(1), b'')

    def test_corruption_in_unused_bytes_is_also_rejected(self):
        stream = self.reader()
        (self.root / 'data' / self.parts[0]['path']).write_bytes(b'abcdefFAIL')
        with self.assertRaises(AssertionError):
            stream.read(1)

    def test_schema_two_accepts_parts_above_the_schema_one_limit(self):
        big = dict(self.parts[1], bytes=(16 << 20) + 1)
        with self.assertRaises(AssertionError):
            self.reader([self.parts[0], big])
        large = self.root / 'data/segments/00000001/' / ('f' * 32 + '.bin')
        payload = bytes(range(256)) * ((16 << 20) // 256 + 1)
        large.write_bytes(payload)
        part = dict(path='segments/00000001/' + large.name, stream_offset=0, bytes=len(payload),
                    used=14, blake3=blake3(payload).hexdigest())
        with self.assertRaises(AssertionError):
            self.reader([part], schema=1)
        with self.reader([part], schema=2) as stream:
            self.assertEqual(stream.read(14), payload[:14])
        with self.assertRaises(AssertionError):
            self.reader([part], schema=3)

    def test_missing_range_or_parent_escape_is_rejected(self):
        for change in [{'stream_offset': 9}, {'path': '../outside.bin'}]:
            parts = [dict(row) for row in self.parts]
            parts[0].update(change)
            with self.assertRaises(AssertionError):
                self.reader(parts)


if __name__ == '__main__':
    unittest.main(verbosity=2)
