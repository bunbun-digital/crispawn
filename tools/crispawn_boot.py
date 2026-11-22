# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
"""Boot defaults and GRUB presentation; preserve each vendor's signed chain."""

DEFAULT_SECURE_PROFILE_NAME = "build/signed-profile"


def efi_vendor(profile):
    # Old Ubuntu profiles predate this explicit field. Never interpolate an
    # arbitrary bundle value into a FAT pathname or a loader configuration.
    distribution = profile.get("distribution", "Ubuntu")
    vendors = {"Debian": "debian", "Ubuntu": "ubuntu"}
    if distribution not in vendors or profile.get("efi_vendor", vendors[distribution]) != vendors[distribution]:
        raise ValueError("unsupported signed-profile EFI vendor")
    return vendors[distribution]

# quiet also silences the EFI stub. ignore_loglevel retains the full diagnostic
# stream on the sole kernel console (UART), including the video timing marker.
KERNEL_DISPLAY_ARGS = "quiet ignore_loglevel console=ttyS0,115200n8 logo.nologo vt.global_cursor_default=0"
SPLASH_MODES = ("1024x768", "800x600", "1280x720", "1920x1080")


def grub_splash(prefix):
    # Use only already-embedded modules in the vendor-signed GRUB. Its shim-lock
    # verifier rejects external FONT files but permits PIXMAP files. gfxterm's
    # built-in null font is enough for an image-only screen (no glyphs drawn).
    # Missing GOP/PNG support must still fall back to serial and boot Linux.
    return (
        "set color_normal=white/white\nset color_highlight=white/white\n"
        "set gfxpayload=keep\n"
        "for pbmode in " + " ".join(SPLASH_MODES) + "; do\n"
        "    set gfxmode=$pbmode\n"
        "    if terminal_output gfxterm serial; then\n"
        f"      background_image --mode normal {prefix}/boot/splash-$pbmode.png\n"
        "      clear\n"
        "      break\n    fi\ndone\n"
    )


def optical_grub(volume_id):
    """The same config works from the optical FAT image and the ISO filesystem."""
    import re
    if not re.fullmatch(r"CRISPAWN_[A-F0-9]{20}", volume_id):
        raise ValueError("invalid optical volume ID")
    return (
        "serial --unit=0 --speed=115200\nterminal_output serial\n"
        # The preserved BIOS GRUB lacks some EFI GRUB search modules. Retain
        # its boot-device root when label search is unavailable; Linux still
        # identifies the optical medium by its exact label before any writes.
        f"if search --no-floppy --label --set=pbcd {volume_id}; then\n"
        " set root=$pbcd\nfi\n"
        "echo CRISPAWN_OPTICAL_GRUB_START\n"
        + grub_splash("") +
        "set timeout=0\nmenuentry 'CRISPAWN LiveOS' {\n"
        f" linux /boot/bzImage {KERNEL_DISPLAY_ARGS} net.ifnames=0 rdinit=/init pb.live=1 "
        f"pb.optical_label={volume_id} module.sig_enforce=1\n"
        " initrd /boot/initrd\n}\n"
    )


def optical_efi_image(profile, sdk_bin, work, config):
    """Create a new FAT image, preserving all vendor-signed EFI bytes."""
    import hashlib
    from pathlib import Path
    import subprocess
    import json

    vendor = efi_vendor(json.loads((profile / "profile.json").read_text()))
    image = work / "optical-efi.img"
    with image.open("xb") as stream:
        stream.truncate(8 * 1024 * 1024)
    calls = []
    def run(name, *arguments):
        argv = [str(sdk_bin / name), *map(str, arguments)]
        calls.append(argv)
        subprocess.run(argv, check=True, capture_output=True, timeout=60)
    run("mformat", "-i", image, "-T", "16384", "::")
    run("mmd", "-i", image, "::/EFI", "::/EFI/BOOT", "::/EFI/" + vendor, "::/boot", "::/boot/grub")
    hashes = {}
    for source, target in [("shimx64.efi", "BOOTX64.EFI"), ("grubx64.efi", "grubx64.efi"), ("mmx64.efi", "mmx64.efi")]:
        destination = "::/EFI/BOOT/" + target
        run("mcopy", "-i", image, profile / source, destination)
        readback = work / ("verified-optical-" + target)
        run("mcopy", "-i", image, destination, readback)
        original = (profile / source).read_bytes()
        assert readback.read_bytes() == original, "signed optical EFI changed"
        hashes["EFI/BOOT/" + target] = hashlib.sha256(original).hexdigest()
    for destination in ["::/EFI/BOOT/grub.cfg", "::/EFI/" + vendor + "/grub.cfg", "::/boot/grub/grub.cfg"]:
        run("mcopy", "-i", image, config, destination)
    return image, {"efi_sha256": hashes, "image_bytes": image.stat().st_size,
                   "image_sha256": hashlib.sha256(image.read_bytes()).hexdigest(), "commands": calls}


def optical_bios_grub(volume_id):
    # The preserved Buildroot BIOS loader has fewer built-in modules. Keep its
    # boot-device root and avoid optional EFI presentation/search commands.
    optical_grub(volume_id)  # Apply exactly the same label validation.
    return (
        "set timeout=0\nmenuentry 'CRISPAWN LiveOS' {\n"
        f" linux /boot/bzImage {KERNEL_DISPLAY_ARGS} net.ifnames=0 rdinit=/init pb.live=1 "
        f"pb.optical_label={volume_id} module.sig_enforce=1\n"
        " initrd /boot/initrd\n}\n"
    )


def optical_boot_arguments():
    # Rebuild the catalog: retain the base's BIOS loader, replace its unsigned
    # EFI image with the FAT image containing the unchanged signed vendor chain.
    return ["-boot_image", "grub", "bin_path=/boot/grub/grub-eltorito.img",
            "-boot_image", "grub", "cat_path=/boot.catalog",
            "-boot_image", "grub", "load_size=2048",
            "-boot_image", "grub", "boot_info_table=on",
            "-boot_image", "any", "next",
            "-boot_image", "any", "efi_path=/boot/fat.efi"]
