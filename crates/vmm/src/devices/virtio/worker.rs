//! A device's worker thread: it serves its queue in bounded rounds ([`serve_round`])
//! until stopped, and waits for a notification only once a round leaves the queue idle.
//! Block and pmem requests are answered here, never on a vCPU's thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle, Thread};

use super::DeviceInterrupt;
use super::queue::{Chain, Queue, Round, serve_round};
use crate::memory::GuestMemory;
use crate::{debug, warn};

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
    /// are answered. A worker the system refuses gives the queue back.
    pub fn start(
        name: &'static str,
        queue: Queue,
        memory: Arc<GuestMemory>,
        interrupt: Arc<DeviceInterrupt>,
        mut answer: impl FnMut(&Chain) -> u32 + Send + 'static,
    ) -> Result<Worker, (String, Queue)> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = spawn(name, queue, move |mut queue| {
            while !stopping.load(Ordering::Acquire) {
                match serve_round(&mut queue, &memory, &interrupt, &stopping, &mut answer) {
                    Ok(Round::Idle) => thread::park(),
                    Ok(Round::More) => {}
                    // A malformed ring: the device needs reset (virtio 1.3 §2.1.2).
                    Err(e) => {
                        if interrupt.fail() {
                            warn!("{name}: {e}; device needs reset");
                        } else {
                            debug!("{name}: {e}; device needs reset");
                        }
                        return None;
                    }
                }
            }
            Some(queue)
        })?;
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

/// Spawns thread `name`, running `body` on `payload`. The payload reaches the thread only
/// once the thread exists, through a channel it waits on first, so a thread the system
/// refuses (EAGAIN at a limit on threads, ENOMEM) gives it back with the error (review
/// 1.14): what the thread would own, a device's queues and the sockets it serves, stays
/// the caller's, where moving it into the thread's closure dropped it with the closure.
pub fn spawn<P, R>(
    name: &str,
    payload: P,
    body: impl FnOnce(P) -> Option<R> + Send + 'static,
) -> Result<JoinHandle<Option<R>>, (String, P)>
where
    P: Send + 'static,
    R: Send + 'static,
{
    let (give, take) = mpsc::sync_channel(1);
    let builder = thread::Builder::new().name(name.into());
    #[cfg(test)]
    let builder = if REFUSE.get() {
        builder.stack_size(REFUSED_STACK)
    } else {
        builder
    };
    let thread = match builder.spawn(move || take.recv().ok().and_then(body)) {
        Ok(thread) => thread,
        Err(e) => return Err((format!("spawning the {name} worker: {e}"), payload)),
    };
    // The thread holds the channel's other end until it has the payload.
    match give.send(payload) {
        Ok(()) => Ok(thread),
        Err(mpsc::SendError(payload)) => Err((format!("the {name} worker ended before it began"), payload)),
    }
}

#[cfg(test)]
thread_local! {
    /// Whether the spawns this thread makes are refused, as by a system at its limit of
    /// threads.
    pub static REFUSE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A stack no address space holds, 64 PiB, for which Linux and macOS refuse the thread
/// (EAGAIN).
#[cfg(test)]
const REFUSED_STACK: usize = 1 << 56;

#[cfg(test)]
mod tests {
    use super::*;

    /// A thread the system refuses gives its payload back, whole, with why; a thread it
    /// makes runs on it.
    #[test]
    fn a_refused_thread_gives_its_payload_back() {
        REFUSE.set(true);
        let refused = spawn("refused", vec![1u8, 2, 3], |v| Some(v.len()));
        REFUSE.set(false);
        let (why, payload) = refused.unwrap_err();
        assert_eq!(payload, [1, 2, 3]);
        assert!(why.starts_with("spawning the refused worker: "), "{why}");
        let made = spawn("made", payload, |v| Some(v.len())).unwrap();
        assert_eq!(made.join().unwrap(), Some(3));
    }
}
