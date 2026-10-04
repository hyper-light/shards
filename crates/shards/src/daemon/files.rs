//! The files runs' VMs need made as they run, made on a thread of their own rather than on
//! the followers' loop, which takes every run's messages: a log's next segment, two files
//! made and the oldest two removed (`segments::new_segment`), and the working set a VM
//! recorded, written durably into its template (`shards_vmm::vm::accept_working_set`).
//! On the loop, each held up every other run's messages for as long as the filesystem
//! took: a segment 10.9 ms at a loaded host's p90 (PM M98).

use std::collections::VecDeque;
use std::fs::File;
use std::os::fd::AsFd as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use shards_ipc::kind;

use super::{Daemon, RunSocket, Threads, lock, log};
use crate::containers::Disk;

/// A file to make.
pub(super) enum Job {
    /// Log segment `seq` of the container in `dir`, the oldest past those its log keeps
    /// removed, its descriptors sent to the run's VM on `socket`; none where it cannot be
    /// made, and the VM then keeps no more output (workload.rs, `Logger`).
    Segment {
        id: String,
        dir: PathBuf,
        seq: u64,
        socket: Arc<RunSocket>,
    },
    /// The working set `set` a VM recorded from generation `name` of the template in
    /// `dir`.
    WorkingSet {
        dir: PathBuf,
        name: String,
        set: Vec<u8>,
    },
    /// Holds the thread until told: a test's.
    #[cfg(test)]
    Hold(std::sync::mpsc::Receiver<()>),
}

/// The jobs to do, and the thread that does them.
#[derive(Default)]
pub(super) struct Files {
    queue: Mutex<Queue>,
    /// One is queued, or the thread is to return.
    queued: Condvar,
    /// One is done: those waiting for the queue to empty look again.
    done: Condvar,
    started: AtomicBool,
    /// The thread is to return once none is queued: a test's daemon's, as its scope ends.
    ended: AtomicBool,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    /// Those asked for and not yet done: queued, or being done.
    unfinished: usize,
}

impl Files {
    /// How many jobs wait their turn.
    #[cfg(test)]
    pub(super) fn queued(&self) -> usize {
        lock(&self.queue).jobs.len()
    }

    /// Whether the thread runs.
    #[cfg(test)]
    pub(super) fn started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    /// Ends the thread once nothing is queued: a test's daemon's.
    #[cfg(test)]
    pub(super) fn end(&self) {
        self.ended.store(true, Ordering::SeqCst);
        let _guard = lock(&self.queue);
        self.queued.notify_all();
    }
}

impl<D: Disk> Daemon<D> {
    /// Starts the files' thread, unless it runs.
    pub(super) fn start_files<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        let f = &self.files;
        if !f.started.swap(true, Ordering::SeqCst)
            && let Err(e) = std::thread::Builder::new()
                .name("files".into())
                .spawn_scoped(threads, move || self.make_all())
        {
            f.started.store(false, Ordering::SeqCst);
            log(format!(
                "the files' thread: {e}; runs' files are made where they are asked for"
            ));
        }
    }

    /// Has `job` done on the files' thread, after those queued before it; here, where
    /// that thread does not run. Waits for nothing else.
    pub(super) fn make_soon(&self, job: Job) {
        let f = &self.files;
        if !f.started.load(Ordering::SeqCst) {
            self.make(job);
            return;
        }
        {
            let mut queue = lock(&f.queue);
            queue.jobs.push_back(job);
            queue.unfinished = queue.unfinished.saturating_add(1);
        }
        f.queued.notify_one();
    }

    /// Waits until every job asked for so far is done: as the daemon exits, so that a
    /// template's working set, which no restore records again where restores record none
    /// (`vm::RESTORES_RECORD`), is not lost with it.
    pub(super) fn await_made(&self) {
        let f = &self.files;
        let mut queue = lock(&f.queue);
        while queue.unfinished > 0 {
            queue = f.done.wait(queue).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Does the jobs queued, in turn, until [`Files::end`].
    fn make_all(&self) {
        let f = &self.files;
        loop {
            let job = {
                let mut queue = lock(&f.queue);
                loop {
                    if let Some(job) = queue.jobs.pop_front() {
                        break job;
                    }
                    if f.ended.load(Ordering::SeqCst) {
                        return;
                    }
                    queue = f.queued.wait(queue).unwrap_or_else(PoisonError::into_inner);
                }
            };
            self.make(job);
            {
                let mut queue = lock(&f.queue);
                queue.unfinished = queue.unfinished.saturating_sub(1);
            }
            f.done.notify_all();
        }
    }

    fn make(&self, job: Job) {
        match job {
            Job::Segment { id, dir, seq, socket } => {
                let made = File::open(&dir)
                    .and_then(|d| crate::segments::new_segment(&d, seq, self.logs.files))
                    .map_err(|e| log(format!("container {id}: log segment {seq}: {e}")))
                    .ok();
                let sent = match &made {
                    Some((log, index)) => {
                        socket.send(kind::SEGMENT, &seq.to_be_bytes(), &[log.as_fd(), index.as_fd()])
                    }
                    None => socket.send(kind::SEGMENT, &seq.to_be_bytes(), &[]),
                };
                if let Err(e) = sent {
                    log(format!("container {id}: answering for its log: {e}"));
                }
            }
            Job::WorkingSet { dir, name, set } => {
                match shards_vmm::vm::accept_working_set(&dir, &name, &set) {
                    Ok(0) => {}
                    Ok(n) => log(format!("{}: a working set of {n} pages", dir.display())),
                    Err(e) => log(format!("{}: its working set: {e}", dir.display())),
                }
            }
            #[cfg(test)]
            Job::Hold(until) => {
                let _ = until.recv();
            }
        }
    }
}
