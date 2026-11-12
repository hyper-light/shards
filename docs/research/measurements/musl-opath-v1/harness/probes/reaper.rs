
/// Probe (docs/research/measurements/musl-opath-v1): appended to crates/init/src/managed.rs
/// for one run, then taken away; run alone (`--exact`), so that no other test's child holds
/// the owner pipe's end.
#[cfg(test)]
mod probe {
    #![allow(clippy::unwrap_used)]
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

    fn state(pid: libc::pid_t) -> Option<u8> {
        let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
        let end = stat.iter().rposition(|&b| b == b')')?;
        stat.get(end + 2).copied()
    }

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: pipe2(2) into a two-element array.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: both just made, owned here alone.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    /// Waits until `pid` sleeps, or is no longer running, or 10 s pass; whether it sleeps.
    fn asleep(pid: libc::pid_t) -> bool {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < until {
            match state(pid) {
                Some(b'S') => return true,
                Some(b'R' | b'D') => std::thread::sleep(std::time::Duration::from_millis(1)),
                _ => return false,
            }
        }
        false
    }

    /// The reaper's own loop (`reaping`) under its own signals and filter (`signals`,
    /// `filter`), its owner's pipe at its descriptor and a child of its own to wait for, as
    /// a domain's: stopped as it sleeps in its wait, continued, and then its owner gone,
    /// which it ends at (125).
    #[test]
    fn the_reaper_stopped_in_its_wait_lives_on() {
        let (owner_r, owner_w) = pipe();
        let (ready_r, ready_w) = pipe();
        // SAFETY: fork(2); the child makes raw calls alone, then runs the reaper's loop.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // SAFETY: as above, in the child alone.
            unsafe {
                libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
                libc::close(owner_w.as_raw_fd());
                if libc::dup2(owner_r.as_raw_fd(), super::Fd::Owner as i32) < 0 {
                    libc::_exit(2);
                }
                let me = libc::getpid();
                let held = libc::fork();
                if held == 0 {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
                    if libc::getppid() == me {
                        loop {
                            libc::pause();
                        }
                    }
                    libc::_exit(0);
                }
                if held < 0
                    || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                    || !super::signals()
                    || !super::filter()
                {
                    libc::_exit(3);
                }
                if libc::write(ready_w.as_raw_fd(), b"r".as_ptr().cast(), 1) != 1 {
                    libc::_exit(4);
                }
                super::reaping();
            }
        }
        drop((owner_r, ready_w));
        let mut byte = [0u8; 1];
        // SAFETY: read(2) into a byte of ours.
        assert_eq!(unsafe { libc::read(ready_r.as_raw_fd(), byte.as_mut_ptr().cast(), 1) }, 1);
        let slept = asleep(pid);
        let mut status = 0;
        let mut stopped = false;
        if slept {
            // SAFETY: kill(2) and waitpid(2) of the child forked above.
            unsafe {
                assert_eq!(libc::kill(pid, libc::SIGSTOP), 0);
                assert_eq!(libc::waitpid(pid, &raw mut status, libc::WUNTRACED), pid);
                stopped = libc::WIFSTOPPED(status);
                assert_eq!(libc::kill(pid, libc::SIGCONT), 0);
            }
        }
        let slept_again = asleep(pid);
        drop(owner_w);
        // SAFETY: waitpid(2) of the child forked above.
        assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
        let signal = if libc::WIFSIGNALED(status) { libc::WTERMSIG(status) } else { 0 };
        eprintln!(
            "REAPER slept={slept} stopped={stopped} slept_again={slept_again} exit={} signal={signal} ({})",
            libc::WEXITSTATUS(status),
            std::env::consts::ARCH
        );
        assert!(stopped && slept_again);
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 125);
    }
}
