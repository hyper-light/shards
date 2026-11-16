//! A test's wait on a thread's answer that follows the thread's progress, not a clock: the
//! daemon's `Wait::WhileRuns` (D209), applied to a thread. Past the work's own deadline the
//! thread is waited on while it runs, waits for a CPU or waits on the kernel's own work, so a
//! host that runs it late (a test at nice 19 on a busy host) is waited on as long as it takes,
//! and a thread waiting for an event in a call that should have returned by its deadline
//! fails the test, saying so. Linux and macOS say whether a thread waits so (proc(5)'s
//! `/proc/<pid>/task/<tid>/stat` state; Mach's `thread_info` run state); Windows documents no
//! thread's state, so there the wait is for the answer alone.
//!
//! The thread outlives its answer until its [`Worker`] is dropped, so every look is at that
//! thread: once it has ended, Linux's `/proc/<pid>/task/<tid>` is gone (a look failed ENOENT,
//! or ESRCH for a stat opened as it went) and macOS lets go of its port's name, which a
//! later port may take (PM M585).

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

/// A thread, as another looks at it, and when it began its work; the thread waits, once it
/// has answered, until this is dropped.
#[derive(Debug)]
pub(crate) struct Worker {
    #[cfg(target_os = "linux")]
    tid: libc::pid_t,
    #[cfg(target_os = "macos")]
    port: libc::mach_port_t,
    began: Instant,
    _held: Sender<()>,
}

impl Worker {
    /// The calling thread, beginning its work now, held by `held`.
    fn this(held: Sender<()>) -> Worker {
        #[cfg(target_os = "linux")]
        // SAFETY: gettid(2), no arguments.
        let tid = unsafe { libc::gettid() };
        #[cfg(target_os = "macos")]
        // SAFETY: the calling thread's own port, which lives as long as it does.
        let port = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) };
        Worker {
            #[cfg(target_os = "linux")]
            tid,
            #[cfg(target_os = "macos")]
            port,
            began: Instant::now(),
            _held: held,
        }
    }

    /// Where it waits for an event, where it does: in an interruptible sleep (proc(5)'s `S`,
    /// Mach's TH_STATE_WAITING), as a poll, a read or a futex waits, which only that event
    /// ends. A thread running, waiting for a CPU or stopped (as a pacer stops a run's
    /// processes) is not; nor is one in the kernel's own uninterruptible wait (`D`, Mach's
    /// TH_STATE_UNINTERRUPTIBLE), as a page fault waits for its process's memory map that
    /// another thread holds (`lock_mm_and_find_vma`), which ends as that work does (PM M585);
    /// nor, on Linux, one waiting for a lock another thread holds ([`held_lock`]). On Linux it
    /// says where, its `wchan` and its call.
    fn waiting(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            let task = format!("/proc/self/task/{}", self.tid);
            let stat = std::fs::read_to_string(format!("{task}/stat")).unwrap();
            // The state follows the command, which is in parentheses and may hold any byte.
            let state = stat.rfind(')').and_then(|i| stat.get(i + 2..i + 3)).unwrap();
            if state != "S" {
                return None;
            }
            let call = std::fs::read_to_string(format!("{task}/syscall")).unwrap_or_default();
            if held_lock(&call) {
                return None;
            }
            let wchan = std::fs::read_to_string(format!("{task}/wchan")).unwrap_or_default();
            Some(format!("{wchan}: {}", call.trim()))
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: zeroed plain data, which thread_info fills.
            let mut info: libc::thread_extended_info = unsafe { std::mem::zeroed() };
            let mut count = libc::THREAD_EXTENDED_INFO_COUNT;
            // SAFETY: the port of a live thread of this process (held until this drops), into
            // a buffer of `count` words.
            let r = unsafe {
                libc::thread_info(
                    self.port,
                    libc::THREAD_EXTENDED_INFO as libc::thread_flavor_t,
                    (&raw mut info).cast(),
                    &raw mut count,
                )
            };
            assert_eq!(r, libc::KERN_SUCCESS);
            (info.pth_run_state == libc::TH_STATE_WAITING).then(String::new)
        }
        #[cfg(windows)]
        None
    }
}

/// Whether a thread's call, as `/proc/<pid>/task/<tid>/syscall` says it (proc(5): the call's
/// number and arguments, in hex), waits for a lock another thread holds: glibc's low-level
/// lock contended, `futex(word, FUTEX_WAIT, 2)`, 2 being its word's "locked, with waiters"
/// (glibc nptl/lowlevellock.c `__lll_lock_wait`), as malloc's arenas and stdio's locks wait.
/// Its holder runs, waits for a CPU or is stopped: the host's hold, as `D` is, never a wait
/// for an event; condition variables and parks (glibc's and std's) wait FUTEX_WAIT_BITSET,
/// which this is not, and so does std's own `Mutex` (std/src/sys/pal/unix/futex.rs), which
/// stays counted. Such a lock names no holder to look at, so a deadlock through one is not
/// refused here: it surfaces as whatever bounds the run (PM M589).
#[cfg(target_os = "linux")]
fn held_lock(call: &str) -> bool {
    let mut words = call.split_whitespace();
    let number = words.next().and_then(|n| n.parse::<libc::c_long>().ok());
    let mut args = words.map(|a| u64::from_str_radix(a.trim_start_matches("0x"), 16).ok());
    let (_word, op, value) = (args.next(), args.next().flatten(), args.next().flatten());
    let flags = u64::try_from(libc::FUTEX_PRIVATE_FLAG | libc::FUTEX_CLOCK_REALTIME).unwrap();
    number == Some(libc::SYS_futex)
        && op.map(|op| op & !flags) == u64::try_from(libc::FUTEX_WAIT).ok()
        && value == Some(2)
}

/// Runs `work` on a thread of its own: the thread, as [`answer`] looks at it, and its answer.
pub(crate) fn on_a_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> (Worker, Receiver<T>) {
    let (to_us, answered) = std::sync::mpsc::channel();
    let (said, worker) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let (held, released) = std::sync::mpsc::channel();
        said.send(Worker::this(held)).unwrap();
        let _ = to_us.send(work());
        // Disconnected as its `Worker` drops: until then, its tid or port is still its own.
        let _ = released.recv();
    });
    (worker.recv().unwrap(), answered)
}

/// The answer `worker` sends on `rx`. Until `timeout` past its beginning, the work's own
/// deadline, it may wait as it likes; past it, it is looked at each `timeout` (the work's
/// scale: no shorter than the millisecond a poll rounds its last wait up to), and fails the
/// test if found waiting for an event ([`Worker::waiting`]) at two looks in a row, where it
/// should have answered; any other worker is waited on. A look counts only where no answer
/// had come by its end: a worker that has answered waits too, held until `worker` drops.
pub(crate) fn answer<T>(rx: &Receiver<T>, worker: Worker, timeout: Duration) -> T {
    let due = worker.began + timeout;
    if let Ok(answer) = rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
        return answer;
    }
    let mut was_waiting = None;
    loop {
        match rx.recv_timeout(timeout) {
            Ok(answer) => return answer,
            Err(RecvTimeoutError::Disconnected) => panic!("the worker ended without answering"),
            Err(RecvTimeoutError::Timeout) => {}
        }
        let waiting = worker.waiting();
        match rx.try_recv() {
            Ok(answer) => return answer,
            Err(TryRecvError::Disconnected) => panic!("the worker ended without answering"),
            Err(TryRecvError::Empty) => {}
        }
        if let (Some(before), Some(now)) = (&was_waiting, &waiting) {
            panic!(
                "waiting for an event {:?} past its {timeout:?}, where it should have answered (in {before:?}, then {now:?})",
                worker.began.elapsed().saturating_sub(timeout)
            );
        }
        was_waiting = waiting;
    }
}

/// The least timed wait this host's streams keep, measured as the test runs: a read on a
/// fresh pair whose timeout is the least there is (a nonzero `Duration`'s least), timed from
/// its call to its timing out: microseconds where the streams keep a timeout to its
/// nanosecond (macOS, Linux), a millisecond or more where the poll's unit is one (WSAPoll).
/// Learned where it runs, so it carries this host's scheduling as the test does: the timeout
/// of a test that needs some timeout kept and any one shows it, never a number of ours (R03),
/// and never a wait on another thread's fact.
pub(crate) fn kept_wait() -> Duration {
    use std::io::Read as _;
    let (mut a, _b) = crate::stream::pair().unwrap();
    crate::Stream::set_read_timeout(&a, Some(Duration::from_nanos(1))).unwrap();
    let began = Instant::now();
    let read = a.read(&mut [0u8; 1]).map_err(|e| e.kind());
    let kept = began.elapsed();
    assert!(
        matches!(
            read,
            Err(std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
        ),
        "{read:?}"
    );
    kept
}

/// The latest instant this host's clock names: the deadline of a wait for a fact, which no
/// wait reaches, so the test waits for the fact itself. Halved down from `Duration::MAX`
/// until the clock can add it: `Instant`'s range is the platform's own.
pub(crate) fn latest() -> Instant {
    let now = Instant::now();
    let mut far = Duration::MAX;
    loop {
        if let Some(at) = now.checked_add(far) {
            return at;
        }
        far /= 2;
    }
}

/// A finite patience no wait reaches, for an API that takes only finite ones (a borrowed
/// connect refuses `Duration::MAX`): half the span from now to the clock's latest instant
/// ([`latest`]), so the API's own `now`, taken later, can still add it.
pub(crate) fn unreached() -> Duration {
    latest().saturating_duration_since(Instant::now()) / 2
}

/// What a test's attempt asked with a patience came to: done, with what it found; or its
/// patience ran out sooner than its subject's step began (a dial, a CONNECT written).
pub(crate) enum Reached<T> {
    Done(T),
    Sooner,
}

/// The patience a test's subject needs to reach its step, this host's own, learned as the
/// test runs: `attempt` asked first with the least wait this host's streams keep
/// ([`kept_wait`], which on a host that keeps a timeout to its nanosecond is microseconds),
/// then again with its patience grown by what each attempt that ran out sooner took, no less
/// than that patience, until one is done. No number of ours (R03), and never a wait on another
/// thread's fact.
pub(crate) fn until_reached<T>(mut attempt: impl FnMut(Duration) -> Reached<T>) -> T {
    let mut patience = kept_wait();
    loop {
        let began = Instant::now();
        match attempt(patience) {
            Reached::Done(done) => return done,
            Reached::Sooner => patience = patience.saturating_add(began.elapsed()),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod tests {
    use super::on_a_thread;

    /// glibc's contended low-level lock is a hold; a condition's or a park's wait, a poll and a
    /// running thread are not (PM M589's calls, read on adacachy2).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_wait_for_a_held_lock_is_told_from_a_wait_for_an_event() {
        let futex = libc::SYS_futex;
        assert!(super::held_lock(&format!(
            "{futex} 0x7f42cc000030 0x80 0x2 0x0 0x8 0x150 0x7f426e1ef298 0x7f42f36a9699"
        )));
        assert!(super::held_lock(&format!(
            "{futex} 0x7f2273a3eb00 0x0 0x2 0x0 0x0 0x0 0x1 0x2"
        )));
        assert!(!super::held_lock(&format!(
            "{futex} 0x55ef1a182bd0 0x89 0x2 0x0 0x0 0xffffffff 0x1 0x2"
        )));
        assert!(!super::held_lock(&format!(
            "{futex} 0x55ef1a182bd0 0x80 0x1 0x0 0x0 0x0 0x1 0x2"
        )));
        assert!(!super::held_lock(&format!(
            "{} 0x5 0x1 0xffffffff 0x0 0x0 0x0 0x1 0x2",
            libc::SYS_ppoll
        )));
        assert!(!super::held_lock("running"));
    }

    /// A worker that has answered is still its own thread at a look: it waits to be let go of,
    /// where one that had ended would leave a look nothing to read (PM M585).
    #[test]
    fn a_worker_that_has_answered_is_still_its_thread_at_each_look() {
        let (worker, rx) = on_a_thread(|| ());
        rx.recv().unwrap();
        while worker.waiting().is_none() {
            std::thread::yield_now();
        }
    }
}
