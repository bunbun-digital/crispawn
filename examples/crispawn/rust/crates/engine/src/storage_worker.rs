// SPDX-License-Identifier: Apache-2.0
//! Bounded physical-file writers. They never own their caller or its journal.
//!
//! Tasks carry a key (the physical file they append to). At most one task per
//! key runs at a time, in submission order, so appends to one NTFS file stay
//! sequential. The kernel serializes writes to one inode anyway; two writers
//! are kept busy by working on different files.
use crate::{Error, Result};
use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub(crate) const WORKERS: u32 = 2;
pub(crate) const QUEUED: usize = 4;
pub(crate) type Task = Box<dyn FnOnce() + Send>;
#[derive(Default)]
struct Queue {
    tasks: VecDeque<(u64, Task)>,
    busy: BTreeSet<u64>,
    closed: bool,
}
type Shared = Arc<(Mutex<Queue>, Condvar)>;
pub(crate) struct Pool {
    shared: Shared,
    threads: Vec<JoinHandle<()>>,
}
fn next(shared: &Shared) -> Option<(u64, Task)> {
    let (lock, wake) = &**shared;
    let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        // Earlier tasks of a key always precede later ones in the queue, so the
        // first task whose key is idle is that key's next task in order.
        if let Some(index) = q.tasks.iter().position(|(key, _)| !q.busy.contains(key)) {
            let (key, task) = q.tasks.remove(index).unwrap();
            q.busy.insert(key);
            wake.notify_all();
            return Some((key, task));
        }
        if q.closed && q.tasks.is_empty() {
            return None;
        }
        q = wake
            .wait_timeout(q, Duration::from_millis(100))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}
impl Pool {
    pub fn new() -> Result<Self> {
        let shared: Shared = Arc::default();
        let mut threads = vec![];
        for index in 0..WORKERS {
            let shared = shared.clone();
            threads.push(
                std::thread::Builder::new()
                    .name(format!("storage-write-{index}"))
                    .spawn(move || {
                        while let Some((key, task)) = next(&shared) {
                            task();
                            let (lock, wake) = &*shared;
                            lock.lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .busy
                                .remove(&key);
                            wake.notify_all();
                        }
                    })?,
            );
        }
        Ok(Self { shared, threads })
    }
    /// Queue a task behind earlier tasks of the same key. Waiting is bounded
    /// by the total number of queued (not yet running) tasks.
    pub fn submit(&self, key: u64, task: Task, check: impl Fn() -> Result<()>) -> Result<()> {
        let _trace = crate::io_trace::span(crate::io_trace::Op::QueueWait, 0);
        let (lock, wake) = &*self.shared;
        let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            check()?;
            if q.closed {
                return Err(Error::new(
                    "STORAGE_WORKER_FAILED",
                    "writer queue disconnected",
                ));
            }
            if q.tasks.len() < QUEUED {
                q.tasks.push_back((key, task));
                wake.notify_all();
                return Ok(());
            }
            q = wake
                .wait_timeout(q, Duration::from_millis(10))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        {
            let (lock, wake) = &*self.shared;
            lock.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
            wake.notify_all();
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[test]
    fn one_task_per_key_runs_in_order_while_other_keys_use_the_second_writer() {
        let pool = Pool::new().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let same_key_overlap = Arc::new(AtomicUsize::new(0));
        let active_key = Arc::new(Mutex::new(BTreeSet::new()));
        for n in 0..12u64 {
            let key = n % 2;
            let (log, running, peak, overlap, active) = (
                log.clone(),
                running.clone(),
                peak.clone(),
                same_key_overlap.clone(),
                active_key.clone(),
            );
            pool.submit(
                key,
                Box::new(move || {
                    if !active.lock().unwrap().insert(key) {
                        overlap.fetch_add(1, Ordering::Relaxed);
                    }
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    log.lock().unwrap().push((key, n));
                    running.fetch_sub(1, Ordering::SeqCst);
                    active.lock().unwrap().remove(&key);
                }),
                || Ok(()),
            )
            .unwrap();
        }
        drop(pool);
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 12);
        for key in 0..2 {
            let order: Vec<_> = log.iter().filter(|v| v.0 == key).map(|v| v.1).collect();
            assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
        }
        assert_eq!(same_key_overlap.load(Ordering::Relaxed), 0);
        assert_eq!(peak.load(Ordering::SeqCst), 2);
    }
}
