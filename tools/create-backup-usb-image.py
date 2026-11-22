#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Create a NEW virtual USB disk: GPT + FAT EFI loader + NTFS ISO/data volume.

Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
Only ordinary image files are accepted. Never formats a host block device.
"""
import argparse
import errno
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
# Resolve the companion module relative to this tool, including fixture imports.
_boot_spec = importlib.util.spec_from_file_location("crispawn_boot", Path(__file__).with_name("crispawn_boot.py"))
_boot = importlib.util.module_from_spec(_boot_spec)
_boot_spec.loader.exec_module(_boot)
KERNEL_DISPLAY_ARGS = _boot.KERNEL_DISPLAY_ARGS
grub_splash = _boot.grub_splash

ROOT = Path(__file__).resolve().parents[1]


def sparse_copy(source, target, offset=0, length=None, source_offset=0):
    """Copy content without allocating source holes; target must be a new file."""
    length = source.stat().st_size - source_offset if length is None else length
    with source.open("rb", buffering=0) as src, target.open("r+b", buffering=0) as dst:
        a, b = os.fstat(src.fileno()), os.fstat(dst.fileno())
        assert stat.S_ISREG(a.st_mode) and stat.S_ISREG(b.st_mode), "ordinary image files only"
        assert (a.st_dev, a.st_ino) != (b.st_dev, b.st_ino), "source and target must differ"
        position = source_offset
        end = source_offset + length
        while position < end:
            try:
                data = os.lseek(src.fileno(), position, os.SEEK_DATA)
            except OSError as e:
                if e.errno == errno.ENXIO:
                    break
                raise
            if data >= end:
                break
            hole = min(os.lseek(src.fileno(), data, os.SEEK_HOLE), end)
            while data < hole:
                chunk = os.pread(src.fileno(), min(hole - data, 1024 * 1024), data)
                if not chunk:
                    raise RuntimeError("short sparse source")
                # Some tmpfs files contain allocated zero pages.
                if any(chunk):
                    written = 0
                    while written < len(chunk):
                        n = os.pwrite(dst.fileno(), chunk[written:], offset + data - source_offset + written)
                        if n == 0:
                            raise RuntimeError("short sparse write")
                        written += n
                data += len(chunk)
            position = hole
        dst.flush(); os.fsync(dst.fileno())


def tools():
    root = Path("/")
    env = os.environ.copy()
    return root, env


def create(iso, config, output, work, size_mib=2048, secure_profile=None):
    iso = iso.resolve(strict=True)
    assert iso.is_file() and 0 < iso.stat().st_size < 100_000_000
    if config is not None:
        config = config.resolve(strict=True)
        assert config.is_file() and config.stat().st_size <= 65536
        config_spec = importlib.util.spec_from_file_location("backup_settings", ROOT / "tools/configure-backup.py")
        settings = importlib.util.module_from_spec(config_spec)
        config_spec.loader.exec_module(settings)
        settings.validate_config(config.read_bytes())
        value = json.loads(config.read_bytes())
        if value.get("schema_version", 1) == 2:
            # Check this ISO's executable feature declaration, not a caller's
            # future-release fixture, before putting a v2 override on media.
            with tempfile.TemporaryDirectory(prefix=".crispawn-capabilities-", dir=output.parent) as temporary:
                initrd = Path(temporary) / "initrd"
                settings.iso.extract(str(ROOT / "build/os/host/bin/xorriso"), iso, "/boot/initrd", initrd)
                import lzma
                decoder = lzma.LZMADecompressor(memlimit=256 * 1024**2)
                raw = decoder.decompress(initrd.read_bytes(), max_length=settings.iso.MAX_ROOTFS + 1)
                assert decoder.eof and not decoder.unused_data and len(raw) <= settings.iso.MAX_ROOTFS
                settings.validate_image_support(config.read_bytes(), settings.iso.unpack_cpio(raw))
    assert size_mib >= 512
    assert not output.exists() and not output.is_symlink()
    work.mkdir(exist_ok=False, parents=True)
    profile = None
    if secure_profile:
        secure_profile = secure_profile.resolve(strict=True)
        profile = json.loads((secure_profile / "profile.json").read_text())
        assert profile["phase"] == "complete"
        vendor = _boot.efi_vendor(profile)
        assert all(hashlib.sha256((secure_profile / p).read_bytes()).hexdigest() == h for p, h in profile["files"].items())
    host, env = tools()
    sdk = ROOT / "build/os/host/bin"
    calls = []

    def run(argv, input_bytes=None):
        calls.append([str(x) for x in argv])
        p = subprocess.run([str(x) for x in argv], input=input_bytes, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        with (work / "commands.log").open("ab") as f:
            f.write(json.dumps(calls[-1]).encode() + b"\n" + p.stdout + p.stderr)
        assert p.returncode == 0, (argv[0], p.stderr.decode(errors="replace"))
        return p

    total = size_mib * 1024 * 1024
    esp_offset = 1024 * 1024
    # The signed GRUB loads the ISO from FAT. Reserve room for a 100 MB ISO,
    # vendor loaders and settings while retaining the existing two volumes.
    esp_bytes = 128 * 1024 * 1024
    ntfs_offset = esp_offset + esp_bytes
    ntfs_bytes = total - ntfs_offset - 1024 * 1024
    with output.open("xb") as f:
        assert stat.S_ISREG(os.fstat(f.fileno()).st_mode)
        f.truncate(total)
    run(["sfdisk", str(output)], f"label: gpt\nunit: sectors\nstart={esp_offset//512}, size={esp_bytes//512}, type=U\nstart={ntfs_offset//512}, size={ntfs_bytes//512}, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7\n".encode())
    esp, ntfs = work / "esp.img", work / "ntfs.img"
    for path, size in [(esp, esp_bytes), (ntfs, ntfs_bytes)]:
        with path.open("xb") as f:
            f.truncate(size)
    run([host / "usr/sbin/mkntfs", "-Q", "-F", "-L", "CRISPAWN", "-s", "512", "-c", "4096",
         "-p", str(ntfs_offset // 512), "-H", "255", "-S", "63", ntfs])
    with ntfs.open("rb") as f:
        sector = f.read(512)
    assert sector[3:11] == b"NTFS    "
    volume_uuid = f'{int.from_bytes(sector[72:80], "little"):016X}'
    if not profile:
        run([host / "usr/sbin/ntfscp", ntfs, iso, "/perfect-backup.iso"])
        if config is not None:
            run([host / "usr/sbin/ntfscp", ntfs, config, "/perfect-backup.json"])
    if profile:
        spec = importlib.util.spec_from_file_location("iso_check", ROOT / "tools/initramfs.py")
        iso_check = importlib.util.module_from_spec(spec); spec.loader.exec_module(iso_check)
        iso_check.extract(str(sdk / "xorriso"), iso, "/boot/bzImage", work / "iso-kernel")
        assert hashlib.sha256((work / "iso-kernel").read_bytes()).hexdigest() == profile["files"]["kernel"], "ISO kernel does not match signed profile"
    run([sdk / "mformat", "-i", esp, "-F", "::"])
    boot_uuid = volume_uuid
    if profile:
        # Keep the existing FAT loopback route for the signed vendor GRUB.
        # Keep ISO/config on the boot partition and backup/logs on NTFS.
        boot_sector = esp.read_bytes()[:512]
        assert boot_sector[82:90] == b"FAT32   "
        serial = int.from_bytes(boot_sector[67:71], "little")
        boot_uuid = f"{serial >> 16:04X}-{serial & 65535:04X}"
    grub = work / "grub.cfg"
    grub.write_text("serial --unit=0 --speed=115200\nterminal_output serial\n"
                    f"echo PB_USB_GRUB_START uuid={boot_uuid} iso=perfect-backup.iso\n"
                    f"search --no-floppy --fs-uuid --set=pbroot {boot_uuid}\n"
                    "loopback loop ($pbroot)/perfect-backup.iso\n"
                    + grub_splash("(loop)") +
                    f"linux (loop)/boot/bzImage {KERNEL_DISPLAY_ARGS} net.ifnames=0 rdinit=/init pb.live=1 pb.boot_uuid={boot_uuid}" +
                    (f" pb.data_uuid={volume_uuid} pb.require_secure_boot=1 module.sig_enforce=1" if profile else "") + "\n"
                    "initrd (loop)/boot/initrd\nboot\necho PB_USB_BOOT_FAILED\nsleep 30\n")
    boot = work / "BOOTX64.EFI"
    if profile:
        shutil.copyfile(secure_profile / "shimx64.efi", boot)
    else:
        mkimage = shutil.which("grub-mkimage")
        assert mkimage
        run([mkimage, "-O", "x86_64-efi", "-p", "/boot/grub", "-d", "/usr/lib/grub/x86_64-efi", "-c", grub, "-o", boot,
             "part_gpt", "fat", "ntfs", "ntfscomp", "iso9660", "loopback", "linux", "search", "search_fs_uuid",
             "normal", "serial", "terminal", "echo", "boot"])
    run([sdk / "mmd", "-i", esp, "::/EFI", "::/EFI/BOOT"])
    run([sdk / "mcopy", "-i", esp, boot, "::/EFI/BOOT/BOOTX64.EFI"])
    if profile:
        run([sdk / "mcopy", "-i", esp, iso, "::/perfect-backup.iso"])
        if config is not None:
            run([sdk / "mcopy", "-i", esp, config, "::/perfect-backup.json"])
        run([sdk / "mcopy", "-i", esp, secure_profile / "grubx64.efi", "::/EFI/BOOT/grubx64.efi"])
        run([sdk / "mcopy", "-i", esp, secure_profile / "mmx64.efi", "::/EFI/BOOT/mmx64.efi"])
        run([sdk / "mmd", "-i", esp, "::/EFI/" + vendor, "::/boot", "::/boot/grub"])
        for path in ["::/EFI/BOOT/grub.cfg", "::/EFI/" + vendor + "/grub.cfg", "::/boot/grub/grub.cfg"]:
            run([sdk / "mcopy", "-i", esp, grub, path])
    sparse_copy(esp, output, offset=esp_offset)
    sparse_copy(ntfs, output, offset=ntfs_offset)
    result = {"schema_version": 1, "image": str(output), "bytes": total, "uuid": volume_uuid,
              "data_volume_label": "CRISPAWN", "data_directory": "crispawn",
              "configuration_source": "external-boot-file" if config is not None else "embedded-in-iso",
              "esp_offset": esp_offset, "esp_bytes": esp_bytes, "ntfs_offset": ntfs_offset, "ntfs_bytes": ntfs_bytes,
              "iso_sha256": hashlib.sha256(iso.read_bytes()).hexdigest(),
              "efi_sha256": hashlib.sha256(boot.read_bytes()).hexdigest(), "commands": calls}
    if profile:
        result["secure_boot_required"] = True
        result["boot_uuid"] = boot_uuid
        result["data_uuid"] = volume_uuid
        result["layout"] = "fat32-esp-with-iso-config+ntfs-backup-logs"
        result["signed_profile_sha256"] = hashlib.sha256((secure_profile / "profile.json").read_bytes()).hexdigest()
        result["distribution"] = profile["distribution"]
        result["kernel_release"] = profile["kernel_release"]
        result["efi_vendor"] = vendor
    output.with_suffix(".img.json").write_text(json.dumps(result, indent=2) + "\n")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--iso", type=Path, required=True)
    settings = parser.add_mutually_exclusive_group(required=True)
    settings.add_argument("--config", type=Path, help="external boot-volume JSON overrides the ISO settings")
    settings.add_argument("--use-embedded-config", action="store_true", help="use the ISO settings without an external override")
    parser.add_argument("--output", type=Path, required=True)
    security = parser.add_mutually_exclusive_group()
    security.add_argument("--secure-boot-profile", type=Path, help="verified signed profile; defaults to Debian")
    security.add_argument("--unsigned-boot", action="store_true", help="explicit legacy unsigned-media experiment")
    parser.add_argument("--format", choices=["raw", "vhdx"], default="raw")
    parser.add_argument("--qemu-img", type=Path, help="local qemu-img executable, required for VHDX output")
    parser.add_argument("--size-mib", type=int, default=2048)
    args = parser.parse_args()
    output = args.output.resolve()
    assert output.parent.is_dir()
    with tempfile.TemporaryDirectory(prefix=".orchestrator-usb-build-", dir="/tmp") as temp:
        raw = Path(temp) / "usb.img" if args.format == "vhdx" else output
        profile = None if args.unsigned_boot else args.secure_boot_profile or ROOT / _boot.DEFAULT_SECURE_PROFILE_NAME
        result = create(args.iso, args.config, raw, Path(temp) / "partitions", args.size_mib, profile)
        if args.format == "vhdx":
            assert args.qemu_img and not output.exists() and not output.is_symlink()
            tool = args.qemu_img.resolve(strict=True)
            subprocess.run([tool, "convert", "-f", "raw", "-O", "vhdx", "-o", "subformat=dynamic", raw, output], check=True)
            subprocess.run([tool, "compare", "-f", "raw", "-F", "vhdx", raw, output], check=True)
            result.update(image=str(output), format="vhdx", stored_bytes=output.stat().st_size,
                          image_sha256=hashlib.sha256(output.read_bytes()).hexdigest(), raw_content_compared=True)
            output.with_suffix(".vhdx.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
