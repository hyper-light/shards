//! Following runs and VMs to their ends on one thread: every run's VM's socket waited on
//! at once (`platform::Poller`), each run's messages taken as they come in whole
//! (`take_messages`), and every VM process's end, and its network process's, as it comes
//! (`Poller::add_exit`). A thread a run would bound the runs the daemon can follow to the
//! threads a process may have, before memory (PM M89, M90), and wake each of them to look
//! at its clock. And the removals of `--rm` containers that end, made durable together.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::io::{self, Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use shards_vmm::platform::{ExitWatch, Poller};

use super::{Daemon, Inbox, Threads, lock, log};
use crate::containers::{Disk, Removal};

/// The token of the followers' own wake socket.
const WAKE: u64 = 0;

/// A run followed: its container's ID and its inbox, which holds its socket open while the
/// poller watches it.
type Followed = (String, Arc<Mutex<Inbox>>);

/// A process followed to its end ([`Daemon::follow_vm`]), with its watch while it has one.
enum Process {
    /// A VM; the pool it waits in, if it was started for one; and its network process's
    /// token, if it has one.
    Vm {
        vm: Arc<shards_ipc::Child>,
        pool: Option<PathBuf>,
        net: Option<u64>,
        watch: Option<ExitWatch>,
    },
    /// A VM's network process: its VM's pid, whose ports are freed once both have ended,
    /// and whether that VM still runs.
    Net {
        child: Arc<shards_ipc::Child>,
        vm: u32,
        vm_running: bool,
        watch: Option<ExitWatch>,
    },
}

/// The runs being followed, and the loop that follows them.
pub(super) struct Followers {
    poller: Poller,
    /// Each run followed, by its token.
    pub(super) runs: Mutex<HashMap<u64, Followed>>,
    /// Each process followed, by its token.
    processes: Mutex<HashMap<u64, Process>>,
    /// When network processes whose VMs have gone are ended, by their tokens.
    deadlines: Mutex<BinaryHeap<Reverse<(Instant, u64)>>>,
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
            processes: Mutex::new(HashMap::new()),
            deadlines: Mutex::new(BinaryHeap::new()),
            next: AtomicU64::new(WAKE + 1),
            wake: (tell, heard),
            started: AtomicBool::new(false),
            ended: AtomicBool::new(false),
        })
    }

    /// Wakes the loop, to look again at what it waits for.
    fn wake(&self) {
        // A byte is enough: one still unread wakes it as well.
        let _ = (&self.wake.0).write(&[0]);
    }

    /// Ends the loop, once it has woken, and every process it follows: a test's daemon's,
    /// as its scope ends.
    #[cfg(test)]
    pub(super) fn end(&self) {
        self.ended.store(true, Ordering::SeqCst);
        self.wake();
        for process in lock(&self.processes).values() {
            let (Process::Vm { vm: child, .. } | Process::Net { child, .. }) = process;
            let _ = child.kill(libc::SIGKILL);
        }
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
        // The files its VM asks for are made on a thread of their own (files.rs).
        self.start_files(threads);
        if let Err(e) = added {
            lock(&f.runs).remove(&token);
            log(format!("container {id}: following its run with the others: {e}"));
            self.follow_alone(&id, &inbox);
            return;
        }
        self.start_followers(threads);
    }

    /// Starts the followers' thread, unless it runs.
    fn start_followers<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        let f = &self.followers;
        if !f.started.swap(true, Ordering::SeqCst)
            && let Err(e) = std::thread::Builder::new()
                .name("followers".into())
                .spawn_scoped(threads, move || self.follow_all(threads))
        {
            f.started.store(false, Ordering::SeqCst);
            log(format!("the followers' thread: {e}"));
        }
    }

    /// Takes each run's messages as they come, and each process's end, and ends network
    /// processes past their grace, until [`Followers::end`].
    fn follow_all<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>) {
        let f = &self.followers;
        let mut ready = Vec::new();
        while !f.ended.load(Ordering::SeqCst) {
            let due = lock(&f.deadlines)
                .peek()
                .map(|Reverse((at, _))| at.saturating_duration_since(Instant::now()));
            if let Err(e) = f.poller.wait(&mut ready, due) {
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
                    self.process_ended(threads, token);
                    continue;
                };
                if self.take_messages(&id, &inbox) {
                    let _ = f.poller.remove(lock(&inbox).socket.stream.as_fd());
                    lock(&f.runs).remove(&token);
                }
            }
            let now = Instant::now();
            loop {
                let overdue = {
                    let mut deadlines = lock(&f.deadlines);
                    match deadlines.peek() {
                        Some(Reverse((at, _))) if *at <= now => {
                            deadlines.pop().map(|Reverse((_, token))| token)
                        }
                        _ => None,
                    }
                };
                let Some(token) = overdue else {
                    break;
                };
                // Its end follows, which frees its VM's ports.
                if let Some(Process::Net { child, .. }) = lock(&f.processes).get(&token) {
                    let _ = child.kill(libc::SIGKILL);
                }
            }
        }
    }

    /// Follows VM `vm`, and its network process `net`, to their ends on the followers'
    /// loop, rather than on a thread each (PM M90): each reaped as it ends, the VM taken
    /// out of `pool` if it ends waiting there, its network process given
    /// [`netproc::GRACE`](crate::netproc::GRACE) to follow it, and its ports freed once
    /// both have gone. A process whose end cannot be watched (Linux before 5.3, or no
    /// descriptor to spare) is waited for on a thread of its own.
    pub(super) fn follow_vm<'s, 'e>(
        &'s self,
        threads: &'s Threads<'s, 'e>,
        vm: Arc<shards_ipc::Child>,
        pool: Option<PathBuf>,
        net: Option<shards_ipc::Child>,
    ) {
        let f = &self.followers;
        let vm_token = f.next.fetch_add(1, Ordering::Relaxed);
        let net = net.map(|child| (f.next.fetch_add(1, Ordering::Relaxed), Arc::new(child)));
        let mut watched = vec![(vm_token, vm.clone())];
        // Followed before watched: an end told at once finds it.
        {
            let mut processes = lock(&f.processes);
            if let Some((token, child)) = &net {
                watched.push((*token, child.clone()));
                processes.insert(
                    *token,
                    Process::Net {
                        child: child.clone(),
                        vm: vm.id(),
                        vm_running: true,
                        watch: None,
                    },
                );
            }
            processes.insert(
                vm_token,
                Process::Vm {
                    vm,
                    pool,
                    net: net.map(|(token, _)| token),
                    watch: None,
                },
            );
        }
        self.start_followers(threads);
        for (token, child) in watched {
            match f.poller.add_exit(child.id(), token) {
                // Kept while it is followed; one that has ended meanwhile goes now.
                Ok(watch) => {
                    if let Some(Process::Vm { watch: kept, .. } | Process::Net { watch: kept, .. }) =
                        lock(&f.processes).get_mut(&token)
                    {
                        *kept = Some(watch);
                    }
                }
                // Ended already: macOS watches only ends to come.
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => self.process_ended(threads, token),
                Err(e) => {
                    let pid = child.id();
                    let waiting = std::thread::Builder::new()
                        .name("process end".into())
                        .spawn_scoped(threads, move || {
                            let _ = child.ended();
                            self.process_ended(threads, token);
                        });
                    if let Err(spawn) = waiting {
                        log(format!(
                            "process {pid}: its end cannot be watched ({e}) nor waited for ({spawn})"
                        ));
                    }
                }
            }
        }
    }

    /// What follows process `token`'s end: a VM reaped, out of its pool if it ended
    /// waiting there, its network process given its grace; a network process reaped, and
    /// its VM's ports freed once the VM has gone too.
    fn process_ended<'s, 'e>(&'s self, threads: &'s Threads<'s, 'e>, token: u64) {
        let f = &self.followers;
        let Some(process) = lock(&f.processes).remove(&token) else {
            return;
        };
        // Its watch goes with it: on Linux, its descriptor leaves the poller.
        match process {
            Process::Vm { vm, pool, net, .. } => {
                let pid = vm.id();
                // It has ended: no wait.
                let status = vm.wait();
                let waited = pool.is_some_and(|dir| {
                    let mut state = lock(&self.state);
                    let Some(pool) = state.pools.get_mut(&dir) else {
                        return false;
                    };
                    let Some(i) = pool.ready.iter().position(|r| r.vm.id() == pid) else {
                        return false;
                    };
                    pool.ready.remove(i);
                    drop(state);
                    log(format!("warm VM {pid} ended while it waited ({status:?})"));
                    self.refill_soon(threads, Some(&dir));
                    true
                });
                // A VM that served exits 0, whatever its command's status: the client has that.
                if !waited && !matches!(status, Ok(0)) {
                    log(format!("VM {pid} ended with {status:?}"));
                }
                let Some(net) = net else {
                    return;
                };
                let mut processes = lock(&f.processes);
                if let Some(Process::Net { vm_running, .. }) = processes.get_mut(&net) {
                    *vm_running = false;
                    drop(processes);
                    lock(&f.deadlines).push(Reverse((Instant::now() + crate::netproc::GRACE, net)));
                    f.wake();
                } else {
                    drop(processes);
                    self.free_ports(None, Some(pid));
                }
            }
            Process::Net {
                child,
                vm,
                vm_running,
                ..
            } => {
                let _ = child.wait();
                if !vm_running {
                    self.free_ports(None, Some(vm));
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

    /// Sets `removal` aside and makes it durable, with any others pending, on the
    /// completer's thread; on this one where that thread does not run. Never under the
    /// records' lock: the completer's waiters take it under the queue's
    /// ([`await_released`]).
    pub(super) fn complete_soon(&self, removal: Removal) {
        let c = &self.completing;
        if !c.started.load(Ordering::SeqCst) {
            if self.set_aside(&removal).is_ok() {
                let _ = self.complete(&removal);
            }
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
