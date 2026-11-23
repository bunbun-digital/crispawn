# Runtime settings

Runtime settings are defined by `config/crispawn/schemas/live-v2.schema.json`.
Start from `config/crispawn/examples/live-v2-usb.json`.
The community edition requires USB/NTFS and `network.mode=none`.
Schemas describe shared wire types; schema validity alone never enables a backend.

- `startup.delay_seconds`: automatic-copy delay, 0–300 seconds; default 30.
- `ui.language`: `ja` or `en`.
- `completion.action`: `poweroff` or `stay`.
- `completion.delay_seconds`: 0–60 seconds for poweroff, default 5.
- `failure.action`: optional `poweroff` (default) or `stay`. Error poweroff uses
  the normal completion delay. If normal completion stays, the error delay is 5 seconds.
- The actual free-space cutoff is 1,000,000,000 bytes. No source-capacity reservation
  or consumption/progress forecast is used.
- `logging.policy`: `always`, `on_error`, or `never`. Logs use the backup destination.

Use `tools/configure-crispawn.py` to create a new configured ISO. Never edit an
existing image in place. On USB, an external `/crispawn-config.json` overrides
the embedded `/etc/perfect-backup/live.json` as a whole; malformed overrides
are rejected, not silently ignored. Internal legacy paths remain for Builder compatibility.

The ISO carries both `/usr/share/crispawn/runtime-capabilities.json` and the
Builder-facing `/etc/perfect-backup/release-capabilities.json`. Both list the
actual compiled implementation. The ISO also contains `/boot/crispawn-media.json`
with media-layout schema 2, `boot_iso_path` `/crispawn.iso`, label `CRISPAWN`
and top-level directory `crispawn`. The ISO belongs at the ESP root under that
exact filename; see [the USB file layout specification](COMPATIBILITY.md#usb-media-layout-v2).

