#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Independent v1/v2/v3 reference check for development fixtures; no restore writes.

Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
Uses Python's struct/hashlib/zlib/lzma and the host libzstd, never Rust code.
"""
import argparse
import csv
import ctypes
import ctypes.util
import hashlib
import json
import lzma
import re
from pathlib import Path
import struct
import zlib


def sha(data):
    return hashlib.sha256(data).digest()


def golden():
    ident = bytes(range(16))
    head = struct.pack("<8sII16sQ24x", b"PBKFILE1", 1, 96, ident, 0)
    record = struct.pack("<8sII16sQQQQQIi32s16x", b"PBKREC01", 1, 160, ident,
                         0, 0, 0, 3, 3, 0, 0, sha(b"abc"))
    foot = struct.pack("<8sII16sQQQQ", b"PBKEND01", 1, 96, ident, 0, 1, 3, 259)
    return head + sha(head) + record + sha(record) + b"abc" + foot + sha(foot)


def checked_header(data, magic, length, ident, file_id):
    assert len(data) == length and data[:8] == magic
    assert struct.unpack_from("<II", data, 8) == (1, length)
    assert data[16:32] == ident and struct.unpack_from("<Q", data, 32)[0] == file_id
    assert sha(data[:-32]) == data[-32:]


def decode(codec, payload, length):
    assert 0 < length <= 16 * 1024 * 1024
    if codec == 0:
        raw = payload
    elif codec == 1:
        assert not payload
        raw = bytes(length)
    elif codec == 2:
        lib = ctypes.CDLL(ctypes.util.find_library("zstd"))
        lib.ZSTD_decompress.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p, ctypes.c_size_t]
        lib.ZSTD_decompress.restype = ctypes.c_size_t
        output = ctypes.create_string_buffer(length)
        actual = lib.ZSTD_decompress(output, length, payload, len(payload))
        assert actual == length
        raw = output.raw
    elif codec == 3:
        decoder = lzma.LZMADecompressor(format=lzma.FORMAT_XZ, memlimit=128 * 1024 * 1024)
        raw = decoder.decompress(payload, max_length=length + 1)
        assert decoder.eof and not decoder.unused_data
    elif codec == 4:
        decoder = zlib.decompressobj(31)
        raw = decoder.decompress(payload, length + 1)
        assert decoder.eof and not decoder.unused_data and not decoder.unconsumed_tail
    else:
        raise AssertionError("unknown codec")
    assert len(raw) == length
    return raw


def inspect(root, source):
    commit = json.loads((root / "COMMITTED").read_bytes())
    meta = json.loads((root / "metadata.json").read_bytes())
    plan = json.loads((root / "plan.json").read_bytes())
    if meta["format_version"] in (2, 3):
        return inspect_v2(root, source, commit, meta, plan)
    assert commit["format_version"] == meta["format_version"] == plan["format_version"] == 1
    assert commit["backup_id"] == meta["backup_id"]
    assert set(commit["hashes"]) == {"metadata.json", "plan.json", "files.csv", "ranges.csv", "checksums.sha256"}
    for name, digest in commit["hashes"].items():
        assert hashlib.file_digest((root / name).open("rb"), "sha256").hexdigest() == digest
    ident = bytes.fromhex(meta["backup_id"])
    n, chunk = int(meta["source"]["bytes"]), int(plan["range_bytes"])
    records = {}
    file_count = 0
    with (root / "files.csv").open(newline="", encoding="utf-8") as f:
        for row in csv.DictReader(f):
            file_id = int(row["file_id"])
            assert file_id == file_count
            name = f"data/data-{file_id:06}.pbk"
            assert row["relative_path"] == name
            data = (root / name).read_bytes()  # Reference tool is for small fixtures.
            assert len(data) == int(row["stored_bytes"]) <= int(plan["container_bytes"])
            assert hashlib.sha256(data).hexdigest() == row["stored_sha256"]
            checked_header(data[:96], b"PBKFILE1", 96, ident, file_id)
            assert data[40:64] == bytes(24)
            pos, count, raw_total = 96, 0, 0
            while pos < len(data) - 96:
                b = data[pos:pos + 160]
                checked_header(b, b"PBKREC01", 160, ident, file_id)
                rid, offset, length, stored, codec, level = struct.unpack_from("<QQQQIi", b, 40)
                assert b[112:128] == bytes(16) and rid not in records
                assert offset == rid * chunk and length == min(chunk, n - offset)
                assert stored <= length and pos + 160 + stored <= len(data) - 96
                raw = decode(codec, data[pos + 160:pos + 160 + stored], length)
                assert sha(raw) == b[80:112]
                if source:
                    source.seek(offset)
                    assert source.read(length) == raw
                records[rid] = {"range_id": str(rid), "source_offset_bytes": str(offset),
                                "source_length_bytes": str(length), "raw_sha256": sha(raw).hex(),
                                "file_id": str(file_id), "record_offset_bytes": str(pos),
                                "record_length_bytes": str(160 + stored),
                                "codec": ["raw", "zero", "zstd", "xz", "gzip"][codec],
                                "codec_params_id": f'{["raw", "zero", "zstd", "xz", "gzip"][codec]}-{level}-v1'}
                pos += 160 + stored
                count += 1
                raw_total += length
            foot = data[-96:]
            checked_header(foot, b"PBKEND01", 96, ident, file_id)
            assert struct.unpack_from("<QQQ", foot, 40) == (count, raw_total, pos)
            assert count == int(row["range_count"]) and raw_total == int(row["raw_bytes_total"])
            file_count += 1
    with (root / "ranges.csv").open(newline="", encoding="utf-8") as f:
        rows = list(csv.DictReader(f))
    assert len(rows) == len(records) == (n + chunk - 1) // chunk
    assert rows == [records[i] for i in range(len(records))]
    return {"reference_verified": True, "source_bytes": str(n), "ranges": len(records), "files": file_count}


class SegmentedStream:
    """Independent streaming reader for segmented descriptors.

    Schema 1: one file per write unit (at most 16 MiB). Schema 2: appended
    large parts (at most 1 GiB). Parts are hashed and read in bounded chunks.
    """
    def __init__(self, root, name, backup_id, file_id, stored, descriptor_hash, blake3):
        path = root / name
        assert path.is_file() and not path.is_symlink() and path.stat().st_size <= 8 << 20
        descriptor = path.read_bytes()
        assert blake3(descriptor).hexdigest() == descriptor_hash
        value = json.loads(descriptor)
        assert set(value) == {'kind', 'schema_version', 'backup_id', 'file_id', 'bytes', 'parts'}
        assert value['kind'] == 'crispawn.segmented_file' and value['schema_version'] in (1, 2)
        part_limit = 16 << 20 if value['schema_version'] == 1 else 1 << 30
        assert value['backup_id'] == backup_id and value['file_id'] == file_id and value['bytes'] == stored
        assert len(value['parts']) <= 32768
        position, names = 0, set()
        for part in value['parts']:
            assert set(part) == {'path', 'stream_offset', 'bytes', 'used', 'blake3'}
            assert re.fullmatch(r'segments/' + f'{file_id:08x}' + r'/[0-9a-fA-F]{32}\.bin', part['path'])
            assert part['path'] not in names and part['stream_offset'] == position
            assert 0 < part['used'] <= part['bytes'] <= part_limit
            assert re.fullmatch(r'[0-9a-f]{64}', part['blake3'])
            names.add(part['path']); position += part['used']
        assert position == stored
        self.root, self.parts, self.blake3 = root / 'data', value['parts'], blake3
        self.index, self.file, self.remaining = 0, None, 0

    def __enter__(self):
        return self

    def __exit__(self, *_):
        if self.file:
            self.file.close()
        self.file = None

    def read(self, length):
        assert 0 <= length <= 16 << 20
        result = bytearray()
        while len(result) < length:
            if self.remaining == 0:
                if self.file:
                    self.file.close()
                    self.file = None
                if self.index == len(self.parts):
                    break
                part = self.parts[self.index]; self.index += 1
                path = self.root / part['path']
                assert all(not parent.is_symlink() for parent in [path, path.parent, path.parent.parent])
                assert path.is_file() and path.stat().st_size == part['bytes']
                stream = path.open('rb')
                digest = self.blake3()
                while block := stream.read(16 << 20):
                    digest.update(block)
                assert digest.hexdigest() == part['blake3']
                stream.seek(0)
                self.file, self.remaining = stream, part['used']
            data = self.file.read(min(length - len(result), self.remaining))
            assert data
            result.extend(data); self.remaining -= len(data)
        return bytes(result)


def inspect_v2(root, source, commit, meta, plan):
    # Optional host-side dependency only. The Linux image uses Rust BLAKE3.
    try:
        from blake3 import blake3
    except ImportError as error:
        raise RuntimeError("v2 reference verification requires the Python blake3 package") from error
    version = meta["format_version"]
    assert version in (2, 3) and commit["format_version"] == plan["format_version"] == version
    assert meta["hash_algorithm"] == "blake3" and commit["backup_id"] == meta["backup_id"]
    segmented = plan.get('segmented_storage', False)
    assert type(segmented) is bool
    assert commit['verification_kind'] in ({'compressed_blake3', 'skipped'} if segmented else {'buffered_reopen'})
    assert set(commit["hashes"]) == {"metadata.json", "plan.json", "files.csv", "checksums.sha256"}
    for name, digest in commit["hashes"].items():
        with (root / name).open("rb") as f:
            assert hashlib.file_digest(f, "sha256").hexdigest() == digest
    checks = "".join(f'{commit["hashes"][name]}  {name}\n' for name in sorted(commit["hashes"]) if name != "checksums.sha256")
    assert (root / "checksums.sha256").read_bytes() == checks.encode()
    assert not (root / "ranges.csv").exists()
    ident = bytes.fromhex(meta["backup_id"])
    total, chunk, limit = int(meta["source"]["bytes"]), int(plan["range_bytes"]), int(plan["container_bytes"])
    assert len(ident) == 16 and 512 <= chunk <= 16 * 1024 * 1024 and chunk % 512 == 0
    assert chunk + 352 <= limit <= 2_000_000_000
    columns = ["file_id", "relative_path", "source_offset_bytes", "source_length_bytes", "stored_bytes", "frame_count", "raw_blake3"]

    if version == 3:
        columns.append("compression_profiles")
    if segmented:
        columns.append("storage_descriptor_blake3")
    vocabulary = {"raw:0", "zero:0", "zstd:1", "zstd:3", "zstd:6", "zstd:9", "xz:0", "xz:3", "xz:6", "gzip:1", "gzip:6", "gzip:9"}

    def checked(data, magic, length):
        assert len(data) == length and data[:8] == magic
        assert zlib.crc32(data[:-4]) == struct.unpack_from("<I", data, length - 4)[0]

    covered, file_count, ranges, names, ids = 0, 0, 0, set(), set()
    with (root / "files.csv").open(newline="", encoding="utf-8") as index:
        rows = csv.DictReader(index)
        assert rows.fieldnames == columns
        for row in rows:
            file_id, offset, length, stored, count = [int(row[k]) for k in ["file_id", "source_offset_bytes", "source_length_bytes", "stored_bytes", "frame_count"]]
            name = f"data/data-{file_id:06}.pbk"
            assert 0 <= file_id < 65536 and file_id not in ids and row["relative_path"] == name
            assert offset == covered and offset % chunk == 0 and 0 < length <= total - offset
            assert 208 <= stored <= limit and count == (length + chunk - 1) // chunk
            assert len(row["raw_blake3"]) == 64 and row["raw_blake3"] == bytes.fromhex(row["raw_blake3"]).hex()
            ids.add(file_id); names.add(Path(name).name)
            stream = SegmentedStream(root, name, meta['backup_id'], file_id, stored, row['storage_descriptor_blake3'], blake3) if segmented else (root / name).open('rb')
            with stream as data:
                if not segmented:
                    assert (root / name).stat().st_size == stored
                header = data.read(64); checked(header, b"PBKFILE" + str(version).encode(), 64)
                assert struct.unpack_from("<II16sQQ", header, 8) == (version, 64, ident, file_id, offset)
                assert header[48:60] == bytes(12)
                raw_total, frames, position, digest = 0, 0, 64, blake3()
                profiles = set()
                while position < stored - 112:
                    frame = data.read(32); checked(frame, b"PBKFRM0" + str(version).encode(), 32)
                    codec, level, raw_len, payload_len = struct.unpack_from("<IiQI", frame, 8)
                    assert raw_total < length and raw_len == min(chunk, length - raw_total)
                    assert payload_len <= raw_len and position + 32 + payload_len <= stored - 112
                    profile = f'{ {0:"raw",1:"zero",2:"zstd",3:"xz",4:"gzip"}[codec]}:{level}'
                    profiles.add(profile)
                    if version == 3:
                        assert profile in vocabulary
                    if codec in [0, 1]:
                        assert level == 0 and payload_len == (raw_len if codec == 0 else 0)
                    else:
                        if version == 2:
                            assert codec == {"zstd": 2, "xz": 3, "gzip": 4}[plan["codec"]] and level == plan["compression_level"]
                        assert 0 < payload_len < raw_len
                    payload = data.read(payload_len); assert len(payload) == payload_len
                    raw = decode(codec, payload, raw_len); digest.update(raw)
                    if source:
                        source.seek(offset + raw_total); assert source.read(raw_len) == raw
                    raw_total += raw_len; frames += 1; position += 32 + payload_len
                assert position == stored - 112 and raw_total == length and frames == count
                foot = data.read(112); checked(foot, b"PBKEND0" + str(version).encode(), 112)
                assert struct.unpack_from("<II16sQQQQ", foot, 8) == (version, 112, ident, file_id, offset, length, count)
                assert foot[64:96] == digest.digest() == bytes.fromhex(row["raw_blake3"])
                assert foot[96:108] == bytes(12) and data.read(1) == b""
                if version == 3:
                    assert row["compression_profiles"] == ";".join(sorted(profiles))
            covered += length; file_count += 1; ranges += frames
    assert covered == total and {p.name for p in (root / "data").iterdir()} == names | ({'segments'} if segmented else set())
    assert ranges == (total + chunk - 1) // chunk
    return {"reference_verified": True, "format_version": version, "hash_algorithm": "blake3", "source_bytes": str(total), "ranges": ranges, "files": file_count}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", nargs="?", type=Path)
    parser.add_argument("--source-file", type=Path)
    parser.add_argument("--write-golden", type=Path)
    args = parser.parse_args()
    if args.write_golden:
        args.write_golden.parent.mkdir(parents=True, exist_ok=True)
        with args.write_golden.open("x") as f:
            f.write(golden().hex() + "\n")
        return
    if not args.archive:
        parser.error("archive is required")
    if args.source_file:
        with args.source_file.open("rb") as source:
            result = inspect(args.archive, source)
    else:
        result = inspect(args.archive, None)
    print(json.dumps(result))


if __name__ == "__main__":
    main()
