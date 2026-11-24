// SPDX-License-Identifier: Apache-2.0
//! Bounded physical-file writers. They never own their caller or its journal.
use crate::{Error, Result};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub(crate) const WORKERS: u32 = 2;
pub(crate) const QUEUED: usize = 4;
pub(crate) type Task = Box<dyn FnOnce() + Send>;
pub(crate) struct Pool {
    sender: Option<mpsc::SyncSender<Task>>,
    threads: Vec<JoinHandle<()>>,
}
impl Pool {
    pub fn new() -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Task>(QUEUED);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut threads = vec![];
        for index in 0..WORKERS {
            let receiver = receiver.clone();
            threads.push(
                std::thread::Builder::new()
                    .name(format!("storage-write-{index}"))
                    .spawn(move || loop {
                        // Release the receiver lock before performing any work.
                        let task = receiver.lock().unwrap_or_else(|e| e.into_inner()).recv();
                        match task {
                            Ok(task) => task(),
                            Err(_) => break,
                        }
                    })?,
            );
        }
        Ok(Self {
            sender: Some(sender),
            threads,
        })
    }
    pub fn submit(&self, mut task: Task, check: impl Fn() -> Result<()>) -> Result<()> {
        loop {
            check()?;
            match self.sender.as_ref().unwrap().try_send(task) {
                Ok(()) => return Ok(()),
                Err(mpsc::TrySendError::Full(returned)) => task = returned,
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    return Err(Error::new(
                        "STORAGE_WORKER_FAILED",
                        "writer queue disconnected",
                    ));
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        self.sender.take();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}
