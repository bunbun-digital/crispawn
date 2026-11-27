# CRISPAWN integration compatibility

Current source folders, Rust packages, commands, C headers, exported UI symbols
and the shared UI library use CRISPAWN names:

- Application workspace: `examples/crispawn/rust/`.
- Rust packages: `crispawn-core`, `crispawn-engine`, `crispawn-gui`, `crispawn-ui`.
- Commands: `crispawn`, `crispawn-engine`, `crispawn-live`, `crispawn-gui`.
- UI library: `libcrispawn-ui.so.1`; C headers: `crispawn/ui.h` and `crispawn/widgets.h`.
- UI linker configuration: `CRISPAWN_UI_LIB_DIR`.
- Runtime data: `/run/crispawn`; persistent data: `crispawn/` on NTFS.

## Builder and stored-data interfaces

ISO-internal configuration and stored-data paths retain their existing spelling. They are
integration identifiers, not product names. Boot media uses the separate v2
file layout below; old ISO filenames are not supported.

| Identifier | Purpose |
| --- | --- |
| `/etc/perfect-backup/live.json` | Embedded configuration written by Builder |
| `/etc/perfect-backup/release-capabilities.json` | Capability metadata read by Builder |
| `/etc/perfect-backup/live-root` and its marker payload | LiveISO identification used by configuration tools |
| `perfect-backup/` in saved archive paths | Legacy data path read as `crispawn/`; original records are not rewritten |

Changing Builder-facing paths requires a coordinated integration update.
The archive layout, binary format, BLAKE3 range hashes, diagnostic probe markers
and kernel boot parameters are preserved by the source naming cleanup.

## Segmented backup storage

Automatic-copy plans now select a versioned segmented layout. This is a stored
archive extension, not a change to the Builder boot-media/configuration interface.
New readers support existing contiguous backups. Older ISOs reject the new plan
field; use this ISO or a compatible newer release to restore segmented backups.
The layout, original-data and stored-data hashes, limits and progress units are
specified in [Segmented compressed storage](SEGMENTED-STORAGE.md).

From 0.1.0-alpha.5, new backups use segmented descriptor schema 2 (large
appended part files) and may contain `xz:6` frames while a writer backlog holds
xz. Both are additive: this ISO still reads schema-1 descriptors and every
earlier frame profile. ISOs before alpha.5 reject schema-2 descriptors and
`xz:6` frames; restore such backups with alpha.5 or a compatible newer release.

## USB media layout v2

The ISO contains `/boot/crispawn-media.json` with exactly these five fields:

```json
{"kind":"crispawn.media_layout","schema_version":2,"boot_iso_path":"/crispawn.iso","data_volume_label":"CRISPAWN","data_directory":"crispawn"}
```

Builder must require an exact match with the five-field JSON object above
before writing USB media. The signed
USB route places the ISO at the ESP root as `/crispawn.iso`; the ordinary
unsigned virtual-media route uses the same filename on NTFS. GRUB and LiveOS
both use that exact path. Hash-suffixed filenames are not aliases. A missing
file reports `BOOT_ISO_NOT_FOUND` with its mounted path, before backup writes.
Optical boot continues to use its read-only ISO9660 volume directly.
The external USB configuration override is `/crispawn-config.json` at the same
boot-volume root. Builder must use these exact ISO and configuration filenames.
ISO-internal configuration paths and stored-data interfaces in the table above
are unchanged.

## Historical releases

The SBOM and source archive attached to a published release describe that exact
release. Their original names and hashes remain intact. Rebuilt releases have
regenerated inventories for the new CRISPAWN package names. Git history and
previous releases remain available for auditing.
