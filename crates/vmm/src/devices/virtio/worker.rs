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
    let thread = match builder(name).spawn(move || {
        let _prioritized = prioritized();
        take.recv().ok().and_then(body)
    }) {
        Ok(thread) => thread,
        Err(e) => return Err((format!("spawning the {name} worker: {e}"), payload)),
    };
    // The thread holds the channel's other end until it has the payload.
    match give.send(payload) {
        Ok(()) => Ok(thread),
        Err(mpsc::SendError(payload)) => Err((format!("the {name} worker ended before it began"), payload)),
    }
}

/// A device worker's thread: named `name`; on a test's thread that refuses its spawns
/// ([`REFUSE`]), one the system refuses.
fn builder(name: &str) -> thread::Builder {
    let builder = thread::Builder::new().name(name.into());
    #[cfg(test)]
    let builder = if REFUSE.get() {
        builder.stack_size(REFUSED_STACK)
    } else {
        builder
    };
    builder
}

/// The calling worker's thread prioritized as a device's, so that it answers its guest as it
/// is asked, however busy its host (PM M317); at its default policy where the system will not.
fn prioritized() -> Option<impl Sized> {
    match crate::platform::prioritize_device_thread() {
        Ok(prioritized) => Some(prioritized),
        Err(e) => {
            debug!("a device worker runs at its default policy: {e}");
            None
        }
    }
}

/// A device's worker thread that lasts as long as the device (D272, amended 2026-11-18): it
/// runs its body on each payload handed to it ([`Lasting::run`]), one at a time, hands each
/// result back ([`Lasting::back`]), and waits for the next, so that what a payload's calls
/// leave under way outlasts the payload. Windows cancels the calls a thread made as the thread
/// ends ("When a thread exits, the I/Os associated with the thread must be cancelled", Windows
/// Internals, Microsoft Press), and a vsock device's pipes keep calls under way across its
/// pauses.
pub struct Lasting<P, R> {
    thread: JoinHandle<()>,
    give: mpsc::Sender<P>,
    back: mpsc::Receiver<R>,
}

impl<P: Send + 'static, R: Send + 'static> Lasting<P, R> {
    /// Spawns thread `name`, which runs `body` on `first`, then on each payload [`run`]
    /// hands it. A thread the system refuses gives `first` back with the error, as
    /// [`spawn`]'s does.
    ///
    /// [`run`]: Lasting::run
    pub fn spawn(
        name: &str,
        first: P,
        mut body: impl FnMut(P) -> R + Send + 'static,
    ) -> Result<Lasting<P, R>, (String, P)> {
        let (give, take) = mpsc::channel::<P>();
        let (done, back) = mpsc::channel::<R>();
        let thread = match builder(name).spawn(move || {
            let _prioritized = prioritized();
            while let Ok(payload) = take.recv() {
                if done.send(body(payload)).is_err() {
                    return;
                }
            }
        }) {
            Ok(thread) => thread,
            Err(e) => return Err((format!("spawning the {name} worker: {e}"), first)),
        };
        match give.send(first) {
            Ok(()) => Ok(Lasting { thread, give, back }),
            Err(mpsc::SendError(first)) => Err((format!("the {name} worker ended before it began"), first)),
        }
    }

    /// Hands it its next payload, which it runs once it has handed back the last; one whose
    /// thread has ended (its body panicked) gives the payload back.
    pub fn run(&self, payload: P) -> Result<(), P> {
        self.give
            .send(payload)
            .map_err(|mpsc::SendError(payload)| payload)
    }

    /// What the payload it runs comes to, waited for; none where its thread has ended.
    pub fn back(&self) -> Option<R> {
        self.back.recv().ok()
    }

    /// The thread that runs every payload, for a test's look at which thread that is.
    #[cfg(test)]
    pub fn thread(&self) -> &Thread {
        self.thread.thread()
    }

    /// Ends its thread once it has handed back what it runs, and waits for its end.
    pub fn end(self) {
        let Lasting { thread, give, back } = self;
        drop((give, back));
        if thread.join().is_err() {
            warn!("a virtio worker ended abnormally");
        }
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

    /// A lasting worker runs every payload on its one thread, one after another, each result
    /// handed back as it ends; one the system refuses gives its first payload back (D272,
    /// amended 2026-11-18).
    #[test]
    fn a_lasting_worker_runs_every_payload_on_its_one_thread() {
        REFUSE.set(true);
        let refused = Lasting::spawn("refused", 7u8, |n: u8| (n, thread::current().id()));
        REFUSE.set(false);
        let (why, first) = refused.err().unwrap();
        assert_eq!(first, 7);
        assert!(why.starts_with("spawning the refused worker: "), "{why}");
        let lasting = Lasting::spawn("lasting", 1u8, |n: u8| (n, thread::current().id()))
            .ok()
            .unwrap();
        let (n, its) = lasting.back().unwrap();
        assert_eq!((n, its), (1, lasting.thread().id()));
        for n in 2..5u8 {
            assert!(lasting.run(n).is_ok());
            assert_eq!(lasting.back(), Some((n, its)), "payload {n} on another thread");
        }
        lasting.end();
    }

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
