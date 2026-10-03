//! Messages between shards' processes over Unix-domain stream sockets, with open file
//! descriptors passed alongside (`SCM_RIGHTS`).
//!
//! A message is a kind byte, a big-endian u32 payload length, then the payload. Its
//! descriptors travel with its first byte, so a stream never splits them from their
//! message. Received descriptors are close-on-exec: atomically where the kernel offers
//! `MSG_CMSG_CLOEXEC` (Linux), right after receipt elsewhere.
//!
//! A sender keeps its own descriptor for a socket it passes until the receiver says it
//! has it. macOS's collector of in-flight descriptors (`unp_gc`) marks as reachable only
//! what it finds from other sockets in flight, so a socket in flight that no process
//! holds any more is flushed, and reads end of stream from then on, if a collection runs
//! before it is received. Every freed Unix socket starts one (docs/research/
//! platform-measurements.md M24).

use std::ffi::{CString, OsStr};
use std::io::{self, Write};
use std::mem::{MaybeUninit, size_of};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use crate::{Identity, MAX_FDS, MAX_PAYLOAD};

/// The signals `docker run` forwards to the container with `--sig-proxy`, its default:
/// every one but SIGCHLD, SIGPIPE, SIGURG and those the daemon cannot name (docker/cli
/// cli/command/container/signals.go). These are the ones other processes send, each
/// paired with its Linux number: the guest is Linux, and Docker forwards by name.
pub const FORWARDED: [(libc::c_int, u32); 18] = [
    (libc::SIGHUP, 1),
    (libc::SIGINT, 2),
    (libc::SIGQUIT, 3),
    (libc::SIGABRT, 6),
    (libc::SIGUSR1, 10),
    (libc::SIGUSR2, 12),
    (libc::SIGALRM, 14),
    (libc::SIGTERM, 15),
    (libc::SIGCONT, 18),
    (libc::SIGTSTP, 20),
    (libc::SIGTTIN, 21),
    (libc::SIGTTOU, 22),
    (libc::SIGXCPU, 24),
    (libc::SIGXFSZ, 25),
    (libc::SIGVTALRM, 26),
    (libc::SIGPROF, 27),
    (libc::SIGWINCH, 28),
    (libc::SIGIO, 29),
];

/// The signals to forward: all of [`FORWARDED`], but SIGTTIN when this process reads its
/// terminal. Left unblocked, SIGTTIN stops a reader in the background, as the terminal's
/// job control stops any; blocked, the read would fail with EIO instead (POSIX.1-2024,
/// 11.1.4 Terminal Access Control).
pub fn forwarded(reads_terminal: bool) -> impl Iterator<Item = (libc::c_int, u32)> {
    FORWARDED
        .into_iter()
        .filter(move |&(sig, _)| !(reads_terminal && sig == libc::SIGTTIN))
}

/// The handler that keeps a blocked signal pending for sigwait(3) on XNU; it never runs.
extern "C" fn kept_for_sigwait(_: libc::c_int) {}

/// Readies this process to take the signals it forwards (`forwarded(reads_terminal)`)
/// with sigwait(3). It blocks them in the calling thread, whose later threads inherit the
/// mask, then makes any that this process was started ignoring default again, as Go's
/// `signal.Notify` does for the Docker CLI's signal proxy (os/signal): XNU drops an
/// ignored signal when it is sent, even to a thread in sigwait (bsd/kern/kern_sig.c,
/// psignal_internal), and a non-interactive shell starts `cmd &` with SIGINT and SIGQUIT
/// ignored (POSIX.1-2024, XCU 2.9.3.1). XNU drops SIGWINCH and SIGIO at their default
/// too, whose default is to ignore them (setsigvec adds them to p_sigignore): they get a
/// handler, which never runs while they stay blocked, as Go gives every signal it is
/// notified of (platform-measurements M31). Returns the set to wait on, and the signals
/// that were ignored.
pub fn take_forwarded(reads_terminal: bool) -> io::Result<(libc::sigset_t, Vec<libc::c_int>)> {
    // SAFETY: sigset operations on a local set, pthread_sigmask on this thread, and
    // sigaction(2) reads and writes of dispositions, on valid structures.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for (sig, _) in forwarded(reads_terminal) {
            libc::sigaddset(&mut set, sig);
        }
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut ignored = Vec::new();
        for (sig, _) in forwarded(reads_terminal) {
            if sig == libc::SIGWINCH || sig == libc::SIGIO {
                let mut kept: libc::sigaction = std::mem::zeroed();
                kept.sa_sigaction = kept_for_sigwait as extern "C" fn(libc::c_int) as libc::sighandler_t;
                kept.sa_flags = libc::SA_RESTART;
                let mut was: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, &kept, &mut was) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if was.sa_sigaction == libc::SIG_IGN {
                    ignored.push(sig);
                }
                continue;
            }
            let mut was: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, std::ptr::null(), &mut was) != 0 {
                return Err(io::Error::last_os_error());
            }
            if was.sa_sigaction == libc::SIG_IGN {
                let mut default: libc::sigaction = std::mem::zeroed();
                default.sa_sigaction = libc::SIG_DFL;
                if libc::sigaction(sig, &default, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error());
                }
                ignored.push(sig);
            }
        }
        Ok((set, ignored))
    }
}

/// The binary that runs each microVM, beside the daemon's (`shards-vm`).
pub fn vm_binary(daemon: &Path) -> PathBuf {
    daemon.with_file_name(format!("shards-vm{}", std::env::consts::EXE_SUFFIX))
}

impl Identity {
    /// The build a daemon binary belongs to, as clients and the daemon tell builds apart:
    /// its file's identity folded with that of the VM binary beside it, which runs the
    /// daemon's VMs, so that either rebuilt is another build.
    pub fn of_build(daemon: &Path) -> io::Result<Identity> {
        let (d, v) = (Identity::of(daemon)?, Identity::of(&vm_binary(daemon))?);
        Ok(Identity {
            dev: d.dev ^ v.dev.rotate_left(17),
            ino: d.ino ^ v.ino.rotate_left(29),
            size: d.size ^ v.size.rotate_left(37),
            mtime_s: d.mtime_s ^ v.mtime_s.rotate_left(41),
            mtime_ns: d.mtime_ns ^ v.mtime_ns.rotate_left(13),
        })
    }

    pub fn of(path: &Path) -> io::Result<Identity> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path)?;
        Ok(Identity {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime_s: m.mtime(),
            mtime_ns: u32::try_from(m.mtime_nsec()).unwrap_or(0),
        })
    }
}

const HEADER: usize = 5;
/// Control-message room for the most descriptors a kernel passes in one message: 254 on
/// macOS and 253 on Linux (docs/research/warm-pool-daemon.md §2.6), whose `CMSG_SPACE` is
/// 1028 and 1032 bytes. A receiver with less room gets the rest installed all the same on
/// macOS, where nothing closes them.
const CONTROL: usize = 1040;

/// A received message.
#[derive(Debug)]
pub struct Message {
    pub kind: u8,
    pub payload: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}

/// Room for control messages, aligned as their header requires.
#[repr(C, align(8))]
struct Control([MaybeUninit<u8>; CONTROL]);

impl Control {
    fn new() -> Control {
        Control([MaybeUninit::uninit(); CONTROL])
    }
}

fn space(fds: usize) -> usize {
    let bytes = u32::try_from(fds * size_of::<RawFd>()).unwrap_or(u32::MAX);
    // SAFETY: CMSG_SPACE only computes a size.
    unsafe { libc::CMSG_SPACE(bytes) as usize }
}

/// Sends one message with `fds` attached. Blocks until all of it is written.
pub fn send(sock: &UnixStream, kind: u8, payload: &[u8], fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    if payload.len() > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "message payload too large",
        ));
    }
    if fds.len() > MAX_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many descriptors for one message",
        ));
    }
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("payload length"))?;
    let [l0, l1, l2, l3] = len.to_be_bytes();
    let mut header = [kind, l0, l1, l2, l3];
    let mut iov = [
        libc::iovec {
            iov_base: header.as_mut_ptr().cast(),
            iov_len: HEADER,
        },
        libc::iovec {
            iov_base: payload.as_ptr() as *mut libc::c_void,
            iov_len: payload.len(),
        },
    ];
    let mut control = Control::new();
    // SAFETY: an all-zero msghdr is valid; the pointers set below outlive the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_mut_ptr();
    msg.msg_iovlen = 2;
    if !fds.is_empty() {
        let bytes = fds.len() * size_of::<RawFd>();
        msg.msg_control = control.0.as_mut_ptr().cast();
        msg.msg_controllen = space(fds.len()) as _;
        // SAFETY: the control buffer is aligned and holds CMSG_SPACE(bytes) bytes, so the
        // first header and its data fit.
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            if cmsg.is_null() {
                return Err(io::Error::other("no room for descriptors"));
            }
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(bytes as u32) as _;
            let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
            for (i, fd) in fds.iter().enumerate() {
                data.add(i).write_unaligned(fd.as_raw_fd());
            }
        }
    }
    let total = HEADER + payload.len();
    let sent = loop {
        // SAFETY: a fully initialized msghdr over live buffers.
        let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, 0) };
        if let Ok(n) = usize::try_from(n) {
            break n;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    // The descriptors went with the first byte; the rest follows as plain bytes.
    if sent < total {
        write_rest(&mut &*sock, [&header, payload], sent)?;
    }
    Ok(())
}

/// Writes what follows the first `sent` bytes of `parts`, from where they are, where a
/// signal cut a `sendmsg` short (audit D10).
fn write_rest(w: &mut impl Write, parts: [&[u8]; 2], sent: usize) -> io::Result<()> {
    let mut skip = sent;
    for part in parts {
        w.write_all(part.get(skip.min(part.len())..).unwrap_or_default())?;
        skip = skip.saturating_sub(part.len());
    }
    Ok(())
}

/// Receives one message. `None` if the peer closed the stream between messages.
///
/// Every read is a `recvmsg` with room for all the descriptors a kernel passes at once, so
/// descriptors arriving with any part of a message become this process's to close.
pub fn recv(sock: &UnixStream) -> io::Result<Option<Message>> {
    recv_until(sock, None)
}

/// [`recv`], if all of the message arrives by `deadline`; `TimedOut` then otherwise. A
/// peer that sends nothing, or a message a byte at a time, gains nothing by it: the
/// deadline bounds the whole message, where a timeout would bound each read.
pub fn recv_by(sock: &UnixStream, deadline: Instant) -> io::Result<Option<Message>> {
    recv_until(sock, Some(deadline))
}

fn recv_until(sock: &UnixStream, deadline: Option<Instant>) -> io::Result<Option<Message>> {
    let mut fds = Vec::new();
    let mut header = [0u8; HEADER];
    let got = recv_part(sock, &mut header, &mut fds, deadline)?;
    if got == 0 {
        return if fds.is_empty() {
            Ok(None)
        } else {
            Err(io::ErrorKind::UnexpectedEof.into())
        };
    }
    fill(
        sock,
        header.get_mut(got..).unwrap_or_default(),
        &mut fds,
        deadline,
    )?;
    let [kind, l0, l1, l2, l3] = header;
    let len = u32::from_be_bytes([l0, l1, l2, l3]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message payload too large",
        ));
    }
    let mut payload = vec![0u8; len];
    fill(sock, &mut payload, &mut fds, deadline)?;
    if fds.len() > MAX_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "more descriptors than a message may carry",
        ));
    }
    Ok(Some(Message { kind, payload, fds }))
}

/// Reads exactly `buf.len()` more bytes of a message.
fn fill(
    sock: &UnixStream,
    mut buf: &mut [u8],
    fds: &mut Vec<OwnedFd>,
    deadline: Option<Instant>,
) -> io::Result<()> {
    while !buf.is_empty() {
        let n = recv_part(sock, buf, fds, deadline)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf = buf.get_mut(n..).unwrap_or_default();
    }
    Ok(())
}

/// Waits until `sock` has something to read, or has ended; `TimedOut` at `deadline`.
fn await_readable(sock: &UnixStream, deadline: Instant) -> io::Result<()> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        // Rounded up: a wait of 0 ms would return at once, and spin.
        let ms = libc::c_int::try_from(left.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX);
        let mut pfd = libc::pollfd {
            fd: sock.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll(2) on one valid pollfd.
        match unsafe { libc::poll(&mut pfd, 1, ms) } {
            // Data, its end or an error: the read says which.
            n if n > 0 => return Ok(()),
            0 => {}
            _ => {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
    }
}

/// One `recvmsg` into `buf`, once something arrives, by `deadline` if there is one.
/// Returns the bytes read, and adds the descriptors that came with them to `fds`, owned
/// and close-on-exec.
fn recv_part(
    sock: &UnixStream,
    buf: &mut [u8],
    fds: &mut Vec<OwnedFd>,
    deadline: Option<Instant>,
) -> io::Result<usize> {
    if let Some(deadline) = deadline {
        await_readable(sock, deadline)?;
    }
    let mut control = Control::new();
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // SAFETY: as in `send`.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.0.as_mut_ptr().cast();
    msg.msg_controllen = CONTROL as _;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = 0;
    let got = loop {
        // SAFETY: a fully initialized msghdr over live buffers.
        let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, flags) };
        if let Ok(n) = usize::try_from(n) {
            break n;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    // Owned first, so that each is closed on any error below.
    take_fds(&msg, fds)?;
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "control data truncated",
        ));
    }
    Ok(got)
}

/// Adds the descriptors in `msg`'s control data to `fds`, owned and close-on-exec. Only
/// what the kernel wrote is read: a truncated header still states its full length on
/// macOS.
fn take_fds(msg: &libc::msghdr, fds: &mut Vec<OwnedFd>) -> io::Result<()> {
    let first = fds.len();
    // `msg_controllen` is a size_t on glibc and a socklen_t elsewhere.
    #[allow(clippy::unnecessary_cast)]
    let end = (msg.msg_control as usize).saturating_add(msg.msg_controllen as usize);
    // SAFETY: walking the control messages the kernel wrote into our buffer, within
    // `msg_controllen`; each SCM_RIGHTS descriptor is new to this process and owned by
    // nothing else.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg);
                let stated = ((*cmsg).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                let written = end.saturating_sub(data as usize);
                for i in 0..stated.min(written) / size_of::<RawFd>() {
                    fds.push(OwnedFd::from_raw_fd(data.cast::<RawFd>().add(i).read_unaligned()));
                }
            }
            cmsg = libc::CMSG_NXTHDR(msg, cmsg);
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    for fd in fds.get(first..).unwrap_or_default() {
        // SAFETY: fcntl(2) on a descriptor we own.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let _ = first;
    Ok(())
}

/// The daemon's socket: `daemon.sock` in shards' home, named relative to the working
/// directory, which each process that uses it makes the home first. A relative name fits
/// a socket address (`sun_path`: 104 bytes on macOS, 108 on Linux, NUL included) whatever
/// the home's path. The per-user directories macOS offers instead cost each new process a
/// lookup of 0.4–1.3 ms (docs/research/platform-measurements.md M26).
pub const SOCKET: &str = "daemon.sock";

/// What a daemon that no longer serves keeps in its home while it ends its runs: its
/// process ID. A daemon or client that finds none listening waits for that one while it
/// lives, as long as its runs' stop timeouts make it, and then takes its place.
pub const STOPPING: &str = "daemon.stopping";

/// Whether the daemon that `STOPPING` in `home` names still lives, ending its runs.
pub fn exiting(home: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(home.join(STOPPING)) else {
        return false;
    };
    // Zero or less would name a process group, or every process.
    let Some(pid) = text.trim().parse::<libc::pid_t>().ok().filter(|&pid| pid > 0) else {
        return false;
    };
    // SAFETY: kill(2) with signal 0 only asks whether the process exists.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    alive || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Where a daemon the client starts writes its messages.
pub fn log(home: &Path) -> PathBuf {
    home.join("daemon.log")
}

/// This process's peak resident set so far, in KiB; 0 where unknown. On Linux it is the
/// address space's own high-water mark (`VmHWM`, proc(5)): `getrusage`'s `ru_maxrss`
/// starts from the spawner's, since exec keeps the peak of the address space it
/// replaces, which under `posix_spawn` is the parent's (fs/exec.c `exec_mmap`). macOS's
/// `posix_spawn` starts a new task, and `ru_maxrss` is its own (in bytes there).
pub fn peak_rss_kib() -> u64 {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                let line = status.lines().find_map(|l| l.strip_prefix("VmHWM:"))?;
                line.trim().strip_suffix("kB")?.trim().parse().ok()
            })
            .unwrap_or(0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: getrusage(2) into a zeroed rusage, a valid out-parameter.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
            return 0;
        }
        let rss = u64::try_from(usage.ru_maxrss).unwrap_or(0);
        if cfg!(target_vendor = "apple") {
            rss / 1024
        } else {
            rss
        }
    }
}

/// The effective user ID of the process at the other end of `sock`, as it connected:
/// `SO_PEERCRED` on Linux (unix(7)), `getpeereid(3)` elsewhere.
pub fn peer_uid(sock: &UnixStream) -> io::Result<u32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: an all-zero ucred is valid; getsockopt(2) fills at most `len` bytes of it.
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: as above, on a socket we own.
        let r = unsafe {
            libc::getsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut cred).cast(),
                &mut len,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(cred.uid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: getpeereid(3) into two locals, on a socket we own.
        if unsafe { libc::getpeereid(sock.as_raw_fd(), &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid)
    }
}

/// Apple's `POSIX_SPAWN_SETSID` (sys/spawn.h), which the libc crate does not define.
#[cfg(target_vendor = "apple")]
const SETSID: libc::c_int = 0x0400;

/// A child started by [`spawn`]. Waiting reaps it; dropping it does not. One thread may
/// wait while others signal it: its pid stays the child's until `wait` has marked it
/// reaped, so a signal never reaches another process that reuses the pid.
#[derive(Debug)]
pub struct Child {
    pid: libc::pid_t,
    reaped: Mutex<bool>,
}

impl Child {
    pub fn id(&self) -> u32 {
        self.pid.unsigned_abs()
    }

    /// Waits for the child to end: its exit status, or 128 plus the signal that ended it.
    pub fn wait(&self) -> io::Result<i32> {
        // Waits for the end without reaping (WNOWAIT): the pid is still the child's while
        // `kill` may look at it.
        loop {
            // SAFETY: an all-zero siginfo_t is valid; waitid(2) fills it for our child.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: as above.
            let r = unsafe { libc::waitid(libc::P_PID, self.id(), &mut info, libc::WEXITED | libc::WNOWAIT) };
            if r == 0 {
                break;
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        let mut reaped = self.reaped.lock().unwrap_or_else(PoisonError::into_inner);
        let mut status = 0;
        loop {
            // SAFETY: waitpid(2) for our own child, which has ended, into a local.
            let r = unsafe { libc::waitpid(self.pid, &mut status, 0) };
            if r == self.pid {
                break;
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        *reaped = true;
        if libc::WIFEXITED(status) {
            Ok(libc::WEXITSTATUS(status))
        } else {
            Ok(128 + libc::WTERMSIG(status))
        }
    }

    /// Whether the child has ended, without waiting: its status as [`Child::wait`] gives
    /// it once it has, `None` while it runs.
    pub fn try_wait(&self) -> Option<i32> {
        // SAFETY: an all-zero siginfo_t is valid; waitid(2) fills it for our child.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: as above; WNOHANG returns at once, with si_pid 0 if nothing ended.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                self.id(),
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        // SAFETY: si_pid is set by waitid when it reports a child.
        if r != 0 || unsafe { info.si_pid() } == 0 {
            return None;
        }
        self.wait().ok()
    }

    /// Sends `signal` to the child, unless it has been reaped.
    pub fn kill(&self, signal: libc::c_int) -> io::Result<()> {
        let reaped = self.reaped.lock().unwrap_or_else(PoisonError::into_inner);
        if *reaped {
            return Ok(());
        }
        // SAFETY: kill(2) of our own child, not reaped while we hold the lock.
        if unsafe { libc::kill(self.pid, signal) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Starts `program` with `args` and this process's environment. The child gets exactly the
/// descriptors in `fds`, each at the number paired with it, and nothing else this process
/// holds, whatever its close-on-exec flag says: macOS closes the rest in the child
/// (`POSIX_SPAWN_CLOEXEC_DEFAULT`), and on Linux every descriptor shards holds, received
/// ones included, is close-on-exec. Its signal mask is empty and every signal has its
/// default action.
///
/// With `detach`, the child leads a session of its own, out of reach of the signals a
/// terminal sends this process's group.
pub fn spawn(
    program: &Path,
    args: &[&OsStr],
    fds: &[(BorrowedFd<'_>, RawFd)],
    detach: bool,
) -> io::Result<Child> {
    spawn_with(program, args, fds, detach, &[])
}

/// [`spawn`], with the variables `set` in the child's environment in place of this
/// process's.
pub fn spawn_with(
    program: &Path,
    args: &[&OsStr],
    fds: &[(BorrowedFd<'_>, RawFd)],
    detach: bool,
    set: &[(&str, &OsStr)],
) -> io::Result<Child> {
    let inherited = std::env::vars_os().filter(|(k, _)| !set.iter().any(|(name, _)| k == *name));
    let given = set
        .iter()
        .map(|(k, v)| (OsStr::new(k).to_os_string(), v.to_os_string()));
    spawn_env(program, args, fds, detach, inherited.chain(given).collect())
}

/// [`spawn`], with the variables `env` alone for the child's environment: nothing of this
/// process's, whose own may hold what its child must not (a client's secrets, for a
/// process a guest could take over).
pub fn spawn_in(
    program: &Path,
    args: &[&OsStr],
    fds: &[(BorrowedFd<'_>, RawFd)],
    detach: bool,
    env: &[(&str, &OsStr)],
) -> io::Result<Child> {
    let env = env
        .iter()
        .map(|(k, v)| (OsStr::new(k).to_os_string(), v.to_os_string()))
        .collect();
    spawn_env(program, args, fds, detach, env)
}

fn spawn_env(
    program: &Path,
    args: &[&OsStr],
    fds: &[(BorrowedFd<'_>, RawFd)],
    detach: bool,
    env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> io::Result<Child> {
    let cstring = |s: &OsStr| {
        CString::new(s.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in an argument"))
    };
    let path = cstring(program.as_os_str())?;
    let mut argv = vec![path.clone()];
    for a in args {
        argv.push(cstring(a)?);
    }
    let env: Vec<CString> = env
        .into_iter()
        .map(|(k, v)| {
            let mut entry = k.as_bytes().to_vec();
            entry.push(b'=');
            entry.extend_from_slice(v.as_bytes());
            CString::new(entry)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in the environment"))
        })
        .collect::<io::Result<_>>()?;
    let mut argv_ptrs: Vec<*mut libc::c_char> = argv.iter().map(|a| a.as_ptr().cast_mut()).collect();
    argv_ptrs.push(std::ptr::null_mut());
    let mut env_ptrs: Vec<*mut libc::c_char> = env.iter().map(|e| e.as_ptr().cast_mut()).collect();
    env_ptrs.push(std::ptr::null_mut());
    let check = |rc: libc::c_int| {
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(rc))
        }
    };
    // SAFETY: the attribute and file-action objects are initialized before use and
    // destroyed once; every pointer passed lives until posix_spawn returns.
    unsafe {
        let mut actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
        check(libc::posix_spawn_file_actions_init(&mut actions))?;
        let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
        if let Err(e) = check(libc::posix_spawnattr_init(&mut attr)) {
            libc::posix_spawn_file_actions_destroy(&mut actions);
            return Err(e);
        }
        let result = (|| {
            // The file actions run in order, so a source whose number is another's target
            // would be overwritten before it is read; and one already at its own target
            // would keep its close-on-exec flag, which dup2 onto itself leaves set (macOS
            // then closes it at exec, under POSIX_SPAWN_CLOEXEC_DEFAULT). Each source whose
            // number is any target first moves above every number in play, then
            // everything goes where it belongs, a dup2 onto another number clearing the
            // flag, and the moved copies are closed.
            let targets: Vec<RawFd> = fds.iter().map(|(_, t)| *t).collect();
            let mut spare = fds
                .iter()
                .flat_map(|(fd, t)| [fd.as_raw_fd(), *t])
                .max()
                .unwrap_or(2)
                .saturating_add(1);
            let mut sources = Vec::with_capacity(fds.len());
            let mut moved = Vec::new();
            for (i, (fd, target)) in fds.iter().enumerate() {
                let src = fd.as_raw_fd();
                let _ = (i, target);
                if targets.contains(&src) {
                    check(libc::posix_spawn_file_actions_adddup2(&mut actions, src, spare))?;
                    sources.push(spare);
                    moved.push(spare);
                    spare = spare.saturating_add(1);
                } else {
                    sources.push(src);
                }
            }
            for (src, target) in sources.iter().zip(&targets) {
                check(libc::posix_spawn_file_actions_adddup2(
                    &mut actions,
                    *src,
                    *target,
                ))?;
            }
            for m in moved {
                if !targets.contains(&m) {
                    check(libc::posix_spawn_file_actions_addclose(&mut actions, m))?;
                }
            }
            let mut none: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut none);
            check(libc::posix_spawnattr_setsigmask(&mut attr, &none))?;
            let mut all: libc::sigset_t = std::mem::zeroed();
            libc::sigfillset(&mut all);
            check(libc::posix_spawnattr_setsigdefault(&mut attr, &all))?;
            #[cfg(target_vendor = "apple")]
            let (flags, setsid) = (
                libc::POSIX_SPAWN_SETSIGMASK
                    | libc::POSIX_SPAWN_SETSIGDEF
                    | libc::POSIX_SPAWN_CLOEXEC_DEFAULT,
                SETSID,
            );
            #[cfg(not(target_vendor = "apple"))]
            let (flags, setsid) = (
                libc::POSIX_SPAWN_SETSIGMASK | libc::POSIX_SPAWN_SETSIGDEF,
                libc::c_int::from(libc::POSIX_SPAWN_SETSID),
            );
            let flags = if detach { flags | setsid } else { flags };
            check(libc::posix_spawnattr_setflags(&mut attr, flags as libc::c_short))?;
            let mut pid = 0;
            check(libc::posix_spawn(
                &mut pid,
                path.as_ptr(),
                &actions,
                &attr,
                argv_ptrs.as_ptr(),
                env_ptrs.as_ptr(),
            ))?;
            Ok(Child {
                pid,
                reaped: Mutex::new(false),
            })
        })();
        libc::posix_spawnattr_destroy(&mut attr);
        libc::posix_spawn_file_actions_destroy(&mut actions);
        result
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::AsFd;

    use super::*;

    /// A pipe whose ends are close-on-exec, as every descriptor shards holds is: on Linux
    /// `spawn` relies on that to give a child nothing it was not given.
    fn pipe() -> (File, File) {
        let (r, w) = io::pipe().unwrap();
        (File::from(OwnedFd::from(r)), File::from(OwnedFd::from(w)))
    }

    #[test]
    fn messages_carry_payloads_and_working_descriptors() {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut r1, w1) = pipe();
        let (mut r2, w2) = pipe();
        send(&a, 7, b"request", &[w1.as_fd(), w2.as_fd()]).unwrap();
        drop((w1, w2));
        let m = recv(&b).unwrap().unwrap();
        assert_eq!(
            (m.kind, m.payload.as_slice(), m.fds.len()),
            (7, &b"request"[..], 2)
        );
        for (fd, (reader, text)) in m.fds.into_iter().zip([(&mut r1, "one"), (&mut r2, "two")]) {
            // SAFETY: fcntl(2) on a descriptor we own.
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert_eq!(
                flags & libc::FD_CLOEXEC,
                libc::FD_CLOEXEC,
                "received descriptors are close-on-exec"
            );
            File::from(fd).write_all(text.as_bytes()).unwrap();
            let mut got = String::new();
            reader.read_to_string(&mut got).unwrap();
            assert_eq!(got, text);
        }
    }

    #[test]
    fn empty_messages_and_clean_ends() {
        let (a, b) = UnixStream::pair().unwrap();
        send(&a, 1, &[], &[]).unwrap();
        drop(a);
        let m = recv(&b).unwrap().unwrap();
        assert_eq!((m.kind, m.payload.len(), m.fds.len()), (1, 0, 0));
        assert!(
            recv(&b).unwrap().is_none(),
            "a close between messages is a clean end"
        );
    }

    /// A deadline bounds the whole message: a peer that says nothing, or trickles a
    /// message out a byte at a time faster than any one read would time out, is let go at
    /// the deadline; a message sent in time arrives whole, however it is split.
    #[test]
    fn a_deadline_bounds_the_whole_message() {
        use std::time::Duration;
        let window = Duration::from_millis(200);
        let (_silent, b) = UnixStream::pair().unwrap();
        let start = Instant::now();
        assert_eq!(
            recv_by(&b, start + window).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        let waited = start.elapsed();
        assert!(waited >= window && waited < 10 * window, "{waited:?}");

        let (a, b) = UnixStream::pair().unwrap();
        let trickle = std::thread::spawn(move || {
            // The header, then a payload that would take 2 s at a byte per 20 ms.
            for byte in [4, 0, 0, 0, 100].into_iter().chain([b'x'; 100]) {
                if (&a).write_all(&[byte]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let start = Instant::now();
        assert_eq!(
            recv_by(&b, start + window).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        let waited = start.elapsed();
        assert!(waited >= window && waited < 10 * window, "{waited:?}");
        drop(b);
        trickle.join().unwrap();

        let (a, b) = UnixStream::pair().unwrap();
        let (_r, w) = pipe();
        let split = std::thread::spawn(move || {
            let mut whole = vec![5u8, 0, 0, 0, 3];
            whole.extend_from_slice(b"abc");
            send(&a, 5, b"abc", &[w.as_fd()]).unwrap();
            // And a second, a byte at a time, well within its deadline.
            for byte in whole {
                (&a).write_all(&[byte]).unwrap();
                std::thread::sleep(Duration::from_millis(2));
            }
            a
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        let m = recv_by(&b, deadline).unwrap().unwrap();
        assert_eq!((m.kind, m.payload.as_slice(), m.fds.len()), (5, &b"abc"[..], 1));
        let m = recv_by(&b, deadline).unwrap().unwrap();
        assert_eq!((m.kind, m.payload.as_slice(), m.fds.len()), (5, &b"abc"[..], 0));
        drop(split.join().unwrap());
        assert!(recv_by(&b, deadline).unwrap().is_none(), "a clean end");
    }

    #[test]
    fn an_end_inside_a_message_is_an_error() {
        let (a, b) = UnixStream::pair().unwrap();
        (&a).write_all(&[3, 0, 0, 0, 10, b'x']).unwrap();
        drop(a);
        assert_eq!(recv(&b).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    /// A send cut short anywhere, in its header or its payload, is finished byte for byte.
    #[test]
    fn a_send_cut_short_is_finished_from_where_it_stopped() {
        let (header, payload) = ([9u8, 0, 0, 0, 7], b"payload".as_slice());
        let whole = [&header[..], payload].concat();
        for sent in 0..whole.len() {
            let mut rest = Vec::new();
            write_rest(&mut rest, [&header, payload], sent).unwrap();
            assert_eq!(rest, whole[sent..], "cut at {sent}");
        }
    }

    #[test]
    fn large_payloads_arrive_whole_with_their_descriptors() {
        let (a, b) = UnixStream::pair().unwrap();
        let payload: Vec<u8> = (0..MAX_PAYLOAD).map(|i| i as u8).collect();
        let (mut r, w) = pipe();
        let sent = payload.clone();
        let writer = std::thread::spawn(move || send(&a, 9, &sent, &[w.as_fd()]).unwrap());
        let m = recv(&b).unwrap().unwrap();
        writer.join().unwrap();
        assert_eq!(m.payload, payload);
        File::from(m.fds.into_iter().next().unwrap())
            .write_all(b"ok")
            .unwrap();
        let mut got = [0u8; 2];
        r.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ok");
    }

    #[test]
    fn children_get_the_descriptors_they_are_given() {
        let (mut r, w) = pipe();
        let child = spawn(
            Path::new("/bin/sh"),
            &["-c".as_ref(), "echo given >&3".as_ref()],
            &[(w.as_fd(), 3)],
            false,
        )
        .unwrap();
        drop(w);
        assert_eq!(child.wait().unwrap(), 0);
        let mut got = String::new();
        r.read_to_string(&mut got).unwrap();
        assert_eq!(got, "given\n");
    }

    /// Descriptors go where they are given even when each one's number is the other's
    /// target, a swap, which file actions run in order would get wrong, or its own, close
    /// on exec as every descriptor of ours is; and nothing moved out of the way on the way
    /// is left in the child.
    #[test]
    fn children_get_swapped_descriptors_each_where_it_belongs() {
        let (mut ra, wa) = pipe();
        let (mut rb, wb) = pipe();
        let (a, b) = (wa.as_raw_fd(), wb.as_raw_fd());
        // a's writer goes to b's number, and b's to a's.
        let script = format!(
            "echo A >&{b}; echo B >&{a}; for fd in $(seq {} 64); do [ -e /dev/fd/$fd ] && echo leaked $fd >&{b}; done; exit 0",
            a.max(b) + 1
        );
        let child = spawn(
            Path::new("/bin/sh"),
            &["-c".as_ref(), script.as_ref()],
            &[(wa.as_fd(), b), (wb.as_fd(), a)],
            false,
        )
        .unwrap();
        drop((wa, wb));
        assert_eq!(child.wait().unwrap(), 0);
        let (mut got_a, mut got_b) = (String::new(), String::new());
        ra.read_to_string(&mut got_a).unwrap();
        rb.read_to_string(&mut got_b).unwrap();
        assert_eq!((got_a.as_str(), got_b.as_str()), ("A\n", "B\n"));
    }

    #[test]
    fn a_descriptor_given_its_own_number_reaches_the_child() {
        let (mut r, w) = pipe();
        let n = w.as_raw_fd();
        // SAFETY: fcntl(2) on a descriptor we own: close-on-exec, as ours all are.
        unsafe { libc::fcntl(n, libc::F_SETFD, libc::FD_CLOEXEC) };
        let script = format!("echo kept >&{n}");
        let child = spawn(
            Path::new("/bin/sh"),
            &["-c".as_ref(), script.as_ref()],
            &[(w.as_fd(), n)],
            false,
        )
        .unwrap();
        drop(w);
        assert_eq!(child.wait().unwrap(), 0);
        let mut got = String::new();
        r.read_to_string(&mut got).unwrap();
        assert_eq!(got, "kept\n");
    }

    /// Even a descriptor left inheritable stays behind (macOS: POSIX_SPAWN_CLOEXEC_DEFAULT).
    #[cfg(target_vendor = "apple")]
    #[test]
    fn children_get_nothing_else() {
        let (_r, w) = pipe();
        // SAFETY: fcntl(2) on a descriptor we own: clears close-on-exec.
        assert_eq!(unsafe { libc::fcntl(w.as_raw_fd(), libc::F_SETFD, 0) }, 0);
        let script = format!("[ -e /dev/fd/{} ] && exit 1; exit 0", w.as_raw_fd());
        let child = spawn(
            Path::new("/bin/sh"),
            &["-c".as_ref(), script.as_ref()],
            &[],
            false,
        )
        .unwrap();
        assert_eq!(
            child.wait().unwrap(),
            0,
            "the child inherited descriptor {}",
            w.as_raw_fd()
        );
    }

    /// A child spawned in an environment has that alone: what it is given, and nothing of
    /// this process's (its HOME, here).
    #[test]
    fn a_child_spawned_in_an_environment_has_it_alone() {
        assert!(
            std::env::var_os("HOME").is_some(),
            "the test needs a HOME to leave behind"
        );
        let script = r#"[ "$GIVEN" = yes ] && [ -z "$HOME" ] && exit 0; exit 1"#;
        let child = spawn_in(
            Path::new("/bin/sh"),
            &["-c".as_ref(), script.as_ref()],
            &[],
            false,
            &[("GIVEN", OsStr::new("yes"))],
        )
        .unwrap();
        assert_eq!(
            child.wait().unwrap(),
            0,
            "the child had more, or less, than it was given"
        );
    }

    #[test]
    fn statuses_and_signals() {
        let child = spawn(
            Path::new("/bin/sh"),
            &["-c".as_ref(), "exit 7".as_ref()],
            &[],
            false,
        )
        .unwrap();
        assert_eq!(child.wait().unwrap(), 7);
        let child = spawn(
            Path::new("/bin/sh"),
            &["-c".as_ref(), "sleep 30".as_ref()],
            &[],
            false,
        )
        .unwrap();
        child.kill(libc::SIGTERM).unwrap();
        assert_eq!(child.wait().unwrap(), 128 + libc::SIGTERM);
    }

    #[test]
    fn detached_children_lead_their_own_session() {
        // The test asks the kernel while the child waits for the pipe to close: `ps`
        // differs between systems, and BusyBox's has no `-p`.
        // SAFETY: getpgrp(2) and getsid(2) of this process.
        let (group, session) = unsafe { (libc::getpgrp(), libc::getsid(0)) };
        for detach in [false, true] {
            let (r, w) = pipe();
            let child = spawn(
                Path::new("/bin/sh"),
                &["-c".as_ref(), "read line <&3; exit 0".as_ref()],
                &[(r.as_fd(), 3)],
                detach,
            )
            .unwrap();
            let pid = child.pid;
            // SAFETY: getpgid(2) and getsid(2) of our own child, not yet reaped.
            let (its_group, its_session) = unsafe { (libc::getpgid(pid), libc::getsid(pid)) };
            drop(w);
            assert_eq!(child.wait().unwrap(), 0);
            if detach {
                assert_eq!(
                    (its_group, its_session),
                    (pid, pid),
                    "a detached child leads a session and a group of its own"
                );
            } else {
                assert_eq!(
                    (its_group, its_session),
                    (group, session),
                    "an attached child stays in ours"
                );
            }
        }
    }

    /// Sends `bytes` with `fds` attached in one `sendmsg`, bypassing `send`'s limits.
    fn send_raw(sock: &UnixStream, bytes: &[u8], fds: &[RawFd]) {
        let mut control = vec![0u64; space(fds.len()).div_ceil(8)];
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr() as *mut libc::c_void,
            iov_len: bytes.len(),
        };
        // SAFETY: a msghdr over live buffers, the control one aligned and large enough for
        // one header and `fds`.
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = space(fds.len()) as _;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as u32) as _;
            let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
            for (i, fd) in fds.iter().enumerate() {
                data.add(i).write_unaligned(*fd);
            }
            assert_eq!(libc::sendmsg(sock.as_raw_fd(), &msg, 0), bytes.len() as isize);
        }
    }

    /// Whether every copy of `w`, the write end of `r`, is closed once `w` is: then `r`
    /// reads end of file. On macOS that can lag the last close by a fraction of a
    /// millisecond (0.36 ms at most in these tests' runs), so a copy counts as open only
    /// if none arrives within a second.
    fn all_closed(mut r: File, w: File) -> bool {
        drop(w);
        // SAFETY: fcntl(2) on a descriptor we own.
        unsafe { libc::fcntl(r.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            match r.read(&mut [0u8; 1]) {
                Ok(0) => return true,
                _ if std::time::Instant::now() >= deadline => return false,
                _ => std::thread::sleep(std::time::Duration::from_millis(1)),
            }
        }
    }

    #[test]
    fn peers_are_known_by_their_user() {
        let (a, _b) = UnixStream::pair().unwrap();
        // SAFETY: geteuid(2) cannot fail.
        assert_eq!(peer_uid(&a).unwrap(), unsafe { libc::geteuid() });
    }

    #[test]
    fn descriptors_past_the_limit_are_closed() {
        let (a, b) = UnixStream::pair().unwrap();
        let (r, w) = pipe();
        send_raw(&a, &[1, 0, 0, 0, 0], &[w.as_raw_fd(); 200]);
        assert_eq!(recv(&b).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(all_closed(r, w), "a received descriptor was left open");
    }

    #[test]
    fn descriptors_anywhere_in_a_message_are_taken() {
        let (a, b) = UnixStream::pair().unwrap();
        let (r, w) = pipe();
        (&a).write_all(&[1, 0, 0, 0, 3]).unwrap();
        send_raw(&a, b"abc", &[w.as_raw_fd()]);
        let m = recv(&b).unwrap().unwrap();
        assert_eq!((m.payload.as_slice(), m.fds.len()), (&b"abc"[..], 1));
        drop(m);
        assert!(all_closed(r, w), "a received descriptor was left open");
    }

    #[test]
    fn a_reaped_child_is_never_signalled() {
        let child = std::sync::Arc::new(
            spawn(
                Path::new("/bin/sh"),
                &["-c".as_ref(), "sleep 30".as_ref()],
                &[],
                false,
            )
            .unwrap(),
        );
        let waiter = {
            let child = child.clone();
            std::thread::spawn(move || child.wait().unwrap())
        };
        child.kill(libc::SIGKILL).unwrap();
        assert_eq!(waiter.join().unwrap(), 128 + libc::SIGKILL);
        // Its pid may be another process's by now.
        child.kill(libc::SIGKILL).unwrap();
    }

    #[test]
    fn limits_hold_on_both_ends() {
        let (a, b) = UnixStream::pair().unwrap();
        let too_big = vec![0u8; MAX_PAYLOAD + 1];
        assert_eq!(
            send(&a, 1, &too_big, &[]).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let (_r, w) = pipe();
        let many: Vec<BorrowedFd<'_>> = (0..=MAX_FDS).map(|_| w.as_fd()).collect();
        assert_eq!(
            send(&a, 1, &[], &many).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        // A forged header past the limit is refused rather than allocated.
        let len = u32::try_from(MAX_PAYLOAD + 1).unwrap().to_be_bytes();
        (&a).write_all(&[1, len[0], len[1], len[2], len[3]]).unwrap();
        assert_eq!(recv(&b).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
