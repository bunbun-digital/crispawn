# CRISPAWN integration compatibility

The product name is **CRISPAWN Community**. Some identifiers in the initial
release predate that name. They identify actual source packages, installed
files and integration interfaces; they do not describe separate products.

| Identifier | Current purpose |
| --- | --- |
| `examples/perfect-backup/rust/` | Existing Rust workspace location |
| `perfect-backup-*` | Internal executable and Rust package identifiers |
| `bunbun-ui`, `libbunbun-ui.so.1`, `BUNBUN_UI_LIB_DIR` | Rust-to-C UI runtime integration and dynamic library interface |
| `/etc/perfect-backup/live.json` | Embedded configuration consumed by LiveISO and written by Builder |
| `/etc/perfect-backup/release-capabilities.json` | Capability contract read by Builder |
| `/perfect-backup.json` | External USB configuration override |

Documentation uses CRISPAWN for the product and CRISPAWN UI SDK for its shared
UI API. Technical examples use the identifiers implemented by the current
source and released binaries, so they can be copied and run correctly.

Renaming those interfaces requires coordinated source changes and build and
compatibility checks. In particular, changing Builder-facing paths without a
matching integration update would break configured ISO creation.

The SBOM and corresponding-source archive attached to an existing release
must describe that exact release. Package and file names in those records are
not relabelled independently of their binaries. A renamed build needs a new
release and regenerated inventory. Git history and previous releases retain
their original, auditable identifiers.
