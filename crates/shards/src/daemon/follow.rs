//! Following runs to their ends on one thread: every run's VM's socket waited on at once
//! (`platform::Poller`), each run's messages taken as they come in whole
//! (`take_messages`). A thread a run would bound the runs the daemon can follow to the
//! threads a process may have, before memory (PM M89), and wake each of them to look at
//! its clock. And the removals of `--rm` containers that end, made durable together.

use std::collections::HashMap;
use std::io::{self, Read as _};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use shards_vmm::platform::Poller;

use super::{Daemon, Inbox, Threads, lock, log};
use crate::containers::{Disk, Removal};

/// The token of the followers' own wake socket.
const WAKE: u64 = 0;

/// A run followed: its container's ID and its inbox, which holds its socket open while the
/// poller watches it.
type Followed = (String, Arc<Mutex<Inbox>>);

/// The runs being followed, and the loop that follows them.
pub(super) struct Followers {
    poller: Poller,
    /// Each run followed, by its token.
    pub(super) runs: Mutex<HashMap<u64, Followed>>,
    next: AtomicU64,
    /// Written to wake the loop; its other end, which the poller watches, read.
    wake: (UnixStream, UnixStream),
    started: AtomicBool,
    /// The loop is to return: a test's daemon, as its scope ends.
    ended: AtomicBool,
}

impl Followers {
    pub(super) fn new() -> io::Result<Followers> {
        let poller = Poller::new()?;
        let (tell, heard) = UnixStream::pair()?;
        heard.set_nonblocking(true)?;
        tell.set_nonblocking(true)?;
        poller.add(heard.as_fd(), WAKE)?;
        Ok(Followers {
            poller,
            runs: Mutex::new(HashMap::new()),
            next: AtomicU64::new(WAKE + 1),
            wake: (tell, heard),
            started: AtomicBool::new(false),
            ended: AtomicBool::new(false),
        })
    }

    /// Ends the loop, once it has woken: a test's daemon's, as its scope ends.
    #[cfg(test)]
    pub(super) fn end(&self) {
        use std::io::Write as _;
        self.ended.store(true, Ordering::SeqCst);
        // A byte is enough: one still unread wakes it as well.
        let _ = (&self.wake.0).write(&[0]);
    }
}

/// The removals of containers that ended with `--rm`, made durable together by one
/// thread: one sync of their directory serves every removal before it.
#[derive(Default)]
pub(super) struct Completing {
    pub(super) pending: Mutex<Vec<Removal>>,
    /// One is pending, or the thread is to return.
    queued: Condvar,
    /// A batch is done: names it held are free.
    done: Condvar,
    started: AtomicBool,
    ended: AtomicBool,
}

impl Completing {
    /// Ends the completer's thread once nothing is pending: a test's daemon's.
    #[cfg(test)]
    pub(super) fn end(&self) {
        self.ended.store(true, Ordering::SeqCst);
        let _guard = lock(&self.pending);
        self.queued.notify_all();
    }
}

impl<D: Disk> Daemon<D> {
    /// Follows run `id` to its end on the followers' loop, which the first run starts, and
    /// keeps its container's record: running once the VM says STARTED, exited at its
    /// DONE, or at the VM's end if the VM dies first. A command that never started leaves
    /// its container created, with the status that says why (moby daemon/start.go). A
    /// detached run's client learns whether its command started, and if not, why not, as
    /// `docker run -d` does. Where the poller cannot take it, followed here instead, on
    /// its own.
    pub(super) fn follow<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        id: String,
        inbox: Arc<Mutex<Inbox>>,
    ) {
        let f = &self.followers;
        let token = f.next.fetch_add(1, Ordering::Relaxed);
        let added = {
            let held = lock(&inbox);
            lock(&f.runs).insert(token, (id.clone(), inbox.clone()));
            f.poller.add(held.socket.stream.as_fd(), token)
        };
        if let Err(e) = added {
            lock(&f.runs).remove(&token);
            log(format!("container {id}: following its run with the others: {e}"));
            self.follow_alone(&id, &inbox);
            return;
        }
        if !f.started.swap(true, Ordering::SeqCst)
            && let Err(e) = std::thread::Builder::new()
                .name("followers".into())
                .spawn_scoped(threads, move || self.follow_all())
        {
            f.started.store(false, Ordering::SeqCst);
            log(format!("the followers' thread: {e}"));
        }
    }

    /// Takes each run's messages as they come, until [`Followers::end`].
    fn follow_all(&self) {
        let f = &self.followers;
        let mut ready = Vec::new();
        while !f.ended.load(Ordering::SeqCst) {
            if let Err(e) = f.poller.wait(&mut ready, None) {
                log(format!("the followers' wait: {e}"));
                return;
            }
            for &token in &ready {
                if token == WAKE {
                    let mut drained = [0u8; 64];
                    while matches!((&f.wake.1).read(&mut drained), Ok(n) if n > 0) {}
                    continue;
                }
                let Some((id, inbox)) = lock(&f.runs).get(&token).cloned() else {
                    continue;
                };
                if self.take_messages(&id, &inbox) {
                    let _ = f.poller.remove(lock(&inbox).socket.stream.as_fd());
                    lock(&f.runs).remove(&token);
                }
            }
        }
    }

    /// Follows run `id` on this thread, waiting on its socket alone: where the poller
    /// could not take it.
    fn follow_alone(&self, id: &str, inbox: &Mutex<Inbox>) {
        use std::os::fd::AsRawFd as _;
        let fd = lock(inbox).socket.stream.as_raw_fd();
        while !self.take_messages(id, inbox) {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll(2) on one descriptor `inbox` holds open; no timeout: the run's
            // end, whoever takes it, comes as its VM's end, which makes it readable.
            unsafe { libc::poll(&mut pfd, 1, -1) };
        }
    }

    /// Makes `removal` durable, with any others pending, on the completer's thread; on
    /// this one where that thread does not run. Never under the records' lock: the
    /// completer's waiters take it under the queue's ([`await_released`]).
    pub(super) fn complete_soon(&self, removal: Removal) {
        let c = &self.completing;
        if !c.started.load(Ordering::SeqCst) {
            let _ = self.complete(&removal);
            return;
        }
        lock(&c.pending).push(removal);
        c.queued.notify_all();
    }

    /// Starts the completer's thread.
    pub(super) fn start_completer<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        let c = &self.completing;
        if c.started.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(e) = std::thread::Builder::new()
            .name("completer".into())
            .spawn_scoped(threads, move || self.complete_all())
        {
            c.started.store(false, Ordering::SeqCst);
            log(format!(
                "the completer's thread: {e}; removals complete where they end"
            ));
        }
    }

    /// Completes the removals pending, a batch at a time, until [`Completing::end`].
    fn complete_all(&self) {
        let c = &self.completing;
        loop {
            let batch = {
                let mut pending = lock(&c.pending);
                while pending.is_empty() && !c.ended.load(Ordering::SeqCst) {
                    pending = c
                        .queued
                        .wait(pending)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                if pending.is_empty() {
                    return;
                }
                std::mem::take(&mut *pending)
            };
            self.complete_batch(&batch);
            // Under the queue's lock, which waiters check names under: no wakeup lost.
            let _guard = lock(&c.pending);
            c.done.notify_all();
        }
    }

    /// Waits while a removal pending holds `name`. Takes the records' lock under the
    /// queue's, as nothing takes the queue's under the records'.
    pub(super) fn await_released(&self, name: &str) {
        let c = &self.completing;
        let mut pending = lock(&c.pending);
        loop {
            let held = pending.iter().any(|r| r.container.name == name)
                || lock(&self.containers).is_leaving_name(name);
            if !held {
                return;
            }
            pending = c
                .done
                .wait(pending)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}
