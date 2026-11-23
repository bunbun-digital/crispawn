# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 bunbun-liveos contributors. Licensed under Apache-2.0.
"""Bounded runner for an owned QEMU guest; only normal guest shutdown passes."""
import json
from pathlib import Path
import queue
import shutil
import socket
import subprocess
import threading
import time


def run_guest(work, evidence, usb_image, disk_image, timeout=600, readonly=False, secure_boot=False, usb_format="raw", capture_gui=False,
              exercise_shortcuts=False, cpus=2, memory_mib=1536, expected_failure_code=None,
              extra_disk_images=(), ui_scenario=None, record_frames=False, source_read_bps=None, source_serial="PB-SOURCE-0001",
              network="none", optical_iso=None, legacy_bios=False, expect_failure_stay=False,
              serial_port=True):
    assert usb_format in ["raw", "vhdx"]
    assert isinstance(cpus, int) and 1 <= cpus <= 32
    assert isinstance(memory_mib, int) and 512 <= memory_mib <= 16384
    assert network in ('none', 'isolated-user')
    assert not (legacy_bios and secure_boot)
    assert not legacy_bios or optical_iso is not None
    assert not expect_failure_stay or expected_failure_code is not None
    assert type(serial_port) is bool
    # With no UART there are no live probes. The caller must validate the
    # persisted NTFS archive/logs and captured GUI, separately from shutdown.
    assert serial_port or not (expected_failure_code or exercise_shortcuts or ui_scenario or expect_failure_stay)
    assert shutil.disk_usage(evidence).free >= 10 * 1024**3, "infrastructure: need at least 10 GiB free before the owned VM test"
    # Use the same capable firmware for optical Secure Boot on/off comparisons.
    # An empty private VARS store disables Secure Boot without dropping the
    # firmware implementation of its observation variables.
    secure_firmware = secure_boot or (optical_iso is not None and not legacy_bios)
    variables = work / "OVMF_VARS.fd"
    firmware = "/usr/share/OVMF/OVMF_CODE_4M.secboot.fd" if secure_firmware else "/usr/share/OVMF/OVMF_CODE_4M.fd"
    if not legacy_bios:
        shutil.copyfile("/usr/share/OVMF/OVMF_VARS_4M.ms.fd" if secure_boot else "/usr/share/OVMF/OVMF_VARS_4M.fd", variables)
    qmp_path = work / "qmp.sock"
    argv = ["qemu-system-x86_64", "-machine", "q35,accel=tcg" + (",smm=on" if secure_firmware else ""), "-cpu", "max", "-smp", str(cpus), "-m", str(memory_mib),
            *([] if legacy_bios else ["-drive", "if=pflash,format=raw,unit=0,readonly=on,file=" + firmware,
                                      "-drive", f"if=pflash,format=raw,unit=1,file={variables}"]),
            "-device", "qemu-xhci,id=xhci", "-device", "usb-kbd,bus=xhci.0",
            "-drive", f"if=none,id=usb,format={usb_format},file={usb_image},cache=writeback",
            "-device", "usb-storage,bus=xhci.0,drive=usb,bootindex=" + ("2" if optical_iso else "1") + ",serial=CRISPAWN-USB",
            "-drive", f"if=none,id=source,format=raw,file={disk_image}" + (",readonly=on" if readonly else "") +
                      (f",throttling.bps-read={int(source_read_bps)}" if source_read_bps else ""),
            "-device", f"nvme,drive=source,serial={source_serial}",
            "-vga", "std", "-display", "none", "-serial", "stdio" if serial_port else "none", "-monitor", "none",
            "-qmp", f"unix:{qmp_path},server=on,wait=off", "-no-reboot"]
    if optical_iso is not None:
        assert optical_iso.is_file() and not optical_iso.is_symlink()
        argv += ["-drive", f"if=none,id=optical,format=raw,media=cdrom,readonly=on,file={optical_iso}",
                 "-device", "ide-cd,drive=optical,bus=ide.0,bootindex=1"]
    if network == 'none':
        argv += ['-nic', 'none']
    else:
        # QEMU's private DHCP/DNS only; no tap, host interface changes or external access.
        argv += ['-netdev', 'user,id=net0,restrict=on', '-device', 'e1000e,netdev=net0',
                 '-object', f'filter-dump,id=netcapture,netdev=net0,file={evidence / "network.pcap"}']
    if secure_firmware:
        argv += ["-global", "driver=cfi.pflash01,property=secure,value=on"]
    for index, path in enumerate(extra_disk_images, 2):
        assert path.is_file() and not path.is_symlink()
        argv += ["-drive",f"if=none,id=source{index},format=raw,file={path},readonly=on" +
                 (f",throttling.bps-read={int(source_read_bps)}" if source_read_bps else ""),
                 "-device",f"nvme,drive=source{index},serial=PB-SOURCE-{index:04}"]
    (evidence / "qemu-argv.json").write_text(json.dumps(argv, indent=2) + "\n")
    probes = []
    failures = []
    process = None
    qmp_events = []
    serial_markers = []
    sock = None
    reader = None
    try:
        with (evidence / "qemu.stderr").open("wb") as errors:
            process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=errors)

            def collect():
                with (evidence / "serial.log").open("wb") as log, (evidence / "probes.jsonl").open("w") as records:
                    for line in process.stdout:
                        log.write(line); log.flush()
                        if b"Linux version " in line or b"fb0:" in line:
                            serial_markers.append({"name":"kernel" if b"Linux version " in line else "framebuffer",
                                                   "host_monotonic":time.monotonic()})
                            (evidence / "serial-markers.json").write_text(json.dumps(serial_markers,indent=2)+"\n")
                        at = line.find(b"PB_PROBE ")
                        if at >= 0:
                            try:
                                value = json.loads(line[at + 9:])
                                probes.append(value); records.write(json.dumps(value) + "\n"); records.flush()
                            except (ValueError, UnicodeDecodeError):
                                pass
                        at = line.find(b"PB_FAILURE ")
                        if at >= 0:
                            try:
                                failure = json.loads(line[at + 11:])
                                failures.append(failure)
                                (evidence / "failures.json").write_text(json.dumps(failures, indent=2) + "\n")
                            except (ValueError, UnicodeDecodeError):
                                pass

            reader = threading.Thread(target=collect, daemon=True); reader.start()
            until = time.monotonic() + timeout
            while not qmp_path.exists():
                assert process.poll() is None and time.monotonic() < until
                time.sleep(0.1)
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); sock.settimeout(5); sock.connect(str(qmp_path))
            buffer = bytearray()

            def receive():
                while b"\n" not in buffer:
                    data = sock.recv(65536)
                    if not data:
                        raise EOFError("QMP closed")
                    buffer.extend(data)
                line, _, rest = buffer.partition(b"\n"); buffer[:] = rest
                message = json.loads(line)
                qmp_events.append(message)
                return message

            assert "QMP" in receive()
            counter = 0

            def execute(name, arguments=None):
                nonlocal counter
                counter += 1
                sock.sendall((json.dumps({"execute": name, "arguments": arguments or {}, "id": counter}) + "\n").encode())
                while True:
                    message = receive()
                    if message.get("id") == counter:
                        assert "error" not in message, message
                        return message["return"]

            execute("qmp_capabilities")
            resources = {"cpus": execute("query-cpus-fast"), "memory": execute("query-memory-size-summary")}
            resources['serial_port'] = serial_port
            if not serial_port:
                tree = execute('human-monitor-command', {'command-line':'info qtree'})
                (evidence/'device-tree.txt').write_text(tree)
                assert not any(name in tree for name in ('isa-serial', 'pci-serial', 'virtio-serial'))
                resources['chardevs'] = execute('query-chardev')
            assert len(resources["cpus"]) == cpus and resources["memory"]["base-memory"] == memory_mib * 1024**2
            (evidence / "qemu-resources.json").write_text(json.dumps(resources, indent=2) + "\n")
            captured = set()
            screens = []
            recording_start = time.monotonic()
            shortcut_stage, shortcut_at, pause_at = 0, 0.0, None
            shortcut_events = []
            ui_stage = 0
            ui_at = 0.0
            paused_values = None
            frames = []
            failure_stay_since = None
            failure_stay_heartbeat = None
            failure_stay_verified = False
            failure_seen = None
            if record_frames: (evidence / "frames").mkdir(exist_ok=False)
            def press(key):
                execute("send-key", {"keys":[{"type":"qcode","data":key}],"hold-time":50})
                shortcut_events.append({"key":key,"host_monotonic":time.monotonic(),
                                        "guest_phase":probes[-1]["phase"] if probes else "firmware"})
            with (evidence / "host-monitor.jsonl").open("w") as monitor:
                while process.poll() is None and time.monotonic() < until:
                    available = shutil.disk_usage(evidence).free
                    assert available >= 2 * 1024**3, "infrastructure: local free space fell below 2 GiB; preserve partial evidence and stop the owned test"
                    if expected_failure_code is None and probes and probes[-1]["phase"] in ["FAILED", "CANCELLED"]:
                        raise AssertionError("guest failure: " + probes[-1]["detail"])
                    try:
                        status = execute("query-status")
                        phase = probes[-1]["phase"] if probes else ("firmware" if serial_port else "unobserved-no-serial")
                        if capture_gui and phase == 'FAILED':
                            if failure_seen is None: failure_seen = time.monotonic()
                            if time.monotonic() - failure_seen >= .75 and not (evidence/'failure.png').exists():
                                execute('screendump', {'filename':str(evidence/'failure.png'), 'format':'png'})
                        if expect_failure_stay and probes and phase == 'FAILED' and not probes[-1]['ntfs_mounted']:
                            assert not probes[-1]['poweroff_requested']
                            assert probes[-1].get('shutdown_countdown_seconds') is None
                            if failure_stay_since is None:
                                failure_stay_since = time.monotonic()
                                failure_stay_heartbeat = probes[-1]['heartbeat']
                            elif time.monotonic() - failure_stay_since >= 8:
                                assert probes[-1]['heartbeat'] > failure_stay_heartbeat
                                assert any(f.get('code') == expected_failure_code for f in failures)
                                failure_stay_verified = True
                                # The test ends the deliberately stopped VM. This
                                # must never be reported as a normal guest shutdown.
                                (evidence/'failure-stay.json').write_text(json.dumps({
                                    'verified':True,'observed_seconds':time.monotonic()-failure_stay_since,
                                    'heartbeats_advanced':True,'ntfs_unmounted':True,
                                    'poweroff_requested':False,'test_requested_qmp_quit':True},indent=2)+'\n')
                                try: execute('quit')
                                except (EOFError, OSError): pass
                                process.wait(timeout=10)
                                break
                        if ui_scenario and probes and probes[-1]["gui_ready"]:
                            current = probes[-1]
                            page = current.get("session",{}).get("page")
                            now = time.monotonic()
                            def screenshot(name):
                                execute("screendump",{"filename":str(evidence/(name+".png")),"format":"png"})
                            if ui_scenario == "restore":
                                if ui_stage == 0 and phase == "MENU": press("r");ui_stage=1
                                elif ui_stage == 1 and page == "restore_selection": screenshot("restore-history");press("1");ui_stage=2;ui_at=now
                                elif ui_stage == 2 and now-ui_at>0.5: press("n");ui_stage=3
                                elif ui_stage == 3 and page == "restore_mapping": press("1");ui_stage=4;ui_at=now
                                elif ui_stage == 4 and now-ui_at>0.5: press("a");ui_stage=5;ui_at=now
                                elif ui_stage == 5 and now-ui_at>0.5: screenshot("restore-mapping");press("n");ui_stage=6
                                elif ui_stage == 6 and page == "restore_confirmation": screenshot("restore-review");press("y");ui_stage=7
                                elif ui_stage == 7 and phase == "RESTORING": screenshot("restore-running");ui_stage=20
                            elif ui_scenario == "copy":
                                if ui_stage == 0 and phase == "MENU":
                                    screenshot("copy-menu");ui_stage=1
                                elif ui_stage == 1 and phase == "COPYING" and current["can_pause"]:
                                    screenshot("copy-running");press("i");ui_stage=2;ui_at=now
                                elif ui_stage == 2 and now-ui_at>1:
                                    screenshot("internal-logs");press("pgup");press("end");press("i");ui_stage=3;ui_at=now
                                elif ui_stage == 3 and now-ui_at>1:
                                    press("c");ui_stage=4;ui_at=now
                                elif ui_stage == 4 and now-ui_at>1:
                                    assert phase == "COPYING", "opening cancellation dialog paused/cancelled the copy"
                                    screenshot("cancel-confirmation");press("esc");ui_stage=5;ui_at=now
                                elif ui_stage == 5 and now-ui_at>1 and current["can_pause"]:
                                    press("p");ui_stage=6
                                elif ui_stage == 6 and phase == "PAUSED":
                                    values=(current["snapshot"]["read_raw_bytes"],current["snapshot"]["durable_raw_bytes"])
                                    if paused_values is None: paused_values=values;ui_at=now;screenshot("copy-paused")
                                    else:
                                        assert values == paused_values, "copy counters changed after pause acknowledgement"
                                        if now-ui_at>1:press("p");ui_stage=7
                                elif ui_stage == 7 and phase == "COPYING":screenshot("copy-resumed");ui_stage=20
                            if phase == "SHUTDOWN_COUNTDOWN" and not (evidence/"all-complete.png").exists():screenshot("all-complete")
                        if exercise_shortcuts and probes and probes[-1]["gui_ready"]:
                            if shortcut_stage == 0 and phase == "MENU":
                                press("down");press("ret")
                                shortcut_at=time.monotonic();shortcut_stage=1
                            elif shortcut_stage == 1 and time.monotonic()-shortcut_at >= 1.5:
                                assert phase == "MENU", "arrow/Enter activated a command"
                                press("b");shortcut_stage=2
                            elif shortcut_stage == 2 and phase == "COPYING" and probes[-1]["can_pause"]:
                                press("p");shortcut_stage=3
                            elif shortcut_stage == 3 and phase == "PAUSED":
                                if pause_at is None:pause_at=time.monotonic()
                                elif time.monotonic()-pause_at>=1:
                                    press("p");shortcut_stage=4
                            elif shortcut_stage == 4 and phase in ("COPYING","VERIFYING","JOB_COMPLETE","COMPLETE","SAVING_LOGS","UNMOUNTING_NTFS"):
                                shortcut_stage=5
                        # Sample early boot too: there may be no backend probe
                        # while GRUB or the Linux-only splash owns the screen.
                        elapsed = time.monotonic() - recording_start
                        if record_frames:
                            assert len(frames)<2400, "recording frame limit exceeded"
                            name=f"frames/frame-{len(frames):06}.png"
                            execute("screendump",{"filename":str(evidence/name),"format":"png"})
                            frames.append({"file":name,"elapsed_seconds":elapsed,"phase":phase})
                            (evidence/"frames.json").write_text(json.dumps(frames,indent=2)+"\n")
                        key = phase
                        if phase in ("firmware", "STARTING", "DISCOVERING", "MOUNTING_NTFS"):
                            key += "-" + str(int(elapsed * 2))
                        elif phase == "SHUTDOWN_COUNTDOWN" or (phase == 'FAILED' and probes[-1].get('shutdown_countdown_seconds') is not None):
                            key += "-" + str(probes[-1].get("shutdown_countdown_seconds"))
                        if capture_gui and key not in captured:
                            name = "screen-" + str(len(captured)) + ".ppm"
                            execute("screendump", {"filename": str(evidence / name)})
                            captured.add(key)
                            screens.append({"file":name,"phase":phase,"elapsed_seconds":elapsed,
                                            "host_monotonic":recording_start+elapsed,
                                            "shutdown_countdown_seconds":probes[-1].get("shutdown_countdown_seconds") if probes else None})
                            (evidence / "screens.json").write_text(json.dumps(screens,indent=2)+"\n")
                        monitor.write(json.dumps({"host_monotonic": time.monotonic(), "qemu": status,
                                                  "host_free_bytes": available,
                                                  "guest_heartbeat": probes[-1]["heartbeat"] if probes else None}) + "\n")
                        monitor.flush()
                    except (EOFError, OSError):
                        if process.poll() is None:
                            time.sleep(0.1)
                    time.sleep(0.5)
            assert process.poll() is not None and process.returncode == 0, "no normal VM termination before deadline"
            try:
                while True:
                    receive()
            except (EOFError, OSError):
                pass
            reader.join(timeout=5)
        if expect_failure_stay:
            assert failure_stay_verified, 'configured stopped state was not observed'
            assert not any(e.get('event') == 'SHUTDOWN' and e.get('data',{}).get('guest') is True for e in qmp_events)
        else:
            assert any(e.get("event") == "SHUTDOWN" and e.get("data", {}).get("guest") is True for e in qmp_events)
        if not serial_port:
            # A normal poweroff alone is not backup acceptance. The dedicated
            # console regression test independently reads the NTFS result.
            assert not probes and not failures
        elif expected_failure_code is None:
            assert any(p["phase"] == "UNMOUNTED" and not p["ntfs_mounted"] for p in probes)
            assert any(p["phase"] == "POWEROFF_REQUESTED" and p["poweroff_requested"] for p in probes)
        else:
            assert any(f.get("code") == expected_failure_code for f in failures), "expected typed guest failure missing"
            assert any(p["phase"] == "FAILED" and expected_failure_code in p["detail"] for p in probes)
            assert not any(p.get("finalization_complete") for p in probes), "failed run reported successful finalization"
        if exercise_shortcuts:
            (evidence/"shortcut-events.json").write_text(json.dumps(shortcut_events,indent=2)+"\n")
            assert shortcut_stage == 5, "not all physical keyboard shortcut stages were observed"
        if ui_scenario:
            (evidence/"ui-key-events.json").write_text(json.dumps(shortcut_events,indent=2)+"\n")
            assert ui_stage == 20, "not all real UI steps completed"
        return probes
    finally:
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill(); process.wait(timeout=10)
        if reader:
            reader.join(timeout=3)
        if sock:
            sock.close()
        (evidence / "qmp.json").write_text(json.dumps(qmp_events, indent=2) + "\n")
