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

These paths deliberately retain their existing spelling because Builder and
existing boot media use them. They are integration identifiers, not product names.

| Identifier | Purpose |
| --- | --- |
| `/etc/perfect-backup/live.json` | Embedded configuration written by Builder |
| `/etc/perfect-backup/release-capabilities.json` | Capability metadata read by Builder |
| `/etc/perfect-backup/live-root` and its marker payload | LiveISO identification used by configuration tools |
| `/perfect-backup.json` | External USB configuration override |
| `/perfect-backup.iso` | ISO pathname used by Builder-created USB boot media |
| `perfect-backup/` in saved archive paths | Legacy data path read as `crispawn/`; original records are not rewritten |

Changing Builder-facing paths requires a coordinated integration update.
The archive layout, binary format, BLAKE3 range hashes, diagnostic probe markers
and kernel boot parameters are preserved by the source naming cleanup.

## Historical releases

The SBOM and source archive attached to a published release describe that exact
release. Their original names and hashes remain intact. Rebuilt releases have
regenerated inventories for the new CRISPAWN package names. Git history and
previous releases remain available for auditing.
