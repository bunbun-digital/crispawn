#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Execute the real startup shell logic with owned processes and pseudo TTYs."""
import os
from pathlib import Path
import pty
import shutil
import subprocess
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / 'examples/crispawn/br2-external/package/crispawn-live/S50crispawn-live'


class Startup(unittest.TestCase):
    def exercise(self, serial, shell, old=False):
        with tempfile.TemporaryDirectory(prefix='crispawn-startup-') as temporary:
            folder = Path(temporary)
            marker, pid = folder / 'started', folder / 'live.pid'
            stub = folder / 'live'
            stub.write_text(f'#!/bin/sh\nprintf "%s\\n" "$$" >"{marker}"\nprintf "diagnostic-output\\n"\nprintf "diagnostic-error\\n" >&2\n')
            stub.chmod(0o755)
            text = ('#!/bin/sh\n/usr/bin/crispawn-live </dev/null >/dev/ttyS0 2>&1 &\necho "$!" >/run/crispawn-live.pid\n'
                    if old else SCRIPT.read_text())
            text = text.replace('/usr/bin/crispawn-live', str(stub)).replace('/run/crispawn-live.pid', str(pid))
            text = text.replace('/dev/ttyS0', str(serial) if serial else str(folder / 'absent-uart'))
            local = folder / 'startup'
            local.write_text(text)
            # /dev/tty exists but cannot open in this new session without a
            # controlling terminal: a real ENXIO, not a mocked stat result.
            result = subprocess.run([*shell, str(local), 'start'], stdin=subprocess.DEVNULL,
                                    capture_output=True, timeout=5, start_new_session=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            until = time.monotonic() + (0.15 if old else 2)
            while not marker.exists() and time.monotonic() < until:
                time.sleep(.01)
            self.assertTrue(pid.is_file())
            if old:
                self.assertFalse(marker.exists(), 'old redirect unexpectedly executed the service')
                self.assertIn(b'No such device', result.stderr)
            else:
                self.assertTrue(marker.is_file(), 'service never started')
                self.assertEqual(pid.read_text(), marker.read_text())
                self.assertEqual(result.stdout, b'')
                self.assertEqual(result.stderr, b'')
            self.assertFalse((folder/'absent-uart').exists(), 'must not create a fake UART file')

    def shells(self):
        values = [['/bin/sh']]
        if shutil.which('busybox'):
            values.append([shutil.which('busybox'), 'sh'])
        return values

    def test_old_redirect_fails_before_service_exec_on_unusable_character_device(self):
        for shell in self.shells():
            with self.subTest(shell=shell):
                self.exercise('/dev/tty', shell, old=True)

    def test_missing_serial_node_still_starts_service(self):
        for shell in self.shells():
            with self.subTest(shell=shell):
                self.exercise(None, shell)

    def test_existing_but_unusable_serial_node_still_starts_service(self):
        for shell in self.shells():
            with self.subTest(shell=shell):
                self.exercise('/dev/tty', shell)

    def test_usable_serial_retains_stdout_and_stderr(self):
        for shell in self.shells():
            with self.subTest(shell=shell):
                master, slave = pty.openpty()
                try:
                    self.exercise(os.ttyname(slave), shell)
                    os.set_blocking(master, False)
                    text = os.read(master, 8192)
                    self.assertIn(b'diagnostic-output', text)
                    self.assertIn(b'diagnostic-error', text)
                finally:
                    os.close(slave)
                    os.close(master)

    def test_character_device_that_opens_but_cannot_write_uses_fallback(self):
        for shell in self.shells():
            with self.subTest(shell=shell):
                self.exercise('/dev/full', shell)


if __name__ == '__main__':
    unittest.main()
