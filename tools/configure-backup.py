#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Replace the JSON settings inside a NEW perfect backup ISO; no rebuild or root needed.

Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
"""
import argparse
from datetime import datetime, timezone
import importlib.util
import json
import lzma
import os
from pathlib import Path
import re
import stat
import sys
import tempfile

spec = importlib.util.spec_from_file_location("perfect_backup_iso", Path(__file__).with_name("initramfs.py"))
iso = importlib.util.module_from_spec(spec)
spec.loader.exec_module(iso)
CONFIG = "etc/perfect-backup/live.json"
CAPABILITIES = "usr/share/crispawn/runtime-capabilities.json"
CREDENTIALS = "etc/crispawn/credentials.json"


def selected_credentials(config, credentials=None):
    value = json.loads(config)
    if value.get('schema_version', 1) == 1:
        if credentials is not None:
            raise ValueError('LEGACY_CREDENTIALS_UNSUPPORTED')
        return None
    validator = contract()
    private = validator.read(credentials) if credentials is not None else None
    selected = validator.runtime_credentials(value, private)
    if not selected:
        return None
    return (json.dumps({'kind': 'crispawn.credentials', 'schema_version': 1,
                        'credentials': selected}, ensure_ascii=False) + '\n').encode('utf-8')


def contract():
    """Host-only Schema dependency; never installed in the LiveOS image."""
    module_spec = importlib.util.spec_from_file_location("crispawn_contract", Path(__file__).with_name("check-crispawn-config.py"))
    module = importlib.util.module_from_spec(module_spec)
    try:
        module_spec.loader.exec_module(module)
    except SystemExit:
        raise ValueError("CONFIG_VALIDATOR_DEPENDENCY_MISSING") from None
    return module


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError("duplicate configuration key")
        result[key] = value
    return result


def validate_config(data):
    if not 1 <= len(data) <= 65536:
        raise ValueError("configuration must be 1..65536 bytes")
    value = json.loads(data, object_pairs_hook=unique_object,
                       parse_constant=lambda _: (_ for _ in ()).throw(ValueError("nonfinite number")),
                       parse_float=lambda _: (_ for _ in ()).throw(ValueError("floating point number")))
    if isinstance(value, dict) and "kind" in value:
        if value.get("kind") != "crispawn.live":
            raise ValueError("runtime configuration required")
        contract().read(data)
        return data
    if not isinstance(value, dict) or type(value.get("schema_version", 1)) is not int or value.get("schema_version", 1) != 1:
        raise ValueError("expected configuration schema 1")
    allowed = {"schema_version", "operation", "source_id", "source_ids", "language", "backup_directory", "restore", "tuning", "gui",
               "menu_seconds", "completion_seconds", "poweroff_on_success", "poweroff_on_failure", "log_policy", "log_sync_interval_ms", "probe_interval_ms"}
    if value.keys() - allowed:
        raise ValueError("unknown configuration field")
    if type(value.get('poweroff_on_failure', True)) is not bool:
        raise ValueError('poweroff_on_failure must be boolean')
    if value.get("language", "ja") not in ("ja", "en"):
        raise ValueError("invalid language")
    ids = value.get("source_ids", [])
    if not isinstance(ids, list) or len(ids) > 64 or any(not isinstance(v, str) or not v for v in ids):
        raise ValueError("invalid source_ids")
    if value.get("source_id") is not None and ids:
        raise ValueError("source_id/source_ids are mutually exclusive")
    if value.get("log_policy", "always") not in ("always", "on_error", "never"):
        raise ValueError("invalid log_policy")
    interval = value.get("log_sync_interval_ms", 0)
    if type(interval) is not int or not 0 <= interval <= 60000:
        raise ValueError("log_sync_interval_ms must be 0..60000")
    return data


def validate_image_support(config, entries):
    value = json.loads(config)
    if value.get("schema_version", 1) == 1:
        return
    marker = next((e for e in entries if e[0] == CAPABILITIES), None)
    if not marker or not stat.S_ISREG(marker[1][1]) or marker[1][4] != 1 or len(marker[2]) > 65536:
        raise ValueError("ISO_RUNTIME_V2_UNSUPPORTED")
    capabilities = json.loads(marker[2], object_pairs_hook=unique_object)
    if 2 not in capabilities.get("live_config_versions", []):
        raise ValueError("ISO_RUNTIME_V2_UNSUPPORTED")
    if not contract().required_capabilities(value) <= set(capabilities.get("capabilities", [])):
        raise ValueError("ISO_RUNTIME_CAPABILITY_UNSUPPORTED")


def update_initramfs(compressed, config, credentials=None):
    validate_config(config)
    private = selected_credentials(config, credentials)
    decoder = lzma.LZMADecompressor(format=lzma.FORMAT_XZ, memlimit=256 * 1024**2)
    raw = decoder.decompress(compressed, max_length=iso.MAX_ROOTFS + 1)
    if len(raw) > iso.MAX_ROOTFS or not decoder.eof or decoder.unused_data:
        raise ValueError("unsupported or oversized initramfs")
    entries = iso.unpack_cpio(raw)
    index = {name: (fields, data) for name, fields, data in entries}
    validate_image_support(config, entries)
    marker = index.get("etc/perfect-backup/live-root")
    if not marker or not stat.S_ISREG(marker[0][1]) or marker[1] != b"perfect-backup-liveos-v1\n":
        raise ValueError("not a perfect backup LiveOS")
    for parent in ("etc", "etc/perfect-backup"):
        if parent not in index or not stat.S_ISDIR(index[parent][0][1]):
            raise ValueError("invalid settings directory")
    if CONFIG in index and (not stat.S_ISREG(index[CONFIG][0][1]) or index[CONFIG][0][4] != 1):
        raise ValueError("settings must be a regular unlinked file")
    if CREDENTIALS in index and (not stat.S_ISREG(index[CREDENTIALS][0][1]) or index[CREDENTIALS][0][4] != 1):
        raise ValueError('unsafe credentials entry')
    entries = [entry for entry in entries if entry[0] not in (CONFIG, CREDENTIALS)]
    inode = max(fields[0] for _, fields, _ in entries) + 1
    entries.append([CONFIG, [inode, stat.S_IFREG | 0o600, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0], config])
    if private is not None:
        parent = index.get('etc/crispawn')
        if parent and not stat.S_ISDIR(parent[0][1]):
            raise ValueError('unsafe credentials directory')
        if not parent:
            entries.append(['etc/crispawn', [inode+1, stat.S_IFDIR | 0o700, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0], b''])
        entries.append([CREDENTIALS, [inode+2, stat.S_IFREG | 0o600, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0], private])
    return lzma.compress(iso.pack_cpio(entries), preset=9, check=lzma.CHECK_CRC32)


def boot_entries(binary, image):
    report = iso.run_xorriso(binary, ["-indev", image, "-report_el_torito", "plain"])
    return re.findall(r"El Torito boot img\s*:\s*\d+\s+(BIOS|UEFI)\s+y\s+none\s", report)


def configure(source, output, config, binary, credentials=None):
    validate_config(config)
    source, output = source.resolve(strict=True), output.absolute()
    sidecars = [Path(str(output) + suffix) for suffix in (".sha256", ".json")]
    iso.ensure_absent([output, *sidecars])
    if not source.is_file() or not 0 < source.stat().st_size < iso.LIMIT or not output.parent.is_dir():
        raise ValueError("input ISO or output directory is invalid")
    original = iso.digest(source)
    platforms = boot_entries(binary, source)
    with tempfile.TemporaryDirectory(prefix=".perfect-backup-config-", dir=output.parent) as temporary:
        work = Path(temporary)
        iso.extract(binary, source, "/boot/initrd", work / "initrd")
        updated = work / "updated-initrd"
        updated.write_bytes(update_initramfs((work / "initrd").read_bytes(), config, credentials))
        image = work / "configured.iso"
        iso.run_xorriso(binary, ["-indev", source, "-outdev", image, "-boot_image", "any", "replay",
                                "-overwrite", "on", "-map", updated, "/boot/initrd", "-commit", "-end"])
        image.chmod(0o600)
        if not 0 < image.stat().st_size < iso.LIMIT or boot_entries(binary, image) != platforms:
            raise ValueError("ISO size or boot entries changed unexpectedly")
        iso.extract(binary, image, "/boot/initrd", work / "check-initrd")
        if iso.digest(updated) != iso.digest(work / "check-initrd"):
            raise ValueError("settings readback mismatch")
        for number, name in enumerate(["/boot/bzImage"] + (["/boot/fat.efi"] if "UEFI" in platforms else [])):
            before, after = work / f"before-{number}", work / f"after-{number}"
            iso.extract(binary, source, name, before)
            iso.extract(binary, image, name, after)
            if iso.digest(before) != iso.digest(after):
                raise ValueError("signed boot component changed")
        if iso.digest(source) != original:
            raise ValueError("input ISO changed during configuration")
        import hashlib
        metadata = {"schema_version": 1, "kind": "perfect-backup-configuration",
                    "created_at": datetime.now(timezone.utc).isoformat(), "parent_iso_sha256": original,
                    "iso_sha256": iso.digest(image), "iso_size_bytes": image.stat().st_size,
                    "configuration_sha256": hashlib.sha256(config).hexdigest(), "configuration_path": "/" + CONFIG,
                    "credentials_embedded": selected_credentials(config, credentials) is not None,
                    "boot_entries": platforms, "kernel_preserved": True, "boot_tested": False}
        published = []
        try:
            for path, data in [(sidecars[0], f'{metadata["iso_sha256"]}  {output.name}\n'),
                               (sidecars[1], json.dumps(metadata, indent=2) + "\n")]:
                with path.open("x") as stream:
                    published.append(path)
                    stream.write(data)
                path.chmod(0o600)
            os.link(image, output)
        except OSError:
            for path in published:
                path.unlink()
            raise
        return metadata


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input_iso", type=Path)
    parser.add_argument("output_iso", type=Path)
    parser.add_argument("--config", required=True, type=Path)
    parser.add_argument("--xorriso", default="xorriso")
    parser.add_argument("--credentials", type=Path, help="explicitly embed only referenced credentials; input must be private")
    args = parser.parse_args()
    private = read_private_credentials(args.credentials) if args.credentials else None
    metadata = configure(args.input_iso, args.output_iso, args.config.read_bytes(), args.xorriso, private)
    print(f'ISO_CONFIGURED size_bytes={metadata["iso_size_bytes"]} settings={metadata["configuration_path"]}')


def read_private_credentials(path):
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_uid != os.geteuid() or info.st_mode & 0o077 or not 0 < info.st_size <= 65536:
            raise ValueError('PRIVATE_CREDENTIAL_FILE_REQUIRED')
        value = stream.read(65537)
        after = os.fstat(stream.fileno())
        if len(value) != info.st_size or info.st_mtime_ns != after.st_mtime_ns or info.st_ctime_ns != after.st_ctime_ns:
            raise ValueError('CREDENTIAL_FILE_CHANGED')
        return value


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, lzma.LZMAError, iso.ConfigError):
        print("Backup settings update failed; input ISO and pre-existing outputs were preserved.", file=sys.stderr)
        raise SystemExit(2)
