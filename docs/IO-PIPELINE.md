# Copy pipeline and failure records

Automatic plans now use [segmented compressed storage](SEGMENTED-STORAGE.md).
The producer and queue described here remain; physical part confirmation replaces
sealed-file decompression for those plans. Two writer/checker threads use a
bounded four-part queue and the OS cache, without per-file sync. Log records
continue appending without per-record sync during copying. After draining all
work, a filesystem barrier precedes the durably written COMMITTED marker;
final log/session close and unmount finish before success and poweroff.
Legacy contiguous plans retain their former checkpoint and sealed-file checking
path; historical checkpoint durability statements apply only to them.

The productive v3 path separates source reads, compression and BLAKE3 from
destination writes, checkpoints and concurrent verification:

```text
source -> parallel reads/encoding -> ordered per-file BLAKE3
       -> bounded encoded queue -> bounded part queue -> two cached writers
       -> per-part encoded-byte readback -> ordered descriptors
       -> final filesystem sync -> COMMITTED -> log/session close and unmount
```

The producer continues useful work while the destination writer is waiting for
a write, file creation, journal update or fsync. It uses the front/rear schedule
and productive compression feedback; it does not discard a trial copy. Raw
buffers exist only in the bounded read/encode/reordering workspace. Each
ordered raw buffer is released after updating its file's standard BLAKE3 state.
Only encoded payloads and bounded hash/metadata state cross the writer queue.

Queue accounting charges allocated payload capacity and per-item overhead,
including zero-compressed frames. The encoded queue holds at most two ready
encoding units, with a byte bound of `2 * (range_bytes + 8192)`: approximately
32 MiB for the normal 16 MiB frame. It is not a fraction of RAM. Even highly
compressible frames cannot accumulate by the thousands. Active/read/reordering
jobs together are limited to the current source-worker count, rather than twice
that count. Consumers signal freed slots and producers replenish only consumed
work; a slow destination cannot grow the queue into gigabytes of anonymous RAM.

Automatic live v2 processing has a worst-case safety ceiling of at most 3 GiB
and leaves at least 512 MiB of startup available memory outside that ceiling.
The ceiling covers active encoder/raw/reordering workspaces, descriptors, both
queues and readback buffers; it is not a preallocation or a division of cache
between stages. Actual buffers are allocated only for outstanding work. Explicit
legacy/fixed limits remain caps. Selection reserves all bounded workspaces
together and reduces concurrency if needed. Spare memory remains available to
the OS destination page cache without an application prefetch quota consuming it.
Older serialized v3 plans keep their
historical validation rules; resuming may lower the effective producer ceiling
to fit the new queue without rewriting the archive plan. Queue pressure stops
prefetch until memory becomes available and is not itself an I/O error.

Writes continue to use the ordinary Linux page cache. RAM size alone does not
promise any particular amount of unsynced data: the processing limit, encoded
queue limit and kernel writeback limits all matter. Automatic copies do not
wait for intermediate fsyncs; the storage system may still throttle writes when
dirty-page limits are reached. The retained filesystem fd lets the final sync
detect earlier writeback errors. Legacy contiguous plans retain checkpoint fsyncs.

After a source range is read into its owned raw buffer, a best-effort
`POSIX_FADV_DONTNEED` hint releases clean source-cache pages. It does not change
source bytes, drop destination cache, flush output or require kernel tuning.
The kernel may ignore the hint; dirty source pages in ordinary-file tests are
not forcibly discarded. Source selection and all data/hash coverage are unchanged.

Live status and once-per-probe-cycle `memory_sample` log records include process
RSS, MemAvailable, Cached, Dirty, Writeback, AnonPages, the processing limit and
encoded queue usage/limit. Cached and Dirty overlap and must not be added as
independent allocations. MemAvailable includes reclaimable cache, not only free
pages. These observations distinguish a large anonymous prefetch queue from
normal kernel writeback throttling; a stalled userspace write alone does not
prove either one. The observer never changes host or guest writeback limits.
See the [kernel VM documentation](https://kernel.org/doc/html/latest/admin-guide/sysctl/vm.html)
and [posix_fadvise manual](https://man7.org/linux/man-pages/man2/posix_fadvise.2.html).

`read_ahead` job events report actual completed source reads, queued encoded
bytes and queue budget. Read-ahead does not advance durable/verified progress
or completion. GUI read counters retain these observations when an older writer
snapshot arrives; a different backup ID cannot change the current counter.
These live samples update RAM status rather than requiring an additional NTFS
fsync for each sample. Lifecycle, tuning and failure records retain their normal
persistent logging. The independent failure file includes the latest matching
progress snapshot when available.

Pausing waits for active source/writer/checker I/O to become quiescent. Encoded
frames can remain in bounded RAM and accepted data in the OS cache; automatic
copy does not promise physical durability at pause. Resume means continuing this
paused running process. LiveISO does not automatically resume after power loss.
Cancellation releases backpressure and leaves incomplete files without a
COMMITTED marker; the ordinary-file development adapter retains its explicit
validated recovery path without promising power-loss-safe intermediate state.

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
The supplied failure record identifies `DIAGNOSTIC_LOG_STALLED`: destination
logging did not drain within the former ten-second queue deadline. That queue
failure is distinct from the underlying slow I/O. The older measurements cannot
identify whether NTFS metadata, kernel writeback or device-internal work caused
that wait. Native tests and VM observations are recorded separately and do not
establish a physical-device reproduction or a physical USB throughput fix.


## Independent diagnostic collection and temporary I/O observer

A destination log mirror runs separately from RAM log collection and the backup
workers. Producers never wait for NTFS logging. Both queues are bounded. The
destination backlog is capped by the configured RAM log capacity and 8192
records. If it overflows, retain recent records, increment `dropped_records`,
and write an explicit `log_gap` when output resumes. Backpressure alone does not
raise WORKER_FAILED; actual log I/O errors are still reported. Final/error close
drains the mirror. Under CLI/Live supervision, real destination I/O progress and
the configured stall policy govern that wait, rather than a separate ten-second
log-queue deadline. Standalone unsupervised library controls remain bounded.

The temporary `diagnostics.io_observer` boolean defaults to `true` in this
investigation build. Set it to `false` to disable the additional process. It is
additive to runtime v2 and advertised as `diagnostics.io_observer`; it is not a
network service. See `config/crispawn/examples/live-v2-usb-io-diagnostics.json`.

Worker threads publish active operations and cumulative inclusive durations to
a fixed RAM mapping. A separate `crispawn-live observe-io` process samples once
per second into an 8 MiB rolling RAM history at `/run/crispawn/io-observer/`.
The process does not write to NTFS during copying. Normal continuous logs also
receive its latest sample periodically. Requested byte counts and inclusive wall
times are not completed media bytes or CPU utilization; nested durations overlap.

Samples distinguish source reads, BLAKE3, compression, writer-queue/receipt waits,
directory checks, file creation, data writes, stored-byte readback, descriptor
publication, rename, log append, free-space checks, file sync and filesystem sync.
They include worker/observer PIDs, active TIDs, elapsed operation time, task state,
`wchan`, syscall, and up to two bounded kernel stacks for long-running operations.
The sample also records process CPU/I/O counters, dirty/writeback memory, actual
writeback-limit settings, I/O pressure, partition I/O counters and available USB
protocol/speed/queue attributes. Permission-denied or unavailable kernel details
are recorded; the observer does not alter Secure Boot, kernel restrictions,
writeback settings, the device or firmware. NAND-internal garbage collection
cannot be conclusively identified without device telemetry.

On normal completion, the full retained trace is copied under the generated
`crispawn/logs/<job>-complete-<suffix>/io-observer/` folder. `final.json` and
`finished.json` record the observer's final snapshot and normal exit before
unmount. Failure persistence independently attempts to include the latest RAM
sample in `failure.json`, alongside a 16 KiB kernel-message tail, with a bounded
64 KiB failure record. RAM-only evidence cannot survive an external power cut.

Native tests block the actual continuous-log write syscall for more than ten
seconds, complete a real ordinary-file copy in parallel, then test full-history
recovery and bounded backlog overflow. QEMU evidence is recorded separately;
neither native fixtures nor virtual disks prove a physical USB throughput fix.
