# Boot without a serial port

CRISPAWN does not require a COM port. RAM diagnostics, the GUI and NTFS logs
must work independently of the optional UART diagnostic stream.

## alpha.3 failure

The alpha.3 startup script redirected the live service directly to
`/dev/ttyS0`. If opening that device failed, the shell could fail before
executing the service, while still writing the background PID file.

An absent UART can also expose a character device that opens successfully
but rejects writes. This occurred in the real QEMU test with `-serial none`:
the character-device check and open both succeeded, but writing failed and
the kernel reported the UART type as unknown. An open-only fallback did not
fix that case.

The live diagnostic thread used `println!`. A failed write panics, and the
release profile uses `panic = "abort"`. A diagnostic failure could therefore
terminate the worker before it mounted the destination. The supervisor could
then power off the guest, leaving the splash visible and no NTFS results.
The original release executable was separately confirmed to abort when its
standard output rejected writes, using a non-Live command mode.

## Fix

- Open and test-write the optional UART before launching the service.
- Fall back to `/dev/null` if the device is absent, cannot open, or rejects
  the probe write. The structured RAM and NTFS logs are independent.
- Handle failures from the live diagnostic writer without panicking.
- Keep UART diagnostics when output is available.

GRUB's optional serial output and the kernel console arguments were audited.
The default init script already tolerates an unavailable `/dev/console`,
and the serial getty is disabled. Actual Secure Boot tests without UART
hardware passed with those boot settings unchanged. Signed Debian kernel,
shim and GRUB bytes remain unchanged.

## Regression checks

`test/config/crispawn-startup.test.py` executes the shell logic for a missing
node, an unusable character device, a device that opens but cannot write, and
a working pseudo terminal. It also demonstrates the original redirection
failure before service execution. The same cases were run with the ISO's
BusyBox shell during candidate validation.

Rust tests verify that a diagnostic I/O error does not panic and that a
working output preserves complete probe lines.

`tools/test-crispawn-console-qemu.py` boots a synthetic 1 GiB disk with Secure
Boot enabled, with and without a UART. For the UART-less case it verifies the
actual QEMU device tree. It reads the committed archive and logs directly
from NTFS after guest poweroff, checks read/verified byte counts, and performs
independent source/hash verification. GUI frames are retained for visual
review. Poweroff alone never counts as a successful backup.

The media remains `/crispawn.iso`, media layout v2, NTFS label `CRISPAWN`,
and the `crispawn` data directory. No Builder change or physical USB write is
part of this fix. Hyper-V and physical-device acceptance must be reported
separately from QEMU results.
