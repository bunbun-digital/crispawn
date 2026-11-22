#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Compose a storage-enabled perfect backup LiveOS from freshly built public userspace.

Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import lzma
import os
from pathlib import Path
import shutil
import stat
import subprocess
import uuid
_network_spec = importlib.util.spec_from_file_location('crispawn_network_profile', Path(__file__).with_name('crispawn_network_profile.py'))
_network = importlib.util.module_from_spec(_network_spec)
_network_spec.loader.exec_module(_network)
network_overlay, network_asset = _network.overlay, _network.network_asset
# Resolve the companion module relative to this tool, including fixture imports.
_boot_spec = importlib.util.spec_from_file_location("crispawn_boot", Path(__file__).with_name("crispawn_boot.py"))
_boot = importlib.util.module_from_spec(_boot_spec)
_boot_spec.loader.exec_module(_boot)
KERNEL_DISPLAY_ARGS = _boot.KERNEL_DISPLAY_ARGS
SPLASH_MODES = _boot.SPLASH_MODES
grub_splash = _boot.grub_splash

ROOT = Path(__file__).resolve().parents[1]
PROJECT = ROOT / "examples/perfect-backup/rust"

MEDIA_LAYOUT = {"kind": "crispawn.media_layout", "schema_version": 1,
                "data_volume_label": "CRISPAWN", "data_directory": "crispawn"}


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sdk", type=Path, required=True)
    parser.add_argument("--rootfs", type=Path, required=True)
    parser.add_argument("--toolchain", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--release-id", default="0.1.0-alpha.1")
    parser.add_argument("--kernel", type=Path, help="legacy custom kernel, requires --unsigned-kernel")
    
    parser.add_argument("--target-dir", type=Path, default=Path("/tmp/.orchestrator-perfect-backup-live-target"))
    security = parser.add_mutually_exclusive_group()
    security.add_argument("--secure-boot-profile", type=Path, help="verified signed profile; defaults to the pinned Debian profile")
    security.add_argument("--unsigned-kernel", action="store_true", help="explicit legacy unsigned-kernel experiment; never the default")
    parser.add_argument("--network-drivers", choices=['none'], default='none',
                        help="standard ISO has no network hardware drivers; full explicitly enables network modules")
    parser.add_argument("--credentials", type=Path, help="explicitly embed only referenced private Wi-Fi credentials")
    parser.add_argument("--config", type=Path, default=ROOT / "config/crispawn/examples/live-v2-usb.json",
                        help="JSON settings embedded as /etc/perfect-backup/live.json")
    args = parser.parse_args()
    if args.kernel and not args.unsigned_kernel:
        parser.error("--kernel requires --unsigned-kernel")
    if args.unsigned_kernel and args.network_drivers == 'none':
        parser.error('driver-free standard ISO requires the modular signed kernel')
    sdk = args.sdk.resolve(strict=True)
    embedded_config = args.config.resolve(strict=True)
    config_bytes = embedded_config.read_bytes()
    assert len(config_bytes) <= 65536 and isinstance(json.loads(config_bytes), dict), "invalid embedded config"
    config_hash = digest(embedded_config)
    config_spec = importlib.util.spec_from_file_location('backup_config', ROOT / 'tools/configure-backup.py')
    config_tool = importlib.util.module_from_spec(config_spec); config_spec.loader.exec_module(config_tool)
    config_tool.validate_config(config_bytes)
    if args.network_drivers == 'none' and json.loads(config_bytes).get('network', {'mode': 'none'})['mode'] != 'none':
        parser.error('network settings require a driver-enabled variant; the standard ISO uses network.mode=none')
    credential_bytes = config_tool.read_private_credentials(args.credentials) if args.credentials else None
    credential_bytes = config_tool.selected_credentials(config_bytes, credential_bytes)
    selected_profile = args.secure_boot_profile or ROOT / _boot.DEFAULT_SECURE_PROFILE_NAME
    profile = None if args.unsigned_kernel else selected_profile.resolve(strict=True)
    profile_state = json.loads((profile / "profile.json").read_text()) if profile else None
    if profile:
        assert profile_state["phase"] == "complete"
        _boot.efi_vendor(profile_state)
        assert all(digest(profile / name) == h for name, h in profile_state["files"].items()), "signed profile changed"
    kernel = profile / "kernel" if profile else (args.kernel or ROOT / "build-perfect-backup-kernel/output/arch/x86/boot/bzImage").resolve(strict=True)
    kernel_config = profile / "config" if profile else kernel.parents[3] / ".config"
    configuration = kernel_config.read_text()
    for symbol in ["NTFS3_FS", "USB_STORAGE", "USB_UAS", "BLK_DEV_NVME", "VMD", "VT", "NLS_UTF8"]:
        allowed = ["y", "m"] if profile else ["y"]
        assert any("CONFIG_" + symbol + "=" + value in configuration for value in allowed), "storage kernel missing " + symbol
    rootfs = args.rootfs.resolve(strict=True)
    rootfs_hash = digest(rootfs)
    identifier = datetime.now(timezone.utc).strftime("live-%Y%m%dT%H%M%SZ-") + uuid.uuid4().hex[:8]
    state_root = ROOT / ".orchestrator-perfect-backup-live/builds" / identifier
    state_root.mkdir(parents=True)
    work = args.work_dir / identifier
    work.mkdir(mode=0o700, parents=True, exist_ok=False)
    target = args.target_dir.resolve()
    toolchain = args.toolchain.resolve(strict=True)
    sysroot = sdk / "host/x86_64-buildroot-linux-musl/sysroot"
    cc = next(p for p in [sdk/"host/bin/x86_64-linux-gcc", sdk/"host/bin/x86_64-buildroot-linux-musl-gcc"] if p.is_file())
    ar = cc.with_name(cc.name.removesuffix("gcc")+"ar")
    paths = [ROOT / "tools/build-backup-live.py", ROOT / 'tools/crispawn_network_profile.py', ROOT / 'tools/configure-backup.py', ROOT / "tools/initramfs.py", ROOT / "tools/crispawn_boot.py", ROOT / "tools/prepare-crispawn-asset.py", PROJECT / "Cargo.lock", PROJECT / "Cargo.toml"]
    if profile:
        paths.append(ROOT / "tools/prepare-debian-secure-boot-profile.py")
        if profile_state["distribution"] == "Debian":
            paths += [ROOT / "tools/prepare-debian-secure-boot-profile.py", ROOT / "config/secure-boot/debian-amd64.json"]
    for directory in [PROJECT / "crates", ROOT / "sdk/rust/bunbun-ui", ROOT / "br2-external/package/bunbun-ui-runtime/src",
                      ROOT / "examples/perfect-backup/br2-external", ROOT / "examples/perfect-backup/configs", ROOT / "examples/perfect-backup/assets"]:
        paths += [p for p in directory.rglob("*") if p.is_file()]
    paths += [p for p in (ROOT/'LICENSES').rglob('*') if p.is_file()]
    fingerprints = {str(p.relative_to(ROOT)): digest(p) for p in sorted(set(paths))}
    state = {"phase": "building", "id": identifier, "work": str(work), "inputs": fingerprints,
             "embedded_config_sha256": config_hash,
             "rootfs_sha256": rootfs_hash, "kernel_sha256": digest(kernel), "kernel_config_sha256": digest(kernel_config),
             "qemu_tested": False, "hyperv_tested": False}
    state['network_drivers'] = args.network_drivers
    state['credentials_embedded'] = credential_bytes is not None
    if profile:
        state["signed_profile"] = {"directory": str(profile), "manifest_sha256": digest(profile / "profile.json"),
                                   "kernel_release": profile_state["kernel_release"], "distribution": profile_state["distribution"],
                                   "efi_vendor": _boot.efi_vendor(profile_state), "module_count": profile_state["module_count"]}
        state["boot_mode"] = "signed-optical-and-external-ESP-loopback"

    def save():
        (state_root / "state.json").write_text(json.dumps(state, indent=2) + "\n")

    env = os.environ.copy()
    env.update(PATH=str(toolchain / "bin") + os.pathsep + env["PATH"],
               CARGO_HOME=os.environ["CARGO_HOME"], CARGO_TARGET_DIR=str(target),
               PKG_CONFIG_SYSROOT_DIR=str(sysroot), PKG_CONFIG_LIBDIR=str(sysroot / "usr/lib/pkgconfig"),
               CC_x86_64_unknown_linux_musl=str(cc), AR_x86_64_unknown_linux_musl=str(ar),
               CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=str(cc),
               CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-feature=-crt-static -C link-arg=-Wl,-rpath-link," + str(sysroot / "usr/lib"))

    def run(name, argv, cwd=ROOT):
        with (state_root / (name + ".log")).open("w") as log:
            result = subprocess.run([str(x) for x in argv], env=env, cwd=cwd, stdout=log, stderr=subprocess.STDOUT)
        assert result.returncode == 0, name + " failed: " + str(state_root / (name + ".log"))

    save()
    try:
        print("PHASE LIVE_RUNTIME", flush=True)
        runtime = work / "runtime"
        shutil.copytree(ROOT / "br2-external/package/bunbun-ui-runtime/src", runtime)
        run("runtime", ["make", "CC=" + str(cc), "PKG_CONFIG=" + str(sdk / "host/bin/pkg-config"),
                        "LDFLAGS=-Wl,-rpath-link," + str(sysroot / "usr/lib")], runtime)
        env["BUNBUN_UI_LIB_DIR"] = str(runtime)
        # Linker needs the unversioned name; retain the versioned runtime in the ISO.
        (runtime / "libbunbun-ui.so").symlink_to("libbunbun-ui.so.1")
        print("PHASE LIVE_RUST", flush=True)
        cargo = toolchain / "bin/cargo"
        run("cargo", [cargo, "build", "--offline", "--locked", "--release", "--manifest-path", PROJECT / "Cargo.toml",
                      "--target", "x86_64-unknown-linux-musl", "-p", "perfect-backup-engine", "-p", "perfect-backup-ui"])
        binaries = {name: target / "x86_64-unknown-linux-musl/release" / name for name in
                    ["perfect-backup", "perfect-backup-engine", "perfect-backup-live", "perfect-backup-ui"]}
        state["binaries"] = {name: {"bytes": p.stat().st_size, "sha256": digest(p)} for name, p in binaries.items()}
        loader = [sdk / "target/lib/libc.so", "--library-path", str(runtime) + ":" + str(sdk / "target/usr/lib") + ":" + str(sdk / "target/lib")]
        run("validate-config", loader + [binaries["perfect-backup-live"], "validate-config", embedded_config])
        if credential_bytes is not None:
            private = work / 'credentials.json'; private.write_bytes(credential_bytes); private.chmod(0o600)
            run('validate-network-credentials', loader + [binaries['perfect-backup-live'], 'validate-network-credentials', embedded_config, private])
        run("runtime-capabilities", loader + [binaries["perfect-backup-live"], "runtime-capabilities"])
        capabilities = json.loads((state_root / "runtime-capabilities.log").read_bytes())
        if args.network_drivers == 'none':
            capabilities['capabilities'] = [c for c in capabilities['capabilities'] if not c.startswith('network.')]
        capabilities['network_driver_profile'] = args.network_drivers
        runtime_capabilities = (json.dumps(capabilities, indent=2) + '\n').encode()
        state["runtime_capabilities"] = capabilities
        ui_assets = work / "ui-assets"
        run("ui-assets", ["python3", ROOT / "tools/prepare-crispawn-asset.py", "--input", ROOT / "examples/perfect-backup/assets/crispawn/ui", "--output", ui_assets])
        env["CRISPAWN_UI_ASSET_DIR"] = str(ui_assets)
        run("ui-render", loader + [binaries["perfect-backup-ui"], "--render", sdk / "target/usr/share/fonts/bunbun/noto-sans-jp.otf", state_root / "ui-menu.ppm"])
        # The same LVGL scene produces GRUB canvases and the Linux splash.
        # PNG encoding is a host build step; no additional guest image library.
        from PIL import Image
        splash_maps = []
        logo_rgba = work / "logo.rgba"
        with Image.open(ROOT / "examples/perfect-backup/assets/crispawn/logo.png") as original:
            logo = original.convert("RGBA")
            logo_rgba.write_bytes(b"CRISRGBA" + logo.width.to_bytes(4,"little") + logo.height.to_bytes(4,"little") + logo.tobytes())
        for mode in SPLASH_MODES:
            width, height = mode.split("x")
            ppm = state_root / ("splash-" + mode + ".ppm")
            png = state_root / ("splash-" + mode + ".png")
            run("splash-" + mode, loader + [binaries["perfect-backup-ui"], "--render-splash",
                sdk / "target/usr/share/fonts/bunbun/noto-sans-jp.otf", logo_rgba, width, height, ppm])
            with Image.open(ppm) as canvas:
                canvas.save(png, optimize=True)
            splash_maps += ["-map", png, "/boot/splash-" + mode + ".png"]
        print("PHASE LIVE_INITRAMFS", flush=True)
        spec = importlib.util.spec_from_file_location("wifi_iso", ROOT / "tools/initramfs.py")
        iso = importlib.util.module_from_spec(spec); spec.loader.exec_module(iso)
        xorriso = sdk / "host/bin/xorriso"
        entries = iso.unpack_cpio(lzma.decompress(rootfs.read_bytes()))
        entries = [e for e in entries if not (e[0].startswith("etc/init.d/S50") and any(x in e[0] for x in ["backup-mock", "backup-demo", "bunbun-ui", "perfect-backup"]))]
        if profile:
            entries = [e for e in entries if not e[0].startswith(("lib/modules/", "usr/lib/modules/"))]
        if args.network_drivers == 'none':
            entries = [e for e in entries if not network_asset(e[0])]
        entries = [e for e in entries if e[0] not in {
            'etc/init.d/S40network','etc/init.d/S41bunbun-wifi',
            'etc/network/interfaces','etc/crispawn/credentials.json'}]
        index = {e[0]: e for e in entries}
        assert "etc/bunbun/wifi.conf" not in index, "credential-bearing base ISO refused"
        inode = max(e[1][0] for e in entries) + 1

        def put(name, data, mode=0o644):
            nonlocal inode
            parent = Path(name).parent.as_posix()
            if parent != "." and parent not in index:
                put(parent, b"", stat.S_IFDIR | 0o755)
            if name in index:
                entry = index[name]
                assert stat.S_ISREG(entry[1][1]), name
                entry[2] = data; entry[1][6] = len(data); entry[1][1] = stat.S_IFREG | mode
            else:
                fields = [inode, mode if stat.S_IFMT(mode) else stat.S_IFREG | mode, 0, 0, 1, 0, len(data), 0, 0, 0, 0, 0, 0]
                inode += 1
                entry = [name, fields, data]; entries.append(entry); index[name] = entry

        if profile:
            selected_overlay, network_state = network_overlay(profile, work / 'base-overlay', args.network_drivers == 'full')
            state['network_profile'] = network_state
            for path in sorted(selected_overlay.rglob("*")):
                if path.is_file():
                    name = path.relative_to(selected_overlay).as_posix()
                    put(name, path.read_bytes(), 0o755 if name.startswith("etc/init.d/") else 0o644)
            put("usr/share/perfect-backup/signed-profile.json", (profile / "profile.json").read_bytes())
        for name, path in binaries.items():
            put("usr/bin/" + name, path.read_bytes(), 0o755)
        put("usr/lib/libbunbun-ui.so.1", (runtime / "libbunbun-ui.so.1").read_bytes(), 0o755)
        put("etc/perfect-backup/live-root", b"perfect-backup-liveos-v1\n")
        put("etc/perfect-backup/live.json", config_bytes, 0o600)
        if credential_bytes is not None:
            put('etc/crispawn/credentials.json', credential_bytes, 0o600)
        put("usr/share/crispawn/runtime-capabilities.json", runtime_capabilities)
        put('usr/share/crispawn/network-profile.json', (json.dumps(state.get('network_profile', {'profile': args.network_drivers}))+'\n').encode())
        assert not any(name.startswith('usr/libexec/crispawn/legacy-') for name in index)
        release_capabilities = {'kind':'crispawn.release_capabilities','schema_version':1,
            'release_id':args.release_id,'architecture':'x86_64',
            'kernel_build':profile_state['kernel_release'],
            'live_config_versions':capabilities['live_config_versions'],
            'capabilities':capabilities['capabilities']}
        put('etc/perfect-backup/release-capabilities.json', (json.dumps(release_capabilities,indent=2)+'\n').encode())
        state['release_capabilities'] = release_capabilities
        put("etc/init.d/S50perfect-backup-live", (ROOT / "examples/perfect-backup/br2-external/package/perfect-backup-demo/S50perfect-backup-live").read_bytes(), 0o755)
        put("etc/init.d/S13crispawn-splash", (ROOT / "examples/perfect-backup/br2-external/package/perfect-backup-demo/S13crispawn-splash").read_bytes(), 0o755)
        put("usr/share/crispawn/logo.rgba", logo_rgba.read_bytes())
        for asset in sorted(ui_assets.glob("*.rgba")):
            put("usr/share/crispawn/ui/" + asset.name, asset.read_bytes())
        put("usr/share/licenses/crispawn-logo/LICENSE", (ROOT / "examples/perfect-backup/assets/crispawn/LICENSE").read_bytes())
        inittab = index["etc/inittab"][2].decode()
        inittab = "\n".join(l for l in inittab.splitlines() if "swapon -a" not in l and not l.startswith("ttyS0::")) + "\n"
        put("etc/inittab", inittab.encode())
        put("usr/share/licenses/perfect-backup/LICENSE", (PROJECT / "LICENSE").read_bytes())
        put('usr/share/licenses/crispawn/LICENSE', (ROOT/'LICENSE').read_bytes())
        put('usr/share/licenses/crispawn/NOTICE', (ROOT/'NOTICE').read_bytes())
        metadata = json.loads(subprocess.check_output([str(cargo), 'metadata', '--offline', '--locked',
            '--format-version', '1', '--filter-platform', 'x86_64-unknown-linux-musl',
            '--manifest-path', str(PROJECT/'Cargo.toml')], env=env))
        for package in metadata['packages']:
            folder = Path(package['manifest_path']).parent
            prefix = 'usr/share/licenses/rust-crates/' + package['name'] + '-' + package['version']
            if package['source'] is None:
                continue
            for notice in sorted(folder.rglob('*')):
                if notice.is_file() and notice.name.upper().startswith(('LICENSE','LICENCE','COPYING','NOTICE','COPYRIGHT')):
                    put(prefix + '/' + notice.relative_to(folder).as_posix(), notice.read_bytes())
        # The external toolchain's runtime libraries have their own notices.
        for notice in sorted((ROOT/'LICENSES').iterdir()):
            if notice.is_file():
                put('usr/share/licenses/runtime-libraries/'+notice.name,notice.read_bytes())
        for source, target in [('GPL-2.0-only.txt','GPL-2'),('GPL-3.0-only.txt','GPL-3'),('LGPL-2.1-only.txt','LGPL-2.1')]:
            put('usr/share/common-licenses/'+target,(ROOT/'LICENSES'/source).read_bytes())
        notices = toolchain/'share/doc/rust'
        for name in ['LICENSE-APACHE', 'LICENSE-MIT', 'COPYRIGHT.html', 'COPYRIGHT-library.html']:
            if (notices/name).is_file():
                put('usr/share/licenses/rust-standard-library/'+name,(notices/name).read_bytes())
        for notice in sorted((notices/'licenses').rglob('*')):
            if notice.is_file():
                put('usr/share/licenses/rust-standard-library/licenses/'+notice.relative_to(notices/'licenses').as_posix(),notice.read_bytes())
        fonts = sdk/'build/bunbun-lvgl-9.6.0/scripts/generators/built_in_font/font_license'
        for font, filename in [('Montserrat','OFL.txt'),('FontAwesome5','LICENSE.txt')]:
            notice = fonts/font/filename
            if not notice.is_file():
                raise ValueError('Bundled LVGL font license is missing: '+font)
            put('usr/share/licenses/lvgl-fonts/'+font+'/'+filename,notice.read_bytes())
        legal = sdk/'legal-info/licenses'
        for notice in sorted(legal.rglob('*')):
            if notice.is_file():
                put('usr/share/licenses/buildroot-packages/'+notice.relative_to(legal).as_posix(),notice.read_bytes())
        updated = work / "initrd"
        updated.write_bytes(lzma.compress(iso.pack_cpio(entries), preset=9, check=lzma.CHECK_CRC32))
        image = work / "image.iso"
        # USB retains its external signed ESP route. Optical media carries its
        # own signed chain and identifies its read-only boot volume explicitly.
        grub = work / "grub.cfg"
        grub.write_text("serial --unit=0 --speed=115200\nterminal_output serial\n" + grub_splash("") +
            "set timeout=0\nmenuentry 'CRISPAWN LiveOS' {\n linux /boot/bzImage " + KERNEL_DISPLAY_ARGS + " net.ifnames=0 rdinit=/init pb.live=1\n initrd /boot/initrd\n}\n")
        boot_arguments = ["-boot_image", "any", "replay"]
        optical_maps = []
        if profile:
            modules = {p.name for p in (selected_overlay / 'lib/modules').rglob('*.ko')}
            assert {'sr_mod.ko', 'cdrom.ko', 'isofs.ko'} <= modules, 'signed profile lacks optical modules; prepare the current profile'
            volume_id = 'CRISPAWN_' + uuid.uuid4().hex[:20].upper()
            grub.write_text(_boot.optical_grub(volume_id))
            bios_grub = work / 'grub-bios.cfg'
            bios_grub.write_text(_boot.optical_bios_grub(volume_id))
            efi_image, optical_state = _boot.optical_efi_image(profile, sdk / 'host/bin', work, grub)
            optical_state['volume_id'] = volume_id
            state['optical_boot'] = optical_state
            optical_maps = ['-map', efi_image, '/boot/fat.efi',
                            '-map', bios_grub, '/boot/grub/grub.cfg',
                            '-map', grub, '/EFI/' + _boot.efi_vendor(profile_state) + '/grub.cfg',
                            '-map', grub, '/EFI/BOOT/grub.cfg', '-volid', volume_id]
            boot_arguments = ['-boot_image', 'any', 'discard']
        media_layout = work / 'crispawn-media.json'
        media_layout.write_text(json.dumps(MEDIA_LAYOUT, indent=2) + '\n', encoding='utf-8')
        tree = work/'iso-tree'
        (tree/'boot/grub').mkdir(parents=True)
        shutil.copyfile(updated, tree/'boot/initrd')
        shutil.copyfile(kernel, tree/'boot/bzImage')
        shutil.copyfile(sdk/'images/grub-eltorito.img', tree/'boot/grub/grub-eltorito.img')
        shutil.copyfile(bios_grub, tree/'boot/grub/grub.cfg')
        shutil.copyfile(efi_image, tree/'boot/fat.efi')
        shutil.copyfile(media_layout, tree/'boot/crispawn-media.json')
        for mode in SPLASH_MODES:
            shutil.copyfile(state_root/('splash-'+mode+'.png'), tree/('boot/splash-'+mode+'.png'))
        for name in ['EFI/BOOT/grub.cfg','EFI/'+_boot.efi_vendor(profile_state)+'/grub.cfg']:
            target=tree/name;target.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(grub,target)
        iso.run_xorriso(str(xorriso), ['-outdev', image, '-map', tree, '/', '-volid', volume_id,
            *(_boot.optical_boot_arguments()), '-commit', '-end'])
        verified_media_layout = work / 'verified-crispawn-media.json'
        iso.extract(str(xorriso), image, '/boot/crispawn-media.json', verified_media_layout)
        assert verified_media_layout.read_bytes() == media_layout.read_bytes(), 'ISO media layout readback mismatch'
        assert json.loads(verified_media_layout.read_text()) == MEDIA_LAYOUT
        state['media_layout'] = {'path': '/boot/crispawn-media.json', 'value': MEDIA_LAYOUT,
                                 'sha256': digest(verified_media_layout), 'readback_verified': True}
        if profile:
            assert set(iso.boot_platforms(str(xorriso), image)) == {'BIOS', 'UEFI'}
            iso.extract(str(xorriso), image, '/boot/fat.efi', work / 'verified-optical-efi.img')
            assert digest(efi_image) == digest(work / 'verified-optical-efi.img')
        assert 0 < image.stat().st_size < 100_000_000, "ISO exceeds 100 MB"
        iso.extract(str(xorriso), image, "/boot/initrd", work / "verified-initrd")
        assert digest(updated) == digest(work / "verified-initrd")
        iso.extract(str(xorriso), image, "/boot/bzImage", work / "verified-kernel")
        assert digest(kernel) == digest(work / "verified-kernel")
        assert digest(rootfs) == rootfs_hash
        assert digest(embedded_config) == config_hash, "embedded settings changed during build"
        assert all(digest(ROOT / name) == value for name, value in fingerprints.items()), "sources changed during build"
        if profile:
            assert digest(profile / "profile.json") == state["signed_profile"]["manifest_sha256"]
            assert all(digest(profile / name) == h for name, h in profile_state["files"].items())
        artifacts = ROOT / "artifacts/perfect-backup-live"; artifacts.mkdir(parents=True, exist_ok=True)
        output = artifacts / ("crispawn-community-" + args.release_id + "-" + identifier + "-x86_64.iso")
        with os.fdopen(os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'wb') as f, image.open("rb") as src:
            shutil.copyfileobj(src, f)
        state.update(phase="complete", iso=str(output), iso_bytes=output.stat().st_size, iso_sha256=digest(output),
                     initrd_sha256=digest(updated), runtime_sha256=digest(runtime / "libbunbun-ui.so.1"))
        output.with_suffix(".iso.json").write_text(json.dumps(state, indent=2) + "\n")
        (artifacts/"latest-build.json").write_text(json.dumps(state, indent=2)+"\n")
        print("ISO_READY " + str(output), flush=True)
    except BaseException as error:
        state.update(phase="failed", error=str(error))
        raise
    finally:
        save()
        print("Report: " + str(state_root / "state.json"), flush=True)


if __name__ == "__main__":
    main()
