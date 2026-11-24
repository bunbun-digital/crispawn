<p align="center">
  <img src="examples/crispawn/assets/crispawn/logo-hero.png" alt="CRISPAWN" width="860">
</p>

# CRISPAWN Community

CRISPAWN boots a small Linux environment to copy whole internal disks to a USB
hard disk and restore saved copies. The community edition works offline.
[日本語](README.ja.md)

For boot-media creation and settings, use
[CRISPAWN Builder for Windows](https://github.com/bunbun-digital/crispawn-builder).
Builder prepares the USB drive, ISO and configuration. This repository defines
the LiveISO and its externally observable behavior.

Download the ISO and its SHA-256 file from
[Releases](https://github.com/bunbun-digital/crispawn/releases).
The original software is [Apache-2.0](LICENSE). The CRISPAWN name, logos and
branded artwork are [All rights reserved](examples/crispawn/assets/crispawn/LICENSE).
Third-party components keep their own licenses; the ISO is a mixed-license distribution.

## Demo

![CRISPAWN copying a disk with the English UI](https://github.com/bunbun-digital/crispawn/releases/download/v0.1.0-alpha.3/crispawn-demo-en.gif)

[日本語デモ](README.ja.md)

A six-second time-lapse of a real QEMU run, from the first application screen
after the splash to normal shutdown. The synthetic disk is 10 GB with 1 GB of
compressible demo data. This demo uses a five-second countdown; the default is 30 seconds.

## ISO specification

| Property | Community LiveISO |
| --- | --- |
| Target | x86-64 PC; a live Linux environment that does not install an OS on the source disk |
| Boot | BIOS and UEFI optical boot; Builder-prepared USB boot; signed Debian kernel, shim and GRUB for Secure Boot |
| Size | Official build limit: 100,000,000 bytes; each release lists its actual size and SHA-256 |
| Interface | Keyboard-operated GUI in Japanese or English; console and headless modes are configurable |
| Storage | Local USB hard disk with an NTFS data volume; backup files can be listed from Windows |
| Network | Offline; no Wi-Fi/network drivers or SMB/S3 destination support in the official Community image |

Optical boot uses the read-only ISO directly and requires an explicitly
configured USB destination UUID. The USB file layout is described below.
Secure Boot support preserves the upstream signed files; tested platforms and
limitations are recorded per release.

## Configuration interface

Builder configures the ISO before boot. Runtime settings are a JSON document
with `kind: "crispawn.live"` and `schema_version: 2`.
The [complete USB example](config/crispawn/examples/live-v2-usb.json) and
[JSON Schema](config/crispawn/schemas/live-v2.schema.json) define the settings format;
the values below describe the standard profile, not permission to omit required fields.

On USB, `/crispawn-config.json` at the boot-volume root replaces the embedded
settings **as a complete document**. It is not a partial merge. If absent,
the embedded settings apply. Malformed settings, unknown fields and unsupported
features stop processing rather than silently selecting another destination.
See [ISO files used by Builder](docs/COMPATIBILITY.md#builder-and-stored-data-interfaces)
for the locations of embedded settings and supported-feature information.

| Setting | External behavior / standard profile |
| --- | --- |
| `sources` | `all_internal`, or `selected_ids` with stable disk IDs; standard: all eligible internal disks |
| `destination` | `kind: "usb_disk"`, `filesystem: "ntfs"`; data volume on the boot disk or an explicit UUID; directory `crispawn/backups` |
| `startup` | `auto_copy`, `menu`, or `restore_select`; standard: `auto_copy` after 30 seconds; GUI delay: 0–300 seconds |
| `ui` | `gui`, `console`, or `headless`; language `ja` or `en`; standard: Japanese GUI |
| `processing` | Automatic selection of read size, compression and concurrency; standard initial profile: `balanced` |
| `logging` | `always`, `on_error`, or `never`; standard: continuously write logs to the backup destination; copy-time durability sync is batched until finalization |
| `completion` | `poweroff` after 0–60 seconds, or `stay`; standard: poweroff after 5 seconds |
| `failure` | Optional `poweroff` or `stay`; standard: poweroff using the completion delay, or 5 seconds if normal completion is `stay` |
| `network` | Community requires `mode: "none"`; shared schemas containing remote-storage types do not enable them |

Console automatic copy requires a zero start delay. Headless mode requires
`auto_copy` with a zero delay; interactive restore selection is unavailable there.
See [runtime settings](docs/CONFIGURATION.md) for the full settings specification.

### Boot media layout

Every ISO contains `/boot/crispawn-media.json`, which specifies the boot ISO
path, data-volume label and storage directory. Before writing USB media,
Builder checks that this JSON object exactly matches the five-field example below:

```json
{"kind":"crispawn.media_layout","schema_version":2,"boot_iso_path":"/crispawn.iso","data_volume_label":"CRISPAWN","data_directory":"crispawn"}
```

The signed USB layout places `/crispawn.iso` and optional
`/crispawn-config.json` at the FAT32 EFI System Partition root, with backup data
on the NTFS volume labeled `CRISPAWN`. Hash-suffixed ISO filenames are not aliases.
A missing boot ISO stops before copying and reports `BOOT_ISO_NOT_FOUND` with
the expected path. See [the USB file layout specification](docs/COMPATIBILITY.md#usb-media-layout-v2).

## Copy and restore behavior

1. **Preparation:** startup discovers disks and checks the destination. Read/
   compression preparation proceeds without waiting for the GUI countdown to
   expire. Copy starts only after both preparation and the countdown finish.
2. **Selection:** eligible internal whole disks are initially selected unless
   IDs are preset. USB/removable disks and the boot/destination disks are
   excluded; in-use disks are unavailable. Multiple selected disks are copied
   one at a time. The source disk is read without modification.
3. **Copy:** all bytes are included, including partition tables, unused regions
   and BitLocker-encrypted contents. Encrypted data is copied without decryption;
   compressed size depends on the actual bytes, not the filesystem's used-space count.
4. **Progress and control:** the UI shows percent, bytes and GB, ETA, read size,
   compression/level and concurrency. Pause/resume is available; cancellation
   requires confirmation. The first stored part measures cached write/check latency
   and sets a baseline file size toward a soft five-second target. The UI also
   shows queued write blocks, current file MB, configured/active writer concurrency
   and read/write MB/s. At least two writer slots are provided; active use depends on available data.
   By default, each stored part is reread and checked by compressed-byte BLAKE3 while copying
   continues; backup confirmation does not decompress the data.
   `processing.verify_after_write=false` skips this copy-time readback. Hashes,
   final synchronization and restore checks remain enabled; the UI and logs
   explicitly show that copy-time checking was skipped.
5. **Restore:** a saved date/session, source disk from that set and destination
   disk are selected and confirmed. The target must fit the original disk's byte
   length. Restoring overwrites the selected target and checks restored ranges
   by reading them back while further ranges are restored.
6. **Completion:** success is shown only after all selected work and enabled checks
   finish, including the final storage synchronization. Copying does not wait for
   a physical-media sync after every file. There is no separate verification stage.
   Automatic resume after power loss is not supported. With poweroff enabled,
   the completion message includes the remaining shutdown time before normal shutdown.

A backup destination may be smaller than the source disk if compressed data
fits. Processing stops if actual free space falls below **1,000,000,000 bytes**;
there is no source-capacity reservation or consumption/progress forecast.
Transient I/O errors are retried within configured limits (standard: three
retries with a 100 ms delay). Unrecoverable errors, failed integrity checks or
the configured no-progress timeout cause failure, not success. Error messages
follow the selected language; failure logs are retained when storage remains
usable. `failure.action` selects an error display that stays on screen or a
delayed poweroff.

Dirty NTFS volumes are not force-mounted. When NTFS cannot be used, a diagnostic
is retained in memory and, when logging and the writable FAT boot partition are
available, under `crispawn/diagnostics` on that boot partition. See
[NTFS startup diagnostics](docs/NTFS-DIAGNOSTICS.md).

## Compression

CRISPAWN supports **zstd (Zstandard), gzip and xz**. The standard `balanced`
profile starts with zstd level 3, then automatically adjusts the method and
level based on compression results and processing speed. Current automatic
selection candidates are:

| Method | Compression levels considered |
| --- | --- |
| zstd | 1, 3, 6, 9 |
| gzip | 1, 6, 9 |
| xz | 0, 3 |

Data whose compressed size would equal or exceed its original size is stored
uncompressed (`raw`). All-zero data is stored as a compact record (`zero`) and
restored to the original number of zero bytes.

These methods are used within the CRISPAWN archive format. A backup may contain
multiple methods; the method and level are recorded with the saved data and
used automatically during restore.

## Saved data

The standard NTFS layout is:

```text
crispawn/
  backups/    Compressed disk archives and their metadata
  logs/       Logs for copy, restore and errors
  sessions/   Saved dates/sessions and disk selection information
  reports/    Operation reports
  work/       Temporary operation data
```

Each backup contains compressed part files, their ordering/position metadata,
`metadata.json`, `files.csv`, and completion/integrity records. Storage files can
split a compression frame, so keep the complete backup together for restore.
The first file's measured service time determines the baseline part size; tails
can be smaller. Every physical file remains below **2,000,000,000 bytes**.

With copy-time readback enabled, stored parts are checked against BLAKE3 calculated from the compressed bytes
before writing. The original-range BLAKE3 is retained separately for restored
output checks. Copy confirmation does not decompress the saved data. A single
whole-disk hash is not required. Older contiguous backups remain readable.
Restore newly segmented backups with this or a compatible newer ISO.

## Build and release information

See [BUILDING.md](BUILDING.md) for ISO generation and tests,
[configuration](docs/CONFIGURATION.md) for settings, and
[licensing](docs/LICENSING.md) for redistribution terms.

This is an early development release. See each release's test report for the
actual tested environment and limitations.

