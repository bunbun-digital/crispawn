# Firmware load-option compatibility experiment

Status: experimental. The published ISO and production Builder have not been
changed. A new virtual USB boot removed the synthetic warning while retaining
Secure Boot, the signed Debian kernel, and successful copy/readback. Suppression
on the reported physical Fujitsu PC is **not verified**.

## Cause and scope

The pinned Debian shim 16.1 can interpret firmware load options as the name of
a second-stage loader. An earlier preserved QEMU baseline with optional data
`46 4a 00 00` displayed the three-line `Failed to open`, `Failed to load image`,
and `falling back to default loader` warning. The filename is the Unicode
interpretation of those bytes. This is distinct from an actually missing
`grubx64.efi` and from a later Linux NTFS mount failure.

The Fujitsu photograph has a similar unrenderable character after `\EFI\BOOT\`.
Its actual load-option bytes remain unknown; the synthetic case does not prove
the physical machine has identical firmware behavior.

[Upstream shim](https://github.com/rhboot/shim/blob/16.1/BUILDING) provides the
compile-time `DISABLE_REMOVABLE_LOAD_OPTIONS` option. Ubuntu's
[15.8-0ubuntu2 packaging](https://archive.ubuntu.com/ubuntu/pool/main/s/shim/shim_15.8-0ubuntu2.dsc)
enables it. The tested binaries were extracted from official Ubuntu packages,
not rebuilt or patched. Changing this option in an existing signed binary would
invalidate its signature.

## Tested layout

| ESP path | Unchanged signed file |
| --- | --- |
| `EFI/BOOT/BOOTX64.EFI` | Ubuntu shim, Microsoft-signed |
| `EFI/BOOT/grubx64.efi` | Ubuntu GRUB, Canonical-signed |
| `EFI/BOOT/mmx64.efi` | Ubuntu MokManager |
| `EFI/debian/shimx64.efi` | Existing Debian shim |
| `EFI/debian/grubx64.efi` | Existing Debian GRUB |
| `EFI/debian/mmx64.efi` | Existing Debian MokManager |

The Ubuntu GRUB configuration chainloads `/EFI/debian/shimx64.efi`. Debian GRUB
then uses the existing `EFI/debian/grub.cfg` to load `/crispawn.iso` and its
unchanged signed Debian kernel. This extra GRUB is necessary for this layout:
both vendors' shims normally look for `grubx64.efi` in their own directory.
Simply renaming Debian shim to that filename would point its default loader
back at itself. No private-key enrollment or validation bypass was used.

Inputs used for this experiment:

| Package | SHA-256 |
| --- | --- |
| `shim-signed_1.59+15.8-0ubuntu2_amd64.deb` | `f8ed71ce2d91a304b6d5eb84997f846f331b554578bc02dbfe78e13ad8ac81a9` |
| `grub-efi-amd64-signed_1.202.5+2.12-1ubuntu7.3_amd64.deb` | `8bd5cd99c3af82aab23af0f15f54c91799f343416412e122df661ef36a44a511` |

These are experimental input identities, not new production dependency pins.
The Ubuntu shim's listed issuer is Microsoft Corporation UEFI CA 2011; hardware
db/dbx and SBAT policy compatibility still needs separate testing.

## Observed result

One new ordinary-file virtual USB was tested in QEMU/OVMF with Secure Boot and
the synthetic four-byte load option. The previous baseline was not rerun.

- The recorded boot frames showed firmware followed by the CRISPAWN splash,
  without the three-line shim warning. Frames were sampled about every 0.5 s;
  this is visual evidence for this virtual test, not proof for every device.
- The serial trace reached the compatibility GRUB, the original Debian GRUB,
  and Linux `6.12.117+deb13-amd64`.
- Guest observations reported Secure Boot enabled, setup mode disabled,
  shim validation enabled, and kernel integrity lockdown.
- A 256 MiB synthetic source completed copying and confirmation. NTFS contained
  a COMMITTED archive and logs. QMP recorded a guest-initiated normal shutdown.
- After shutdown, the archive was independently extracted from NTFS and checked
  with the native engine. A native restore matched the original 268,435,456-byte
  source's SHA-256:
  `9fd29e393378c4401013b449d780fad9bb3dd63656403c35a9a495d13076315c`.
- All six EFI files were read back and matched their source bytes. The original
  source image and input firmware fixture were preserved.

The initial post-run collector rejected an early null `secure_boot` observation.
Acceptance was recovered from the saved probes and QMP events without rebooting
or changing product code. A separate extraction helper initially looked in the
wrong host-tools directory; it was corrected to use installed read-only NTFS
utilities against the same saved virtual disk. Neither collector failure was a
guest boot or copy failure.

## Work required before adoption

Builder currently extracts and writes only the three default EFI files. It
must support the additional Debian files and distinct vendor configurations;
an ISO-only change would not update that physical USB layout. Keep the existing
media layout v2 and `/crispawn.iso` identity checks.

The extra three Ubuntu EFI files total 4,483,280 bytes. All six total 9,077,688
bytes, so they do not fit the current 8 MiB optical FAT image. Optical support
would require a larger EFI image and separate boot validation; no new ISO size
or optical acceptance has been measured in this experiment.

Production adoption also requires fresh signed-profile pins, corresponding
sources, notices/SBOM updates, three-file and six-file media handling as decided
with Builder, and regression tests for optical boot, no-UART boot, clean/dirty
NTFS, and hardware revocation policy. The physical Fujitsu PC must finally be
checked for the actual pre-OS message. Until then, retain the current release
and treat this as a demonstrated virtual workaround, not a released fix.
