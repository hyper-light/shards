//! Containers' records written outside the lock the registry is kept under, on one
//! thread (review 7.7): a reserved container's first record, which makes it seen
//! (`Daemon::arrive`), and the record of each change, which stands in memory from the
//! moment it happens (`Registry::change`). A write waits on whatever else the filesystem
//! is doing, milliseconds at a busy host's p90 (PM M46): under the lock, everything that
//! takes the lock waited for it, and on the followers' loop, every run's messages. The
//! records are written in the order their changes were asked for, a container changed
//! again before its record is written once, as it then stands; and a command is answered
//! once every record changed before it is written, or behind (audit A15).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

use shards_ipc::kind;

use super::{Daemon, Threads, lock, log};
use crate::containers::Disk;

/// The records to write, and the thread that writes them.
#[derive(Default)]
pub(super) struct Recording {
    queue: Mutex<Queue>,
    /// One is queued, or the thread is to return.
    queued: Condvar,
    /// A write is done: those waiting for it look again.
    done: Condvar,
    started: AtomicBool,
    /// The thread is to return: a test's daemon's, as its scope ends.
    ended: AtomicBool,
}

#[derive(Default)]
struct Queue {
    /// The containers whose records to write, each once, in the order first asked for,
    /// each with the number of that first ask.
    ids: VecDeque<(u64, String)>,
    /// Those queued, each with the clients of detached runs to tell once it is written.
    told: HashMap<String, Vec<UnixStream>>,
    /// Those whose records could not be written, until they are.
    failed: HashSet<String>,
    /// The number of the write going on, if one is.
    writing: Option<u64>,
    /// The asks so far, numbered: an ask for a container queued already is written with
    /// it, which an earlier number holds its place.
    asked: u64,
}

impl Recording {
    /// How many containers' records could not be written, and are not yet.
    #[cfg(test)]
    pub(super) fn failed(&self) -> usize {
        lock(&self.queue).failed.len()
    }

    /// Ends the recorder's thread once nothing is queued: a test's daemon's.
    #[cfg(test)]
    pub(super) fn end(&self) {
        self.ended.store(true, Ordering::SeqCst);
        let _guard = lock(&self.queue);
        self.queued.notify_all();
    }
}

impl<D: Disk> Daemon<D> {
    /// Starts the recorder's thread, unless it runs. Where it cannot run, a reserved
    /// container's record is written where it is asked for, and a change's stays behind.
    pub(super) fn start_recorder<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        let r = &self.recording;
        if !r.started.swap(true, Ordering::SeqCst)
            && let Err(e) = std::thread::Builder::new()
                .name("recorder".into())
                .spawn_scoped(threads, move || self.record_all())
        {
            r.started.store(false, Ordering::SeqCst);
            log(format!("the recorder's thread: {e}"));
        }
    }

    /// Has the record of reserved container `id` written, which makes it seen: on the
    /// recorder's thread, started here if it is not yet; here, if it cannot be.
    pub(super) fn record_arrival<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, id: &str) {
        self.start_recorder(threads);
        if self.recording.started.load(Ordering::SeqCst) {
            self.record_soon(id, Vec::new());
        } else if let Err(e) = self.arrive(id) {
            log(format!("container {id}: its record is behind: {e}"));
        }
    }

    /// Has container `id`'s record written on the recorder's thread, once however often it
    /// is asked meanwhile, then `told`, the clients of detached runs, told it is, or that
    /// it is behind. Waits for nothing: its callers may hold the registry's lock. Where
    /// the recorder does not run, the record stays behind, and `told` are told so at once.
    pub(super) fn record_soon(&self, id: &str, told: Vec<UnixStream>) {
        let r = &self.recording;
        if !r.started.load(Ordering::SeqCst) {
            tell(told, id, &Err(io::Error::other("no thread writes records")));
            return;
        }
        {
            let mut queue = lock(&r.queue);
            let queued = &mut *queue;
            queued.asked += 1;
            match queued.told.get_mut(id) {
                Some(waiting) => waiting.extend(told),
                None => {
                    queued.ids.push_back((queued.asked, id.to_string()));
                    queued.told.insert(id.to_string(), told);
                }
            }
        }
        r.queued.notify_one();
    }

    /// Writes the records queued, until [`Recording::end`].
    fn record_all(&self) {
        let r = &self.recording;
        loop {
            let (id, told) = {
                let mut queue = lock(&r.queue);
                queue.writing = None;
                r.done.notify_all();
                while queue.ids.is_empty() && !r.ended.load(Ordering::SeqCst) {
                    queue = r.queued.wait(queue).unwrap_or_else(PoisonError::into_inner);
                }
                let Some((number, id)) = queue.ids.pop_front() else {
                    return;
                };
                let told = queue.told.remove(&id).unwrap_or_default();
                queue.writing = Some(number);
                (id, told)
            };
            let recorded = self.record(&id);
            {
                let mut queue = lock(&r.queue);
                match &recorded {
                    Ok(()) if queue.failed.remove(&id) => {
                        log(format!("container {id}: its record is written again"));
                    }
                    Ok(()) => {}
                    Err(e) => {
                        if queue.failed.insert(id.clone()) {
                            log(format!("container {id}: its record is behind: {e}"));
                        }
                    }
                }
            }
            tell(told, &id, &recorded);
        }
    }

    /// Writes container `id`'s record: its first, which makes it seen, or that of its
    /// changes as it now stands. One that changes as it is written stays behind, for the
    /// write its change asked for; one gone has nothing to write, and one taken out of
    /// sight as it was written nothing to keep: what was written went with its directory,
    /// set aside, or found none (`Removal::set_aside`).
    fn record(&self, id: &str) -> io::Result<()> {
        let written = if lock(&self.containers).is_arriving(id) {
            self.arrive(id)
        } else {
            let Some((recorder, c)) = lock(&self.containers).snapshot(id) else {
                return Ok(());
            };
            recorder
                .write(&self.disk, &c)
                .map(|()| lock(&self.containers).written(id, &c))
        };
        match written {
            Err(_) if lock(&self.containers).made(id).is_none() => Ok(()),
            written => written,
        }
    }

    /// Waits until every record asked for so far is written, or behind, the registry's
    /// lock free meanwhile: before a command is answered (audit A15), and as the daemon
    /// exits, so that what it last knew of its containers is what the next one reads.
    /// Those asked for since are not waited for: written in order, they come after.
    pub(super) fn await_recorded(&self) {
        let r = &self.recording;
        if !r.started.load(Ordering::SeqCst) {
            return;
        }
        let mut queue = lock(&r.queue);
        let asked = queue.asked;
        while queue.writing.is_some_and(|n| n <= asked) || queue.ids.front().is_some_and(|(n, _)| *n <= asked)
        {
            queue = r.done.wait(queue).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Tells the clients of detached runs that their containers' records are written, or
/// are behind, and why.
pub(super) fn tell(told: Vec<UnixStream>, id: &str, recorded: &io::Result<()>) {
    for client in told {
        if let Err(e) = recorded {
            let warning = format!("WARNING: container {id}: its record is behind: {e}\n");
            let _ = shards_ipc::send(&client, kind::ERR, warning.as_bytes(), &[]);
        }
        let _ = shards_ipc::send(&client, kind::END, &[0], &[]);
    }
}
