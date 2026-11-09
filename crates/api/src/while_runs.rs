//! A test's wait on a thread's answer that follows the thread's progress, not a clock: the
//! daemon's `Wait::WhileRuns` (D209), applied to a thread. Past the work's own deadline the
//! thread is waited on while it runs or waits for a CPU, so a host that runs it late (a test
//! at nice 19 on a busy host) is waited on as long as it takes, and a thread blocked in a
//! call that should have returned by its deadline fails the test, saying so. Linux and macOS
//! say whether a thread is blocked (proc(5)'s `/proc/<pid>/task/<tid>/stat` state; Mach's
//! `thread_info` run state); Windows documents no thread's state, so there the wait is for
//! the answer alone.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// A thread, as another looks at it, and when it began its work.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Worker {
    #[cfg(target_os = "linux")]
    tid: libc::pid_t,
    #[cfg(target_os = "macos")]
    port: libc::mach_port_t,
    began: Instant,
}

impl Worker {
    /// The calling thread, beginning its work now.
    fn this() -> Worker {
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
        }
    }

    /// Whether it is blocked in a wait of the kernel's: neither running nor waiting for a
    /// CPU (nor stopped, as a pacer stops a run's processes).
    fn blocked(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            let stat = std::fs::read_to_string(format!("/proc/self/task/{}/stat", self.tid)).unwrap();
            // The state follows the command, which is in parentheses and may hold any byte.
            let state = stat.rfind(')').and_then(|i| stat.get(i + 2..i + 3)).unwrap();
            matches!(state, "S" | "D")
        }
        #[cfg(target_os = "macos")]
        {
            // SAFETY: zeroed plain data, which thread_info fills.
            let mut info: libc::thread_extended_info = unsafe { std::mem::zeroed() };
            let mut count = libc::THREAD_EXTENDED_INFO_COUNT;
            // SAFETY: the port of a live thread of this process (it has not answered), into a
            // buffer of `count` words.
            let r = unsafe {
                libc::thread_info(
                    self.port,
                    libc::THREAD_EXTENDED_INFO as libc::thread_flavor_t,
                    (&raw mut info).cast(),
                    &raw mut count,
                )
            };
            assert_eq!(r, libc::KERN_SUCCESS);
            matches!(
                info.pth_run_state,
                libc::TH_STATE_WAITING | libc::TH_STATE_UNINTERRUPTIBLE
            )
        }
        #[cfg(windows)]
        false
    }
}

/// Runs `work` on a thread of its own: the thread, as [`answer`] looks at it, and its answer.
pub(crate) fn on_a_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> (Worker, Receiver<T>) {
    let (to_us, answered) = std::sync::mpsc::channel();
    let (said, worker) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        said.send(Worker::this()).unwrap();
        let _ = to_us.send(work());
    });
    (worker.recv().unwrap(), answered)
}

/// The answer `worker` sends on `rx`. Until `timeout` past its beginning, the work's own
/// deadline, it may wait as it likes; past it, it is looked at each `timeout` (the work's
/// scale: no shorter than the millisecond a poll rounds its last wait up to), and fails the
/// test if found blocked at two looks in a row, where it should have answered. A worker
/// that is running, waits for a CPU or is stopped is waited on.
pub(crate) fn answer<T>(rx: &Receiver<T>, worker: Worker, timeout: Duration) -> T {
    let due = worker.began + timeout;
    if let Ok(answer) = rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
        return answer;
    }
    let mut was_blocked = false;
    loop {
        match rx.recv_timeout(timeout) {
            Ok(answer) => return answer,
            Err(RecvTimeoutError::Disconnected) => panic!("the worker ended without answering"),
            Err(RecvTimeoutError::Timeout) => {}
        }
        let blocked = worker.blocked();
        assert!(
            !(blocked && was_blocked),
            "blocked in the kernel {:?} past its {timeout:?}, where it should have answered",
            worker.began.elapsed().saturating_sub(timeout)
        );
        was_blocked = blocked;
    }
}
