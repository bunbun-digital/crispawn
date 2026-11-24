# Segmented compressed storage

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

The first physical part is at most 1 MiB, subject to the available-memory cap
and an earlier logical-stream end. Measure its complete service time, including
write, file/directory synchronization, BLAKE3 readback and descriptor persistence.
Separate write/check timings, sample bytes and elapsed time are recorded.

For measured sample bytes `B` and service seconds `T`, select:

```text
baseline = align_down(clamp(B / max(T, 0.001) * 4 seconds,
                            64 KiB, memory-dependent cap), 4 KiB)
```

Four seconds provides margin toward the user's **soft five-second target**.
The cap is at most 16 MiB per part and is lower on memory-limited plans. Keep
this first-file baseline for the copy; do not waste another trial copy. A
pending buffer is also flushed when its age reaches five seconds and the
writer next reaches a safe boundary. Logical tails can be smaller.

Device firmware, kernel writeback, retries, metadata operations and changing
media performance can exceed five seconds. This is not a real-time guarantee
or permission to call an operation complete while it is still pending. The
existing no-progress supervisor and configured failure policy still apply.

## Storage confirmation

For each physical part:

1. Compute BLAKE3 from the encoded bytes in RAM, before writing.
2. Create a unique file, write and synchronize it and its directory.
3. Read its complete bytes back, recompute BLAKE3 and compare the expected hash.
4. Persist the descriptor mapping before acknowledging the logical checkpoint.

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

The physical writer and its readback are currently serial: the reported write
parallelism is **1**, independently of parallel source reads, compression and
the bounded multicore BLAKE3 pool. This is the actual implementation, not an
inferred CPU count.

GUI/CUI show the pending encoded block estimate, current file MB, baseline MB,
writer concurrency, source MB/s and synced-write MB/s. Pending blocks are
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
Credit is published after readback and descriptor persistence, never merely
because data is queued or written. Resume credits already checked prefixes once.
This is a progress conversion, not a claim that partial compressed data has been
decompressed and checked against the original. Actual compressed byte counts
remain in diagnostic telemetry. A RAM observer refreshes phases
and elapsed wait time independently of blocked writer calls. It does not invent
completed bytes or extend the I/O deadline from a UI heartbeat alone.

Only block completions, calibration and five-second wait milestones are mirrored
to normal persistent logs; the 250 ms display samples do not each force another
USB synchronization. Low-space checks, I/O retries, failure persistence and
pause/cancel handling remain enabled. Existing signed boot bytes, USB exclusions
and NTFS dirty-volume refusal are unchanged.

## Limits and validation

Staging, active-part receipts, decoder/encoder workspaces and the productive queue
are reserved together. Each descriptor is at most 8 MiB and contains at most
32,768 parts. Physical files remain below the product's 2,000,000,000-byte ceiling.

Native ordinary-file tests cover split frames, exact restore, encoded-byte
corruption, interrupted resume, retained part files, bounded calibration,
pending-block arithmetic and observer activity during a held write. These are
software tests, not evidence of USB throughput or new VM/physical acceptance.
