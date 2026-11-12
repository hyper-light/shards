
/// Probe (docs/research/measurements/musl-opath-v1): appended to crates/net/src/confine.rs
/// for one run on x86_64, the only architecture the network process's filter is installed on,
/// then taken away.
#[cfg(all(test, target_arch = "x86_64"))]
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

    /// The network process's poller under its filter (`program`, `install`), waiting 30 s at
    /// most each time for a pipe it watches: stopped as it sleeps in its wait, continued,
    /// then the pipe written, which it ends at (0); its filter's refusals end it with
    /// shards_syscall_filter::REFUSED.
    #[test]
    fn the_network_process_stopped_in_its_wait_lives_on() {
        let program = super::program().unwrap();
        let (watched, writer) = pipe();
        let (ready_r, ready_w) = pipe();
        let mut events = Vec::with_capacity(16);
        // SAFETY: fork(2); the child loads its filter, then polls as the network process does.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // SAFETY: as above, in the child alone; exit_group(2) ends it.
            unsafe {
                libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
                libc::close(writer.as_raw_fd());
                if super::install(&program).is_err() {
                    libc::syscall(libc::SYS_exit_group, 2);
                }
                let Ok(poller) = crate::poll::Poller::new() else {
                    libc::syscall(libc::SYS_exit_group, 3);
                    return;
                };
                let read = crate::poll::Interest::READ;
                if poller.set(watched.as_raw_fd(), 7, crate::poll::Interest::NONE, read).is_err()
                    || libc::write(ready_w.as_raw_fd(), b"r".as_ptr().cast(), 1) != 1
                {
                    libc::syscall(libc::SYS_exit_group, 4);
                }
                loop {
                    events.clear();
                    let waited = poller.wait(&mut events, Some(std::time::Duration::from_secs(30)));
                    if waited.is_err() {
                        libc::syscall(libc::SYS_exit_group, 5);
                    }
                    if events.iter().any(|e| e.token == 7) {
                        libc::syscall(libc::SYS_exit_group, 0);
                    }
                }
            }
        }
        drop((watched, ready_w));
        let mut byte = [0u8; 1];
        // SAFETY: read(2) into a byte of ours.
        assert_eq!(unsafe { libc::read(ready_r.as_raw_fd(), byte.as_mut_ptr().cast(), 1) }, 1);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut slept = false;
        while std::time::Instant::now() < until {
            match state(pid) {
                Some(b'S') => {
                    slept = true;
                    break;
                }
                Some(b'R' | b'D') => std::thread::sleep(std::time::Duration::from_millis(1)),
                _ => break,
            }
        }
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
        std::thread::sleep(std::time::Duration::from_millis(100));
        // SAFETY: write(2) of a byte of ours to the watched pipe.
        assert_eq!(unsafe { libc::write(writer.as_raw_fd(), b"w".as_ptr().cast(), 1) }, 1);
        // SAFETY: waitpid(2) of the child forked above.
        assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
        let signal = if libc::WIFSIGNALED(status) { libc::WTERMSIG(status) } else { 0 };
        eprintln!(
            "NETWORK slept={slept} stopped={stopped} exit={} signal={signal}",
            libc::WEXITSTATUS(status)
        );
        assert!(stopped);
        assert!(libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0);
    }
}
