<p align="center">
  <img src="examples/perfect-backup/assets/crispawn/logo-hero.png" alt="CRISPAWN" width="860">
</p>

# CRISPAWN Community

CRISPAWN boots a small Linux environment to copy whole internal disks to a USB
hard disk and restore saved copies. The community edition works offline.
[日本語](README.ja.md)

Download the ISO and its SHA-256 file from
[Releases](https://github.com/bunbun-digital/crispawn/releases).
The original software is [Apache-2.0](LICENSE). The CRISPAWN name, logos and
branded artwork are [All rights reserved](examples/perfect-backup/assets/crispawn/LICENSE).
Third-party components keep their own licenses; the ISO is a mixed-license distribution.

## Use

A bootable USB has a FAT32 EFI System Partition and an NTFS data partition
labeled `CRISPAWN`. [CRISPAWN Builder](https://github.com/bunbun-digital/crispawn-builder)
can write the ISO and settings to a prepared drive. It is maintained in its own
repository. The ISO also contains BIOS and signed UEFI optical boot entries.
Optical boot requires a separate explicitly configured USB destination.

On boot, internal disks are discovered while the automatic-copy countdown runs.
The default is 30 seconds. Copy and restore use the same GUI/CUI controller.
Saved files, metadata and logs are under `crispawn/` on the NTFS partition.
The boot and destination disks are excluded from copy sources. Restore always
requires target selection and confirmation; it overwrites the selected disk.

Copies cover every sector, including encrypted data and partition metadata.
Files are compressed, kept below 2,000,000,000 bytes, and paired with BLAKE3
hashes of their original ranges. Saved data is checked concurrently.
A smaller destination is allowed when compression makes it fit; copying stops
if actual free space falls below 1,000,000,000 bytes.
Errors and shutdown behavior are configurable.

The community ISO contains no connection worker, Wi-Fi tools, network drivers,
or SMB/S3 storage backend. Settings cannot enable those features.
The signed Debian kernel retains its built-in IP stack and local sockets.
Network and remote-storage extensions are developed separately for CRISPAWN Pro.
The community code may be modified under Apache-2.0; the USB-only scope describes
official community builds, not a restriction on third-party modifications.

## Develop

See [BUILDING.md](BUILDING.md), [configuration](docs/CONFIGURATION.md),
[architecture](docs/ARCHITECTURE.md) and [licensing](docs/LICENSING.md).
The current runtime source remains under `examples/perfect-backup/rust/` to
preserve existing package names, runtime paths and archive compatibility.
No old private repository, development ISO, SDK or Windows relay is required.

This is an early development release. See each release's test report for the
actual tested environment and limitations.

