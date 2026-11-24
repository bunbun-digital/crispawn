# Copy pipeline and failure records

The productive v3 path separates source reads, compression and BLAKE3 from
destination writes, checkpoints and concurrent verification:

```text
source -> parallel reads/encoding -> ordered per-file BLAKE3
       -> bounded encoded queue -> buffered file writes -> Linux cache -> USB
                                   -> durable checkpoints -> file verification
```

The producer continues useful work while the destination writer is waiting for
a write, file creation, journal update or fsync. It uses the front/rear schedule
and productive compression feedback; it does not discard a trial copy. Raw
buffers exist only in the bounded read/encode/reordering workspace. Each
ordered raw buffer is released after updating its file's standard BLAKE3 state.
Only encoded payloads and bounded hash/metadata state cross the writer queue.

Queue accounting charges allocated payload capacity and per-item overhead,
including zero-compressed frames. Automatic live v2 processing uses 75% of
available RAM after a 192 MiB reserve, leaving the rest for the OS/cache/UI;
it no longer inherits a fixed 4 GiB default as its hardware ceiling. Explicit
legacy/fixed limits remain caps. The encoded queue grows lazily into remaining
processing RAM after worst-case encoder, raw/reordering and verification
workspaces, with an 8 GiB queue ceiling. Selection reserves these together.
Older serialized v3 plans keep their
historical validation rules; resuming may lower the effective producer ceiling
to fit the new queue without rewriting the archive plan. Queue pressure stops
prefetch until memory becomes available and is not itself an I/O error.

Writes continue to use the ordinary Linux page cache. RAM size alone does not
promise any particular amount of unsynced data: the processing limit, encoded
queue limit, kernel writeback limits and durability checkpoints all matter.
For example, eight 16 MiB frames give a 128 MiB *raw* checkpoint interval, not
a promise to cache 10 GB before synchronizing. Checkpoint durability is preserved;
the producer no longer shares the writer's synchronization wait.

`read_ahead` job events report actual completed source reads, queued encoded
bytes and queue budget. Read-ahead does not advance durable/verified progress
or completion. GUI read counters retain these observations when an older writer
snapshot arrives; a different backup ID cannot change the current counter.
These live samples update RAM status rather than requiring an additional NTFS
fsync for each sample. Lifecycle, tuning and failure records retain their normal
persistent logging. The independent failure file includes the latest matching
progress snapshot when available.

Pausing waits for active source and verification I/O to become quiescent and
synchronizes the already written prefixes. Encoded frames remain in bounded RAM;
they are not all flushed to the USB before acknowledging pause. Read bytes can
therefore exceed durable bytes while paused. Resume retains the queued data and
resets tuning timing; cancellation releases backpressure and leaves only durable
prefixes for an explicit resume, without a COMMITTED marker.

## Slow writes and the stall deadline

The supervisor accepts real completed block writes on the registered destination
partition as progress while a long write/fsync is still pending. It samples the
kernel's written-sector counter; unchanged/unavailable/reset counters and RAM/UI
heartbeats do not extend the deadline. This avoids identifying a slow but active
device as stalled solely because one userspace syscall has not returned.
The configured no-progress interval still applies when actual progress stops.

## Failure persistence independent of the collector

Normal worker failures retain the existing log export. For supervisor-detected
failure, including a worker abort or I/O stall, a separate helper also attempts:

```text
crispawn/logs/<job-id>-failure-<unique-id>/failure.json
```

This is on the already mounted NTFS data volume. The helper verifies the registered
major/minor number, writable `ntfs3` mount and opened directory device. Directory
operations use no-follow directory descriptors. It never mounts another disk,
forces a dirty filesystem, repairs media, overwrites normal logs or writes a
COMMITTED marker. `logging.policy: "never"` disables this USB write attempt.

The record contains the failure code/detail, localized message, time, worker PID
and a bounded kernel-message tail when available. Success requires file and
directory synchronization; merely creating a pathname is not success.

The attempt has a five-second deadline, including its synchronization. On timeout
the parent does not wait indefinitely for a child blocked in kernel I/O, and
durability remains unknown. A failed/disconnected USB cannot guarantee persistent
failure evidence. The attempt result is retained in RAM as
`/run/crispawn/failure-persistence.json`, alongside the reserved emergency reason.

The supervisor independently publishes FAILED status and a RAM
`logs/supervisor-failure.jsonl` record for the existing GUI/internal-log viewer.
It reuses the independent early GUI, or attempts a new GUI if the worker-owned
one ended. It honors the configured stopped state or failure poweroff/countdown;
the failure screen no longer depends on the failed worker/collector returning.
RAM-only evidence still disappears at poweroff.

## Validation limits

Ordinary-file tests cover producer progress during a held writer checkpoint,
per-file BLAKE3/restore, interrupted resume, read and write failures, bounded
queue backpressure, cancellation and independent failure-file persistence.
These establish software behavior, not USB hardware health or throughput.
The physical report that triggered this work contained no terminal failure code;
its exact blocked operation and hardware cause remain unproven. No new VM or
physical-device reproduction is implied by these tests.
