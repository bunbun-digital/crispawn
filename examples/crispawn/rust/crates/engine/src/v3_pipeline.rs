// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 bunbun-digital. Licensed under Apache-2.0.
//! Productive read/encode/hash is independent of destination write/fsync latency.
//! Only compressed payloads and per-file BLAKE3 state cross the byte-bounded queue.
use super::*;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
};

struct EncodedFrame {
    range: u64,
    raw_bytes: u64,
    encoded: codec::Encoded,
    hash_after: blake3::Hasher,
}
enum Work {
    Ready(Ready),
    Deferred(crate::online::Profile, u64),
}
impl EncodedFrame {
    fn cost(&self) -> u64 {
        self.encoded.payload.capacity() as u64 + 8192
    }
}
#[derive(Default)]
struct State {
    frames: VecDeque<EncodedFrame>,
    bytes: u64,
    encoded_bytes: u64,
    done: bool,
    error: Option<Error>,
}
struct Queue {
    state: Mutex<State>,
    wake: Condvar,
    stopped: AtomicBool,
    failed: AtomicBool,
    read: AtomicU64,
    active_tasks: AtomicU64,
    resume_epoch: AtomicU64,
    budget: u64,
    storage: Option<Arc<storage::Shared>>,
}
struct StopOnDrop<'a>(&'a Queue);
impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.stop();
    }
}
enum Received {
    Frame(EncodedFrame),
    Tick,
    Done,
}
impl Queue {
    fn new(budget: u64, read: u64) -> Self {
        Self {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            read: AtomicU64::new(read),
            active_tasks: AtomicU64::new(0),
            resume_epoch: AtomicU64::new(0),
            budget,
            storage: None,
        }
    }
    fn push(&self, frame: EncodedFrame, control: &dyn ExecutionControl) -> Result<()> {
        let cost = frame.cost();
        require(
            cost <= self.budget,
            "MEMORY_BUDGET",
            "encoded frame exceeds prefetch budget",
        )?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.frames.len() >= 2 || state.bytes + cost > self.budget {
            control.check_cancel()?;
            require(
                !self.stopped.load(Ordering::Acquire),
                "CANCELLED",
                "writer stopped",
            )?;
            state = self
                .wake
                .wait_timeout(state, Duration::from_millis(25))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        require(
            !self.stopped.load(Ordering::Acquire),
            "CANCELLED",
            "writer stopped",
        )?;
        state.bytes += cost;
        state.encoded_bytes += format::FRAME_HEADER as u64 + frame.encoded.payload.len() as u64;
        if let Some(s) = &self.storage {
            s.queue_bytes.store(state.encoded_bytes, Ordering::Relaxed);
        }
        state.frames.push_back(frame);
        self.wake.notify_all();
        Ok(())
    }
    fn take(&self) -> Result<Received> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.frames.is_empty() && !state.done && state.error.is_none() {
            state = self
                .wake
                .wait_timeout(state, Duration::from_millis(25))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if let Some(error) = state.error.take() {
            return Err(error);
        }
        if let Some(frame) = state.frames.pop_front() {
            state.bytes -= frame.cost();
            state.encoded_bytes -= format::FRAME_HEADER as u64 + frame.encoded.payload.len() as u64;
            if let Some(s) = &self.storage {
                s.queue_bytes.store(state.encoded_bytes, Ordering::Relaxed);
            }
            self.wake.notify_all();
            return Ok(Received::Frame(frame));
        }
        Ok(if state.done {
            Received::Done
        } else {
            Received::Tick
        })
    }
    fn complete(&self, result: Result<()>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.done = true;
        if state.error.is_none() {
            state.error = result.err();
        }
        self.failed.store(state.error.is_some(), Ordering::Release);
        self.wake.notify_all();
    }
    fn remember_failure(&self, error: &Error) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.error.is_none() {
            state.error = Some(Error::new(error.code, error.detail.clone()));
        }
        self.failed.store(true, Ordering::Release);
        self.wake.notify_all();
    }
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.wake.notify_all();
    }
}
struct ProducerControl<'a> {
    base: &'a dyn ExecutionControl,
    queue: &'a Queue,
}
impl ExecutionControl for ProducerControl<'_> {
    fn verify_after_write(&self) -> bool {
        self.base.verify_after_write()
    }
    fn cancelled(&self) -> bool {
        self.queue.stopped.load(Ordering::Acquire)
            || self.queue.failed.load(Ordering::Acquire)
            || self.base.cancelled()
    }
    fn pause_requested(&self) -> bool {
        self.base.pause_requested()
    }
    fn verification_workers(&self) -> u32 {
        self.base.verification_workers()
    }
    fn configure_verification_workers(&self, n: u32) {
        self.base.configure_verification_workers(n)
    }
    fn wait_for_resume(&self) -> Result<()> {
        while self.pause_requested() {
            self.check_cancel()?;
            std::thread::sleep(Duration::from_millis(25));
        }
        self.check_cancel()
    }
}
struct Region {
    hash: blake3::Hasher,
    end: u64,
}
#[derive(Clone, Copy)]
struct WriteFeedback {
    seconds_per_stored: f64,
    backlog: u64,
    verify_bytes: u64,
    verify_seconds: f64,
}

fn produce(
    source: &File,
    meta: &Metadata,
    plan: &Plan,
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    base: &dyn ExecutionControl,
    pool: &CpuPool,
    queue: &Queue,
    mut next: Vec<u64>,
    mut regions: Vec<Region>,
    writer: &Mutex<WriteFeedback>,
) -> Result<()> {
    let control = ProducerControl { base, queue };
    let count = plan.range_count(meta.source.bytes);
    let mut schedule = Schedule::from_ends(count, plan.regions);
    let cpus = std::thread::available_parallelism()
        .map(|v| v.get() as u32)
        .unwrap_or(1);
    let mut feedback = crate::online::Feedback::new(plan, control.verification_workers(), cpus);
    if plan.segmented_storage {
        feedback.verifiers = crate::storage_worker::WORKERS;
        control.configure_verification_workers(feedback.verifiers);
    }
    let gate = Arc::new(crate::online::ReadGate::new(
        feedback.active.workers,
        plan.workers,
    ));
    events.settings(
        &feedback.active,
        feedback.verifiers,
        meta.source.bytes,
        "productive compressed-prefetch pipeline; writer durability runs independently",
    );
    let (sender, receiver) = mpsc::channel::<(u64, Result<Work>)>();
    let mut deferred = VecDeque::new();
    let mut pending: Vec<BTreeMap<u64, Ready>> =
        (0..regions.len()).map(|_| BTreeMap::new()).collect();
    let mut outstanding = 0usize;
    let mut exhausted = false;
    let mut paused = false;
    let mut resume_epoch = 0;
    let mut last_prefetch = std::time::Instant::now() - Duration::from_secs(1);
    pool.pool().in_place_scope(|scope| -> Result<()> {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
            loop {
                control.check_cancel()?;
                if paused && !control.pause_requested() {
                    feedback.resumed();
                    paused = false;
                }
                while (!exhausted || !deferred.is_empty())
                    && outstanding < feedback.active.workers as usize
                    && !control.pause_requested()
                {
                    let (range, saved) = if let Some((range, profile, read_bytes))=deferred.pop_front() {
                        (range,Some((profile,read_bytes)))
                    } else {
                        match schedule.next() {Some(range)=>(range,None),None=>{exhausted=true;break;}}
                    };
                    let lane = region_of(range, count, regions.len() as u64);
                    if range < next[lane] {
                        continue;
                    }
                    let (profile, read_bytes) = saved.unwrap_or_else(||feedback.next(lane));
                    let send = sender.clone();
                    let gate = gate.clone();
                    let control = &control;
                    outstanding += 1;
                    let activity = crate::control::Activity::new(&queue.active_tasks);
                    scope.spawn(move |_| {
                        let _activity = activity;
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                            || -> Result<Work> {
                                let _permit = gate.acquire(control)?;
                                control.check_cancel()?;
                                if control.pause_requested() {
                                    return Ok(Work::Deferred(profile,read_bytes));
                                }
                                let (offset, length) = plan.range(meta.source.bytes, range)?;
                                hooks.at("source_read")?;
                                let mut raw = vec![0; length as usize];
                                let start = std::time::Instant::now();
                                let source_read_trace = crate::io_trace::span(crate::io_trace::Op::SourceRead, length);
                                io::read_exact_at(source, &mut raw, offset, read_bytes).map_err(|mut error| {
                                    error.detail = format!("source read: offset={offset} length={length} request_bytes={read_bytes}: {}", error.detail);
                                    error
                                })?;
                                let read_seconds = start.elapsed().as_secs_f64();
                                drop(source_read_trace);
                                io::release_source_cache(source, offset, length);
                                let read=queue.read.fetch_add(length,Ordering::Relaxed)+length;
                                if let Some(s)=&queue.storage {s.source_read.store(read,Ordering::Relaxed);}
                                hooks.at("codec_encode")?;
                                let start =
                                    crate::online::cpu_seconds(libc::CLOCK_THREAD_CPUTIME_ID);
                                let encoded =
                                    codec::encode(raw.clone(), profile.codec, profile.level)?;
                                let observation = crate::online::Observation {
                                    lane,
                                    requested: profile,
                                    actual: encoded.codec,
                                    raw: length,
                                    stored: encoded.payload.len() as u64
                                        + format::FRAME_HEADER as u64,
                                    encode_cpu: crate::online::cpu_seconds(
                                        libc::CLOCK_THREAD_CPUTIME_ID,
                                    ) - start,
                                    read_seconds,
                                    read_bytes,
                                    writer_seconds: 0.0,
                                };
                                hooks.at("after_codec_encode")?;
                                Ok(Work::Ready(Ready {
                                    range,
                                    raw,
                                    encoded,
                                    observation,
                                }))
                            },
                        ))
                        .unwrap_or_else(|_| {
                            Err(Error::new(
                                "ENGINE_PANICKED",
                                "productive read/compression worker panicked",
                            ))
                        });
                        let _ = send.send((range, result));
                    });
                }
                if outstanding == 0 {
                    if exhausted && deferred.is_empty() {
                        let queued = queue.state.lock().unwrap_or_else(|e| e.into_inner()).bytes;
                        events.prefetch(
                            &meta.backup_id,
                            queue.read.load(Ordering::Relaxed),
                            queued,
                            queue.budget,
                        );
                        return Ok(());
                    }
                    if control.pause_requested() {
                        paused = true;
                        std::thread::sleep(Duration::from_millis(25));
                        continue;
                    }
                }
                match receiver.recv_timeout(Duration::from_millis(25)) {
                    Ok((range, result)) => {
                        let ready = match result? {
                            Work::Ready(ready)=>ready,
                            Work::Deferred(profile,read_bytes)=>{
                                deferred.push_back((range,profile,read_bytes));
                                outstanding-=1;continue;
                            }
                        };
                        let lane = region_of(range, count, regions.len() as u64);
                        require(
                            pending[lane].insert(range, ready).is_none(),
                            "DUPLICATE_RANGE",
                            "productive source range",
                        )?;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {},
                    Err(_) => {
                        return Err(Error::new(
                            "ENGINE_EXITED",
                            "productive worker channel ended",
                        ))
                    }
                }
                for lane in 0..regions.len() {
                    if control.pause_requested() { break; }
                    while let Some(mut ready) = pending[lane].remove(&next[lane]) {
                        if control.pause_requested() {
                            pending[lane].insert(ready.range,ready);
                            break;
                        }
                        let record_bytes =
                            format::FRAME_HEADER as u64 + ready.encoded.payload.len() as u64;
                        let region = &mut regions[lane];
                        if region.end + record_bytes + format::FOOTER as u64 > plan.container_bytes
                        {
                            region.end = format::FILE_HEADER as u64;
                            region.hash = blake3::Hasher::new();
                        }
                        pool.update(&mut region.hash, &ready.raw);
                        region.end += record_bytes;
                        let raw_bytes = ready.raw.len() as u64;
                        // Drop original bytes before crossing the producer/writer queue.
                        drop(ready.raw);
                        let observed = *writer.lock().unwrap_or_else(|e| e.into_inner());
                        let resumed=queue.resume_epoch.load(Ordering::Acquire);
                        if resumed!=resume_epoch {feedback.resumed();resume_epoch=resumed;}
                        ready.observation.writer_seconds =
                            observed.seconds_per_stored * ready.observation.stored as f64;
                        feedback
                            .verification_service(observed.verify_bytes, observed.verify_seconds);
                        if let Some(reason) = feedback.observe(&ready.observation, observed.backlog)
                        {
                            if plan.segmented_storage {feedback.verifiers=crate::storage_worker::WORKERS;}
                            control.configure_verification_workers(feedback.verifiers);
                            gate.set(feedback.active.workers);
                            events.settings(
                                &feedback.active,
                                feedback.verifiers,
                                meta.source.bytes,
                                &reason,
                            );
                        }
                        ready.encoded.payload.shrink_to_fit();
                        let frame = EncodedFrame {
                            range: ready.range,
                            raw_bytes,
                            encoded: ready.encoded,
                            hash_after: region.hash.clone(),
                        };
                        let cost = frame.cost();
                        queue.push(frame, &control)?;
                        if last_prefetch.elapsed() >= Duration::from_secs(1) {
                            let queued =
                                queue.state.lock().unwrap_or_else(|e| e.into_inner()).bytes;
                            events.prefetch(
                                &meta.backup_id,
                                queue.read.load(Ordering::Relaxed),
                                queued,
                                queue.budget,
                            );
                            last_prefetch = std::time::Instant::now();
                        }
                        hooks.span(
                            "compressed_queued",
                            plan.range(meta.source.bytes, ready.range)?.0,
                            cost,
                        )?;
                        next[lane] += 1;
                        outstanding -= 1;
                    }
                }
            }
        }))
        .unwrap_or_else(|_| {
            Err(Error::new(
                "ENGINE_PANICKED",
                "productive coordinator panicked",
            ))
        });
        if let Err(error) = &outcome {
            queue.remember_failure(error);
            queue.stop();
            crate::supervisor::failure(error);
        }
        outcome
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn copy(
    source: &File,
    root: &Path,
    meta: &Metadata,
    plan: &Plan,
    id: [u8; 16],
    events: &dyn EventSink,
    hooks: &dyn Hooks,
    control: &dyn ExecutionControl,
    journal: &mut Journal,
    open: &mut [Option<OpenFile>],
    next: &mut [u64],
    ends: &[u64],
    report: &mut Reporter<'_>,
    pool: &CpuPool,
    verification: &mut crate::inline_verify::Inline,
    storage: &Arc<storage::Shared>,
) -> Result<()> {
    let mut producer_plan = plan.clone();
    while producer_plan.workers > 1
        && crate::profiles::memory_bound(&producer_plan, control.verification_workers())
            > plan.memory_limit_bytes
    {
        producer_plan.workers -= 1;
    }
    let base_memory =
        crate::profiles::processing_workspace(&producer_plan, control.verification_workers());
    let budget = crate::profiles::queue_budget(&producer_plan, control.verification_workers());
    require(
        budget >= 2 * plan.range_bytes + 8192
            && base_memory.saturating_add(budget) <= plan.memory_limit_bytes,
        "MEMORY_BUDGET",
        "productive queue plus encoders/checking exceeds RAM",
    )?;
    let mut queue = Queue::new(budget, report.read);
    if plan.segmented_storage {
        storage.prefetch_limit(budget);
        queue.storage = Some(storage.clone());
    }
    let writer = Mutex::new(WriteFeedback {
        seconds_per_stored: 1.0 / 100e6,
        backlog: 0,
        verify_bytes: 0,
        verify_seconds: 0.0,
    });
    let initial = next.to_vec();
    let regions = open
        .iter()
        .map(|f| match f {
            Some(f) => Region {
                hash: f.hash.clone(),
                end: f.end,
            },
            None => Region {
                hash: blake3::Hasher::new(),
                end: format::FILE_HEADER as u64,
            },
        })
        .collect::<Vec<_>>();
    std::thread::scope(|scope| {
        let _stop_on_unwind = StopOnDrop(&queue);
        scope.spawn(|| {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                produce(
                    source,
                    meta,
                    &producer_plan,
                    events,
                    hooks,
                    control,
                    pool,
                    &queue,
                    initial,
                    regions,
                    &writer,
                )
            }))
            .unwrap_or_else(|_| {
                Err(Error::new(
                    "ENGINE_PANICKED",
                    "productive coordinator panicked",
                ))
            });
            if let Err(error) = &result {
                crate::supervisor::failure(error);
            }
            queue.complete(result);
        });
        let copied = (|| -> Result<()> {
            let effective = ProducerControl {
                base: control,
                queue: &queue,
            };
            let control: &dyn ExecutionControl = &effective;
            let mut space = crate::destination_space::Monitor::new(root)?;
            let mut next_file = journal.state.used.last().map(|v| v + 1).unwrap_or(0);
            let count = plan.range_count(meta.source.bytes);
            let mut cost_seconds = 0.0;
            let mut cost_bytes = 0u64;
            loop {
                control.check_cancel()?;
                verification.poll(report)?;
                report.read = queue.read.load(Ordering::Relaxed);
                if control.pause_requested() {
                    if queue.active_tasks.load(Ordering::Acquire) == 0 && verification.pause_ready()
                    {
                        for f in open.iter_mut().flatten() {
                            checkpoint(f, journal, meta, plan, report, hooks)?;
                        }
                        report.read = queue.read.load(Ordering::Relaxed);
                        crate::destination_space::gate(report, control, root, "COPYING")?;
                        queue.resume_epoch.fetch_add(1, Ordering::Release);
                    } else {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    continue;
                }
                if plan.segmented_storage {
                    for f in open.iter_mut().flatten() {
                        f.file.poll()?;
                    }
                }
                space.poll(root)?;
                let frame = match queue.take()? {
                    Received::Done => break,
                    Received::Tick => continue,
                    Received::Frame(frame) => frame,
                };
                // take() can wait while the producer completes more reads.
                // Never publish durable bytes against the pre-wait read count.
                report.read = queue.read.load(Ordering::Relaxed);
                let started = std::time::Instant::now();
                let region = region_of(frame.range, count, open.len() as u64);
                require(
                    frame.range == next[region],
                    "INVALID_STATE",
                    "productive source order",
                )?;
                let offset = plan.range(meta.source.bytes, frame.range)?.0;
                let record_bytes = format::FRAME_HEADER as u64 + frame.encoded.payload.len() as u64;
                if open[region].as_ref().is_some_and(|f| {
                    f.end + record_bytes + format::FOOTER as u64 > plan.container_bytes
                }) {
                    let entry = seal(
                        open[region].take().unwrap(),
                        root,
                        journal,
                        meta,
                        plan,
                        id,
                        report,
                        hooks,
                        storage,
                    )?;
                    if !plan.segmented_storage {
                        verification.submit(entry, report, control)?;
                    }
                }
                if open[region].is_none() {
                    open[region] = Some(new_file(
                        root,
                        region,
                        offset,
                        &mut next_file,
                        journal,
                        meta,
                        plan,
                        id,
                        report,
                        hooks,
                        storage,
                    )?);
                }
                let f = open[region].as_mut().unwrap();
                require(
                    f.offset + f.raw == offset
                        && f.end + record_bytes + format::FOOTER as u64 <= plan.container_bytes,
                    "INVALID_STATE",
                    "productive extent or file bound",
                )?;
                if plan.segmented_storage {
                    storage.input(record_bytes);
                }
                hooks.at("before_data_write")?;
                hooks.at("destination_space_check")?;
                crate::destination_space::check(root)?;
                storage::write_frame(
                    &mut f.file,
                    &format::Frame {
                        codec: frame.encoded.codec,
                        level: frame.encoded.level,
                        raw_bytes: frame.raw_bytes,
                        payload_bytes: frame.encoded.payload.len() as u32,
                    },
                    plan.format_version,
                    &frame.encoded.payload,
                )?;
                f.raw += frame.raw_bytes;
                f.frames += 1;
                f.end += record_bytes;
                f.hash = frame.hash_after;
                f.profiles.insert(format::profile_name(
                    frame.encoded.codec,
                    frame.encoded.level,
                ));
                report.stored += record_bytes;
                hooks.at("after_data_write")?;
                if !plan.segmented_storage && f.frames % u64::from(plan.checkpoint_records) == 0 {
                    checkpoint(f, journal, meta, plan, report, hooks)?;
                }
                next[region] += 1;
                if next[region] == ends[region] {
                    let entry = seal(
                        open[region].take().unwrap(),
                        root,
                        journal,
                        meta,
                        plan,
                        id,
                        report,
                        hooks,
                        storage,
                    )?;
                    if !plan.segmented_storage {
                        verification.submit(entry, report, control)?;
                    }
                }
                cost_seconds += started.elapsed().as_secs_f64();
                cost_bytes += record_bytes;
                let mut feedback = writer.lock().unwrap_or_else(|e| e.into_inner());
                if cost_seconds >= 0.5 && cost_bytes >= 256 << 10 {
                    feedback.seconds_per_stored = cost_seconds / cost_bytes as f64;
                    cost_seconds = 0.0;
                    cost_bytes = 0;
                }
                feedback.backlog = if plan.segmented_storage {
                    0
                } else {
                    report.durable.saturating_sub(report.verified)
                };
                (feedback.verify_bytes, feedback.verify_seconds) = verification.service();
            }
            Ok(())
        })();
        let copied = if copied
            .as_ref()
            .is_err_and(|error| error.code == "CANCELLED")
        {
            match queue
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .error
                .take()
            {
                Some(error) => Err(error),
                None => copied,
            }
        } else {
            copied
        };
        if let Err(error) = &copied {
            crate::supervisor::failure(error);
        }
        queue.stop();
        copied
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame() -> EncodedFrame {
        EncodedFrame {
            range: 0,
            raw_bytes: 16 << 20,
            encoded: codec::Encoded {
                codec: crispawn_core::types::Codec::Zstd,
                level: 3,
                payload: vec![1; 64],
            },
            hash_after: blake3::Hasher::new(),
        }
    }
    #[test]
    fn queue_charges_compressed_capacity_and_hash_state_and_bounds_backpressure() {
        let cost = frame().cost();
        let mut queue = Queue::new(cost, 0);
        let storage = storage::Shared::new(&"a".repeat(32), 1).unwrap();
        storage.prefetch_limit(cost);
        queue.storage = Some(storage.clone());
        let cancel = AtomicBool::new(false);
        queue.push(frame(), &cancel).unwrap();
        assert_eq!(queue.state.lock().unwrap().bytes, cost);
        assert!(storage.snapshot().prefetch_encoded_bytes > 0);
        assert!(cost < (16 << 20));
        let finished = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let done = &finished;
            let queue = &queue;
            let cancel = &cancel;
            let child = scope.spawn(move || {
                queue.push(frame(), cancel).unwrap();
                done.store(true, Ordering::Release);
            });
            std::thread::sleep(Duration::from_millis(50));
            assert!(!finished.load(Ordering::Acquire));
            assert!(matches!(queue.take().unwrap(), Received::Frame(_)));
            child.join().unwrap();
            assert_eq!(queue.state.lock().unwrap().bytes, cost);
        });
        assert!(matches!(queue.take().unwrap(), Received::Frame(_)));
        assert_eq!(storage.snapshot().prefetch_encoded_bytes, 0);
        assert_eq!(storage.snapshot().prefetch_limit_bytes, cost);
    }
    #[test]
    fn stopping_writer_releases_a_full_compressed_queue() {
        let queue = Queue::new(frame().cost(), 0);
        let control = AtomicBool::new(false);
        queue.push(frame(), &control).unwrap();
        std::thread::scope(|scope| {
            let queue = &queue;
            let control = &control;
            let child = scope.spawn(move || queue.push(frame(), control).unwrap_err().code);
            std::thread::sleep(Duration::from_millis(25));
            queue.stop();
            assert_eq!(child.join().unwrap(), "CANCELLED");
        });
        assert!(Queue::new(1, 0).push(frame(), &control).is_err());
    }
    #[test]
    fn small_compressed_frames_cannot_fill_spare_ram_beyond_two_handoffs() {
        // Even a huge byte allowance must not turn highly compressed data into
        // an arbitrarily deep prefetch queue.
        let queue = Queue::new(8 << 30, 0);
        let control = AtomicBool::new(false);
        queue.push(frame(), &control).unwrap();
        queue.push(frame(), &control).unwrap();
        let (send, received) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let queue = &queue;
            let control = &control;
            let child = scope.spawn(move || {
                send.send(()).unwrap();
                queue.push(frame(), control)
            });
            received.recv_timeout(Duration::from_secs(2)).unwrap();
            std::thread::sleep(Duration::from_millis(30));
            assert_eq!(queue.state.lock().unwrap().frames.len(), 2);
            assert!(!child.is_finished());
            // Cancelling while the count limit (rather than bytes) blocks
            // must release the producer without waiting for destination I/O.
            control.store(true, Ordering::Release);
            assert_eq!(child.join().unwrap().unwrap_err().code, "CANCELLED");
        });
    }
}
