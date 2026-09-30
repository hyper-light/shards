//! A device's worker thread: it serves its queue in bounded rounds ([`serve_round`])
//! until stopped, and waits for a notification only once a round leaves the queue idle.
//! Block and pmem requests are answered here, never on a vCPU's thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle, Thread};

use super::DeviceInterrupt;
use super::queue::{Chain, Queue, Round, serve_round};
use crate::memory::GuestMemory;
use crate::warn;

#[derive(Debug)]
pub struct Worker {
    thread: JoinHandle<Option<Queue>>,
    waker: Thread,
    stop: Arc<AtomicBool>,
}

impl Worker {
    /// Starts the worker of device `name`, serving `queue` with `answer`, which returns
    /// the bytes it wrote into each request. It serves what is there before it first
    /// waits, so requests published before it started, or while a snapshot paused it,
    /// are answered.
    pub fn start(
        name: &'static str,
        mut queue: Queue,
        memory: Arc<GuestMemory>,
        interrupt: Arc<DeviceInterrupt>,
        mut answer: impl FnMut(&Chain) -> u32 + Send + 'static,
    ) -> Result<Worker, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                while !stopping.load(Ordering::Acquire) {
                    match serve_round(&mut queue, &memory, &interrupt, &stopping, &mut answer) {
                        Ok(Round::Idle) => thread::park(),
                        Ok(Round::More) => {}
                        // A malformed ring: the device needs reset (virtio 1.3 §2.1.2).
                        Err(e) => {
                            warn!("{name}: {e}; device needs reset");
                            interrupt.fail();
                            return None;
                        }
                    }
                }
                Some(queue)
            })
            .map_err(|e| format!("spawning the {name} worker: {e}"))?;
        let waker = thread.thread().clone();
        Ok(Worker { thread, waker, stop })
    }

    /// The driver notified the queue.
    pub fn notify(&self) {
        self.waker.unpark();
    }

    /// What [`notify`](Self::notify) wakes, for a test's driver on another thread.
    #[cfg(test)]
    pub fn waker(&self) -> Thread {
        self.waker.clone()
    }

    /// Stops the worker after the request it is answering; its queue, unless the ring was
    /// malformed.
    pub fn stop(self) -> Option<Queue> {
        self.stop.store(true, Ordering::Release);
        self.waker.unpark();
        match self.thread.join() {
            Ok(queue) => queue,
            Err(_) => {
                warn!("a virtio worker ended abnormally");
                None
            }
        }
    }
}
