# NTFS startup failures

CRISPAWN checks the destination's NTFS state before requesting a writable mount.
It reads `$Volume` through a read-only handle; this does not mount the filesystem,
replay its journal, repair it or clear the dirty flag. A known dirty volume stops
before copying. An unreadable state also stops safely.

| Code | Meaning |
| --- | --- |
| `NTFS_VOLUME_DIRTY` | The read-only inspection found the dirty flag, or this exact partition's kernel message explicitly reported dirty-volume refusal. |
| `NTFS_STATE_UNAVAILABLE` | The NTFS state could not be established. This does not by itself prove filesystem corruption or a defective drive. |
| `NTFS_JOURNAL_REPLAY_FAILED` | This exact partition's `ntfs3` message reported that journal replay failed. |
| `NTFS_MOUNT_FAILED` | The writable mount failed without enough evidence for a more specific classification. |

The GUI and console show Japanese or English explanations. The raw operating
system error remains in diagnostics. `EINVAL` / error 22 alone is not classified
as a dirty filesystem, corruption, an incompatible formatter or a hardware fault.
CRISPAWN never adds `force` to the writable mount options.

## Finding the diagnostic

The internal log records `ntfs_mount_failure`. A structured copy is retained in
RAM at `/run/crispawn/ntfs-mount-diagnostics.json`. It contains:

- Partition path, major/minor number, size and discovered media identity.
- Read-only inspection result: NTFS version, known volume flags, dirty state,
  sector/cluster/file-record sizes and volume serial, when readable.
- Inspection errors and available underlying errno, separately from mount errno.
- Whether a writable mount was attempted, its fixed options and `force: false`.
- `user_message`: the localized reason and recovery guidance also shown on screen.
- Kernel release, `ntfs3` availability and bounded new `/dev/kmsg` messages from
  the attempt. An unavailable or truncated kernel capture is explicitly marked.
- The current UEFI boot entry and bounded OptionalData observation, when exposed.
  This is not a claim to have read shim's transient LoadedImage arguments.

If logging is enabled and the verified FAT boot partition can be written, one
bounded JSON file is also saved there:

```text
crispawn/diagnostics/ntfs-mount-<job-id>.json
```

This fallback writes to the **boot partition**, not the failed NTFS partition.
It leaves the ISO and EFI files intact, syncs the diagnostic and returns the boot
mount to read-only. `logging.policy: "never"` disables this persistent fallback.
DVD/read-only boot media may provide RAM and screen diagnostics only. Failure to
save is recorded; successful persistence is never inferred from a path alone.
Normal NTFS copy logs remain at `crispawn/logs` on the data partition.

For failures after a successful mount, including a supervisor-detected stalled
worker, see [Copy pipeline and failure records](IO-PIPELINE.md). That path makes
a separate bounded NTFS failure-file attempt without waiting for the ordinary
log collector.

`failure.action` still selects `stay` or delayed `poweroff`. This diagnostic does
not change the configured failure/completion countdown or choose another disk.
For a dirty drive, repair it in Windows and safely eject it before trying again.
Alternatively, save any needed data first and use Builder to initialize and
recreate the USB drive. Initialization removes existing data; CRISPAWN never
initializes the drive automatically after a mount failure.
CRISPAWN does not run `chkdsk`, `ntfsfix` or an automatic repair.

## Read-only inspector isolation

The NTFS parser runs in a separate process with an inherited read-only partition
descriptor, a 2 MiB metadata-read budget and bounded IPC. Its timeout is at most
30 seconds and never longer than the configured no-progress interval. It uses
the configured transient-read retries. Parser abort, malformed output, timeout
or unreadable metadata becomes `NTFS_STATE_UNAVAILABLE` in the parent. There is
no indefinite wait for a child stuck in a kernel read. The inspector cannot
report a usable volume after an exhausted I/O error.

This separation is required because `ntfs` 0.4.0 can panic while converting a
nested `binrw` I/O error. The parent preserves the read errno sent before that
conversion and retains its own failure/logging/shutdown path. The third-party
source and its MIT/Apache-2.0 notices remain unmodified.

## Reproducing virtual-media tests

Build as described in [BUILDING.md](../BUILDING.md), then run:

```sh
python3 tools/test-crispawn-ntfs-qemu.py --iso /path/to/candidate.iso --work .orchestrator-test/new-ntfs-run
python3 tools/test-crispawn-console-qemu.py --iso /path/to/candidate.iso --work .orchestrator-test/new-console-run
```

The NTFS test checks successful copy and independent BLAKE3 readback, a dirty
volume whose NTFS bytes must remain unchanged, unreadable metadata, and an
explicit invalid-index-size mount failure. Error cases must read zero source
bytes, retain a diagnostic on the FAT boot partition and leave the signed EFI
files and ISO unchanged. Both UART and no-UART copying are tested separately.

Linux-created images do not establish Windows formatter compatibility. The
optional `--windows-media DIRECTORY` input accepts the clean golden VHDX, detached
dirty test clone and provenance from a reviewed Windows Builder test adapter.
It validates their hashes, converts them to new ordinary images, and boots them
with Secure Boot for copy/readback and dirty-volume refusal. The production
Builder helper and its Windows `Format-Volume` call must remain unchanged. An
adapter or configuration-staging step must be disclosed in the test report.
Neither these virtual tests nor a successful build proves physical USB behavior.

## Pre-OS shim messages

A `Failed to open \\EFI\\BOOT\\...` message followed by `falling back to default
loader` occurs before Linux's NTFS mount. Do not combine the two diagnoses.
With the release's signed Debian shim 16.1, synthetic four-byte firmware options
`46 4a 00 00` reproduce a filename of one Unicode character and the three-line
fallback message. Three-byte `46 4a 00` options are explicitly ignored in the
[upstream shim parser](https://github.com/rhboot/shim/blob/16.1/load-options.c).

The Fujitsu photograph shows `\\EFI\\BOOT\\` followed by an unrenderable glyph;
its actual argument bytes have not been observed. UEFI BootCurrent/Boot#### data
can help investigate, but might differ from the data passed to the loaded image.
The [shim build option](https://github.com/rhboot/shim/blob/16.1/BUILDING)
`DISABLE_REMOVABLE_LOAD_OPTIONS` is compile-time; changing it invalidates the
existing binary's Microsoft signature. CRISPAWN does not patch or re-sign shim,
enroll keys, or rewrite the machine's boot variables to hide this message.

A tested Microsoft-signed openSUSE shim 16.1 → signed Debian shim/GRUB/kernel
chain still produced the same warning with four-byte synthetic options. It was
not adopted. Keep the pinned Debian chain until a replacement actually meets
the boot, signature and warning criteria. Physical Fujitsu message suppression
and the original NTFS failure cause require fresh physical evidence.

A later [signed compatibility-stage experiment](SHIM-COMPATIBILITY.md) removed
the synthetic warning using an unchanged Ubuntu shim/GRUB before the existing
Debian chain. Secure Boot, copy, NTFS readback and native restore passed in that
virtual test. Production adoption requires coordinated Builder EFI placement;
the published ISO and the physical Fujitsu result remain unchanged.
