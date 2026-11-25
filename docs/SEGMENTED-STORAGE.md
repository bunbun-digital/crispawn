# Segmented compressed storage

Copy-time stored-byte readback is enabled by default. Runtime settings may
explicitly set `processing.verify_after_write` to `false`. In that mode, part
hashes and original-range hashes are still computed, but no newly written part
payload is reread during copying and confirmed counters remain zero. Structural
receipts, disk coverage checks and final filesystem synchronization still apply.
The `COMMITTED` marker and job report record `verification_kind: "skipped"`.
This records a completed unchecked copy. Explicit archive verification and
restoration still validate the recorded hashes and restored target. This additive
commit value requires a compatible new reader; older ISOs reject it. Existing
checked archives and their commit values remain readable without rewriting them.

New automatic-copy plans set `segmented_storage: true`. The setting is an
explicit extension to the stored plan, separate from the unchanged Builder
media layout v2 and runtime configuration v2. Old plans omit it and retain
their contiguous-file behavior. Older ISOs reject the new plan field; use this
ISO or a newer compatible version to restore a newly segmented backup.

## Logical data and physical files

The logical PBK3 stream and the source-range BLAKE3 are retained. Compression
frames still describe contiguous original ranges and may use zstd, gzip, xz,
raw or zero. Physical storage boundaries can fall **inside** a compressed
frame. Read size, compression-frame size and storage-file size are independent.

In this layout a `data/<id>.pbk` file is a bounded JSON descriptor, rather than
the compressed payload itself. Its `kind` is `crispawn.segmented_file` and its
`schema_version` is 1. It records `backup_id`, `file_id`, logical stream `bytes`
and an ordered `parts` array. Each part records:

| Field | Meaning |
| --- | --- |
| `path` | `segments/<file-id>/<unique-id>.bin`, relative to `data/` |
| `stream_offset` | Start in the logical compressed stream |
| `bytes` | Complete physical-file length |
| `used` | Number of bytes contributing to this logical stream |
| `blake3` | Standard BLAKE3 of the complete physical file, calculated before writing |

Normal parts use all their bytes. Interrupted resume may retain a prefix of an
already checked part (`used <= bytes`); original files are kept. Appended parts
start exactly at the next logical offset. Restore reads across part boundaries
without creating a temporary reassembled full file.

`files.csv` retains each logical source offset, length, framing information and
`raw_blake3`, and adds `storage_descriptor_blake3`. The latter covers the exact
descriptor bytes, including its part hashes and offsets. The existing index
checksums and COMMITTED identity protect the index/plan/metadata. A logical
stream is not an independently stored payload file; the descriptor and all
referenced parts are required together.

## Calibration and size limits

The first physical part is at most 1 MiB. A very fast cached admission does
not increase this baseline: copying into RAM is not USB media throughput.
There is no separate disposable write trial.

While copying, completed partition-sector counters provide a rolling write-rate
estimate. Divide this rate between the two writers and apply the soft sizing rule:

```text
baseline = align_down(clamp(per_writer_bytes_per_second * 4 seconds,
                            64 KiB, memory-dependent cap), 4 KiB)
```

Without usable device counters, substantial part-service samples of at least
256 KiB and 50 ms provide a fallback. Rates are smoothed, growth is limited to
2x per update, and slow samples can reduce the target. The first calibration
bytes/time remain visible separately from the changing baseline. The ceiling
is at most 16 MiB and can be lower on memory-limited plans. This is a scheduling
estimate, not a measurement of physical durability or an absolute five-second
completion guarantee. Already queued files retain their actual sizes.
A pending buffer is also flushed at the next safe boundary after five seconds;
logical tails can be smaller.

Device firmware, kernel writeback, retries, metadata operations and changing
media performance can exceed five seconds. This is not a real-time guarantee
or permission to call an operation complete while it is still pending. The
existing no-progress supervisor and configured failure policy still apply.

## Storage confirmation

For each physical part:

1. Compute BLAKE3 from the encoded bytes in RAM, before writing.
2. Create a unique file and write its bytes through the ordinary OS page cache.
3. Read its complete bytes back, recompute BLAKE3 and compare the expected hash.
4. Retain the checked mapping in source-stream order. Batch its descriptor publication.

Two writer/checker threads consume a bounded queue of four parts. A slow first
part does not prevent the second writer from working. Each stream's outstanding
receipts are also bounded; out-of-order completions cannot create a hole in its
in-memory mapping. The queue is fed without waiting for each individual part
to finish, subject to available encoded data, memory, pause/cancel and error checks.

During productive copying there is no per-part, per-descriptor or per-journal
`fsync`. Persistent logs continue appending; their per-record/periodic durability
sync is suspended during copying and forced on final/error close. This avoids
another device flush from each progress record or log rotation.

Descriptor JSON is replaced after eight accepted parts or two seconds at a
safe receipt boundary, and always by an explicit stream sync/checkpoint/seal.
This reduces NTFS file creation, replacement and directory-index updates. There
is no new on-disk format. In-process checked/source credit can use the validated
in-memory mapping; it does not promise intermediate power-loss recovery. Every
mapping is published before a checkpoint/seal and the final COMMITTED barrier.
Directory creation is attempted only when a parent is actually absent; existing
parents are still checked for non-directory/symlink substitution.

After all writers, checks, descriptors, indexes and journal updates are finished,
perform one `syncfs` barrier before creating COMMITTED. Its filesystem descriptor
is retained from **before** writes, so it can observe earlier writeback errors;
opening a new descriptor only at the end could miss those errors. COMMITTED is
then atomically written and its file/directory synced. A writeback EIO fails the
copy and is never retried into apparent success. Log/session finalization and
normal unmount still complete before the poweroff countdown.

Intermediate accepted/checked bytes are not yet power-loss-durable. LiveISO does
not automatically resume a copy after power loss. The ordinary-file development
adapter retains explicit prefix recovery, without a new power-loss guarantee.

There is **no decompression or original-data comparison during normal backup
confirmation**. Matching stored bytes proves that the encoded data was retained;
it does not independently test the compressor/decoder implementation. Reads use
the normal Linux cache, so this is not guaranteed cache-bypassing physical-media
verification. The original-data hash remains available for restored-target
readback. Explicit interrupted resume retains the existing source/prefix
validation needed to reconstruct hashing state.

The final COMMITTED marker requires receipts for every referenced part and
unchanged descriptors/part identities. Receipts are compacted after each logical
stream seals, so a large backup does not retain every part identity in RAM.
There is no final full-payload reread or decompression phase. Unsupported paths,
truncated parts, a BLAKE3 mismatch, changed files and missing confirmations fail
the operation. Unreferenced parts left by interruption are preserved and are
not silently adopted by a new descriptor.

## Progress and controls

There are **two** physical-file writer/checker threads, independently of source
read/encoder concurrency and the bounded multicore BLAKE3 pool. The UI reports
both configured writer concurrency (2) and currently active writers (0–2).
Startup, tails or insufficient input may use fewer than two active workers;
dummy writes are not generated to fill an empty queue.

GUI/CUI show the pending encoded block estimate, current file MB, baseline MB,
writer concurrency, source MB/s and cached-write MB/s. Pending blocks are
`ceil((encoded queue + current input + pending storage buffers) / baseline)`;
source *uncompressed* read bytes are not used in that division. Rates are recent
one-second samples, not the initial RAM-prefetch rate averaged over the whole copy.
Elapsed copy time is displayed as hours, minutes and seconds beside the ETA;
it includes pauses and continues updating during an I/O wait.

The confirmation bar uses the original source disk size as its denominator,
like the source-copy bar. Each compressed frame with `R` original bytes and `C`
payload bytes earns `floor(R * checked_payload_bytes / C)` source-byte equivalents.
Accumulate the differences at each checked physical part, separately for each
frame, so changing compression ratios and rounding do not distort or lose progress.
For example, 100 MB compressed to 10 MB earns 10 MB of source-equivalent progress
when 1 MB is stored and checked. File headers/footers do not earn progress; a
zero frame earns its source range when its payload-free frame header is checked.
Credit is published after readback and ordered in-memory mapping, never merely
because data is queued or written. Resume credits already checked prefixes once.
This is a progress conversion, not a claim that partial compressed data has been
decompressed and checked against the original. Actual compressed byte counts
remain in diagnostic telemetry. A RAM observer refreshes phases
and elapsed wait time independently of blocked writer calls. It does not invent
completed bytes or extend the I/O deadline from a UI heartbeat alone.

Only block completions, calibration and five-second wait milestones are mirrored
to normal persistent logs; the 250 ms display samples do not each force another
record. Low-space checks, I/O retries, failure persistence and
pause/cancel handling remain enabled. Existing signed boot bytes, USB exclusions
and NTFS dirty-volume refusal are unchanged.

## Limits and validation

Staging, six queued/active part buffers, active-part receipts, encoder workspaces and the productive queue
are reserved together. Each descriptor is at most 8 MiB and contains at most
32,768 parts. Physical files remain below the product's 2,000,000,000-byte ceiling.

The descriptor reader uses this same 8 MiB limit during copy sealing, explicit
verification, recovery and restoration. Generic JSON metadata retains its 64 KiB
limit. Earlier readers incorrectly applied that generic limit to descriptors,
so a legitimate descriptor larger than 64 KiB could stop copying with
`LIMIT_EXCEEDED: metadata file too large`. Rejections now include the path,
observed length and applicable limit. This is a reader correction; the stored
format, descriptor BLAKE3 and part-count/size limits are unchanged.

Native ordinary-file tests cover split frames, exact restore, encoded-byte
corruption, explicit interrupted recovery, retained part files, bounded calibration,
pending-block arithmetic and two active held writers plus a queued part. An
isolated Linux child traces the actual sync syscalls: no part/descriptor fsync,
no per-progress/log-rotation flush, a final syncfs before COMMITTED, and failure
without COMMITTED when that barrier returns EIO. These are
software tests, not evidence of USB throughput or new VM/physical acceptance.
