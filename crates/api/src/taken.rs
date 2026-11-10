//! A guest's VM process found again by its [`Identity`](crate::Identity), for
//! [`Guests::attach`](crate::Guests::attach): by its pid and the start that tells it from a
//! process given that pid since, held from before that start is read, so that its end is
//! seen and a kill reaches it alone. It is another process's child (the client's that booted
//! it, or the one that process left it to), so its exit status is that parent's, never this
//! client's.

use std::io;

#[cfg(target_os = "linux")]
use linux as os;
#[cfg(not(target_os = "linux"))]
use macos as os;

/// The start of process `pid`, as its OS counts it, set as it is made and kept through
/// exec: Linux's `/proc/<pid>/stat` starttime (clock ticks after boot, proc(5)); macOS's
/// `proc_pid_rusage` `ri_proc_start_abstime` (Mach absolute time).
pub(crate) fn started(pid: u32) -> io::Result<u64> {
    os::started(pid)
}

/// A process found by its pid and start.
#[derive(Debug)]
pub(crate) struct Held {
    os: os::Held,
    pid: u32,
}

impl Held {
    /// Process `pid`, where it started at `started` and has not exited; else why none is.
    /// Held before its start is read: a process given `pid` once it has gone shows another
    /// start, and one that has gone since shows its end.
    pub(crate) fn find(pid: u32, started: u64) -> Result<Held, String> {
        let os = os::Held::open(pid).map_err(|e| format!("no process has pid {pid}: {e}"))?;
        let now = os::started(pid).map_err(|e| format!("pid {pid}'s start: {e}"))?;
        if now != started {
            return Err(format!(
                "pid {pid} started at {now}, not {started}: another process's since"
            ));
        }
        if os.exited(false).map_err(|e| format!("pid {pid}: {e}"))? {
            return Err(format!("its VM's process, pid {pid}, has ended"));
        }
        Ok(Held { os, pid })
    }

    /// Waits for it to exit.
    pub(crate) fn exited(&self) -> io::Result<()> {
        self.os.exited(true).map(drop)
    }

    /// Kills it (SIGKILL): on Linux through its pidfd, which no process given its pid since
    /// receives; on macOS, which has none, by its pid, which its caller sends only while its
    /// exit has not been seen.
    pub(crate) fn kill(&self) {
        self.os.kill(self.pid);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsStr;
    use std::io::{self, Read as _};
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;

    #[cfg(test)]
    mod tests {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/research/measurements/managed-proc-main/repro.rs"
        ));
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/research/measurements/managed-proc-main/reader-tests.rs"
        ));
    }

    pub(super) fn started(pid: u32) -> io::Result<u64> {
        let mut path = [0; STAT_PATH_BYTES];
        let stat = read_stat(std::fs::File::open(stat_path(pid, &mut path)?)?)?;
        start_field(&stat)
    }

    const PREFIX: &[u8] = b"/proc/";
    const SUFFIX: &[u8] = b"/stat";
    // procfs uses decimal u32 PID input; zero also needs one digit. No pathname
    // beyond these literals and u32::MAX's decimal width can be produced here.
    const STAT_PATH_BYTES: usize = PREFIX.len() + (u32::MAX.ilog10() as usize + 1) + SUFFIX.len();

    fn stat_path(mut pid: u32, bytes: &mut [u8; STAT_PATH_BYTES]) -> io::Result<&Path> {
        let mut at = bytes.len();
        for byte in SUFFIX.iter().rev() {
            at = at.checked_sub(1).ok_or(io::ErrorKind::InvalidData)?;
            *bytes.get_mut(at).ok_or(io::ErrorKind::InvalidData)? = *byte;
        }
        loop {
            let digit = u8::try_from(pid % 10).map_err(|_| io::ErrorKind::InvalidData)?;
            at = at.checked_sub(1).ok_or(io::ErrorKind::InvalidData)?;
            *bytes.get_mut(at).ok_or(io::ErrorKind::InvalidData)? =
                b'0'.checked_add(digit).ok_or(io::ErrorKind::InvalidData)?;
            pid /= 10;
            if pid == 0 {
                break;
            }
        }
        for byte in PREFIX.iter().rev() {
            at = at.checked_sub(1).ok_or(io::ErrorKind::InvalidData)?;
            *bytes.get_mut(at).ok_or(io::ErrorKind::InvalidData)? = *byte;
        }
        Ok(Path::new(OsStr::from_bytes(
            bytes.get(at..).ok_or(io::ErrorKind::InvalidData)?,
        )))
    }

    fn read_stat(mut file: std::fs::File) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        loop {
            // One further byte to observe data/EOF; growth is fallible even for
            // procfs's zero metadata size, unlike std's initial small probe.
            bytes
                .try_reserve(1)
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
            let used = bytes.len();
            // Only initialize already-owned capacity; resize cannot grow here.
            bytes.resize(bytes.capacity(), 0);
            let count = loop {
                match file.read(bytes.get_mut(used..).ok_or(io::ErrorKind::InvalidData)?) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    result => break result?,
                }
            };
            let filled = used
                .checked_add(count)
                .filter(|filled| *filled <= bytes.len())
                .ok_or(io::ErrorKind::InvalidData)?;
            bytes.truncate(filled);
            if count == 0 {
                return Ok(bytes);
            }
        }
    }

    fn start_field(stat: &[u8]) -> io::Result<u64> {
        // Its command, field 2, is in parentheses and may hold any byte but NUL, a ')' too:
        // fields 3 on follow the last ')', starttime the 20th of them (field 22).
        // Only that decimal field needs UTF-8 validation; comm is raw kernel bytes.
        stat.iter()
            .rposition(|byte| *byte == b')')
            .and_then(|i| stat.get(i.saturating_add(1)..))
            .and_then(|fields| {
                fields
                    .split(u8::is_ascii_whitespace)
                    .filter(|field| !field.is_empty())
                    .nth(19)
            })
            .and_then(|field| std::str::from_utf8(field).ok())
            .and_then(|f| f.parse().ok())
            .ok_or_else(|| io::ErrorKind::InvalidData.into())
    }

    /// A process held by a pidfd (pidfd_open(2), Linux 5.3), readable once it has exited.
    #[derive(Debug)]
    pub(super) struct Held(OwnedFd);

    impl Held {
        pub(super) fn open(pid: u32) -> io::Result<Held> {
            let pid = libc::pid_t::try_from(pid).map_err(io::Error::other)?;
            // SAFETY: pidfd_open(2) of `pid`, no flags; the descriptor is close-on-exec.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            let fd = libc::c_int::try_from(fd).map_err(io::Error::other)?;
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the descriptor pidfd_open just made, owned from here alone.
            Ok(Held(unsafe { OwnedFd::from_raw_fd(fd) }))
        }

        /// Whether it has exited: `wait`ing until it has, else as it is now.
        pub(super) fn exited(&self, wait: bool) -> io::Result<bool> {
            let mut fd = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            loop {
                // SAFETY: poll(2) of one pollfd, of the pidfd this holds open.
                let n = unsafe { libc::poll(&raw mut fd, 1, if wait { -1 } else { 0 }) };
                if n >= 0 {
                    return Ok(n > 0);
                }
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }

        pub(super) fn kill(&self, _pid: u32) {
            // SAFETY: pidfd_send_signal(2) of SIGKILL through the pidfd this holds open, no
            // siginfo, no flags.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.0.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod macos {
    use std::io;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

    pub(super) fn started(pid: u32) -> io::Result<u64> {
        let pid = libc::c_int::try_from(pid).map_err(io::Error::other)?;
        // SAFETY: a rusage_info_v0, plain data, for proc_pid_rusage to fill.
        let mut info: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
        // SAFETY: proc_pid_rusage(3) of `pid` into a rusage_info_v0 of ours, which
        // RUSAGE_INFO_V0 names.
        let r = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, (&raw mut info).cast()) };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.ri_proc_start_abstime)
    }

    /// A process held by a kqueue with its exit registered (kqueue(2): EVFILT_PROC,
    /// NOTE_EXIT), which a process may register for another of its user's.
    #[derive(Debug)]
    pub(super) struct Held(OwnedFd);

    impl Held {
        pub(super) fn open(pid: u32) -> io::Result<Held> {
            let ident = usize::try_from(pid).map_err(io::Error::other)?;
            // SAFETY: kqueue(2), no arguments.
            let kq = unsafe { libc::kqueue() };
            if kq < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the descriptor kqueue just made, owned from here alone.
            let kq = unsafe { OwnedFd::from_raw_fd(kq) };
            let change = libc::kevent {
                ident,
                filter: libc::EVFILT_PROC,
                flags: libc::EV_ADD,
                fflags: libc::NOTE_EXIT,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            // SAFETY: kevent(2) registering one change on the kqueue this owns, asking for no
            // events: an error is returned, not placed in an event.
            let r = unsafe {
                libc::kevent(
                    kq.as_raw_fd(),
                    &raw const change,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            if r < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Held(kq))
        }

        /// Whether it has exited: `wait`ing until it has, else as it is now.
        pub(super) fn exited(&self, wait: bool) -> io::Result<bool> {
            let now = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            let at: *const libc::timespec = if wait { std::ptr::null() } else { &raw const now };
            loop {
                // SAFETY: an event, plain data, for kevent to fill.
                let mut event: libc::kevent = unsafe { std::mem::zeroed() };
                // SAFETY: kevent(2) waiting for one event on the kqueue this holds, into one of
                // ours, for as long as `at` says (null: until one comes).
                let n =
                    unsafe { libc::kevent(self.0.as_raw_fd(), std::ptr::null(), 0, &raw mut event, 1, at) };
                // The one event it has registered is the exit.
                if n >= 0 {
                    return Ok(n > 0);
                }
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }

        pub(super) fn kill(&self, pid: u32) {
            if let Ok(pid) = libc::pid_t::try_from(pid) {
                // SAFETY: kill(2) of `pid`, sent while its exit has not been seen.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }
}
