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
use std::net::Shutdown;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::{Identity, Judge, MAX_FDS, MAX_PAYLOAD};

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

/// The signals to forward by waiting for them: all of [`FORWARDED`], and on Linux SIGSTKFLT
/// and SIGPWR, which Docker names there too (moby/sys signal_linux.go); but SIGTTIN when
/// this process reads its terminal. Left unblocked, SIGTTIN stops a reader in the
/// background, as the terminal's job control stops any; blocked, the read would fail with
/// EIO instead (POSIX.1-2024, 11.1.4 Terminal Access Control).
pub fn forwarded(reads_terminal: bool) -> impl Iterator<Item = (libc::c_int, u32)> {
    FORWARDED
        .into_iter()
        .chain(linux_only())
        .filter(move |&(sig, _)| !(reads_terminal && sig == libc::SIGTTIN))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn linux_only() -> impl Iterator<Item = (libc::c_int, u32)> {
    [(libc::SIGSTKFLT, 16), (libc::SIGPWR, 30)].into_iter()
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn linux_only() -> impl Iterator<Item = (libc::c_int, u32)> {
    std::iter::empty()
}

/// Linux's real-time signals Docker names, RTMIN, 34, to RTMAX (moby/sys
/// signal_linux.go), less those below the C library's own SIGRTMIN, which it keeps; each
/// its own number. The client's alone ([`take_rest`]): a VM's process kicks its vCPUs
/// with SIGRTMIN (vmm hv/kvm).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn realtime() -> impl Iterator<Item = (libc::c_int, u32)> {
    (libc::SIGRTMIN().max(34)..=libc::SIGRTMAX())
        .filter_map(|sig| u32::try_from(sig).ok().map(|linux| (sig, linux)))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn realtime() -> impl Iterator<Item = (libc::c_int, u32)> {
    std::iter::empty()
}

/// The signals a fault raises as well as another process may send, each with its Linux
/// number. On Linux those another process sent are forwarded, as Go's `os/signal` delivers
/// them to the Docker CLI (runtime sigFromUser), and one a fault of the process's own raised
/// ends it as it would have. So they are caught ([`take_rest`]), not blocked: blocked, a
/// fault's signal is delivered with the default action, past the one the process had, such
/// as Rust's report of a stack overflow (kernel/signal.c force_sig_info_to_task). Not on
/// macOS, where nothing tells them apart: XNU gives a SIGSEGV, SIGBUS or SIGILL the codes
/// of a fault whoever sent it (bsd/dev/arm/unix_signal.c sendsig: SEGV_ACCERR, BUS_ADRALN,
/// ILL_ILLTRP), and never `SI_USER`, which is what Go asks of a signal sent, so the Docker
/// CLI there takes them as faults.
pub const FAULTS: [(libc::c_int, u32); 6] = [
    (libc::SIGILL, 4),
    (libc::SIGTRAP, 5),
    (libc::SIGBUS, 7),
    (libc::SIGFPE, 8),
    (libc::SIGSEGV, 11),
    (libc::SIGSYS, 31),
];

/// What wakes the thread that waits for forwarded signals when one of [`FAULTS`] was sent:
/// SIGURG, which nothing forwards, as Docker leaves it to the Go runtime
/// (cli/command/container/signals.go, isRuntimeSig).
pub const CARRIER: libc::c_int = libc::SIGURG;

/// [`FAULTS`] sent and not yet taken, a bit each.
static SENT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// The thread that waits for the carrier, once it has said so; 0 until then.
static WAITER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// The actions [`FAULTS`] had before they were caught, which a fault gets back.
#[cfg(any(target_os = "linux", target_os = "android"))]
static PREVIOUS: std::sync::OnceLock<[libc::sigaction; FAULTS.len()]> = std::sync::OnceLock::new();

/// [`take_rest`]'s handler: a signal another process sent (`si_code` not above 0:
/// include/uapi/asm-generic/siginfo.h, SI_FROMUSER) is marked and its waiter woken; a
/// fault's gets back the action it had before, and the fault, raised again as its
/// instruction runs again, is handled as it would have been (Rust's report of a stack
/// overflow, or the default). Async-signal-safe: atomics, kill(2), pthread_kill(3),
/// sigaction(2).
#[cfg(any(target_os = "linux", target_os = "android"))]
extern "C" fn sent_or_fault(sig: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    let Some(i) = FAULTS.iter().position(|&(s, _)| s == sig) else {
        return;
    };
    // SAFETY: the kernel passes an SA_SIGINFO handler the signal's siginfo.
    let code = unsafe { info.as_ref() }.map_or(1, |info| info.si_code);
    if code <= 0 {
        SENT.fetch_or(1 << i, std::sync::atomic::Ordering::SeqCst);
        let waiter = WAITER.load(std::sync::atomic::Ordering::SeqCst);
        // SAFETY: kill(2) and pthread_kill(3) are async-signal-safe; the waiter, once
        // set, is a thread that lives as long as the process.
        unsafe {
            if waiter == 0 || libc::pthread_kill(waiter as libc::pthread_t, CARRIER) != 0 {
                libc::kill(libc::getpid(), CARRIER);
            }
        }
        return;
    }
    if let Some(previous) = PREVIOUS.get().and_then(|p| p.get(i)) {
        // SAFETY: sigaction(2) is async-signal-safe; `previous` is what it gave before.
        unsafe { libc::sigaction(sig, previous, std::ptr::null_mut()) };
    }
}

/// The rest of what `docker run` forwards, for the client to forward too, after
/// [`take_forwarded`], on Linux: [`FAULTS`], caught, to forward those another process
/// sends ([`sent_faults`]); and in `set`, blocked in the calling thread, whose later
/// threads inherit the mask, the real-time signals Docker names and [`CARRIER`]. The thread
/// that waits on `set` says so first ([`wait_here`]). Not for a VM's process: its vCPUs'
/// kick is SIGRTMIN, and seccomp catches its SIGSYS. On macOS, nothing ([`FAULTS`]).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn take_rest(set: &mut libc::sigset_t) -> io::Result<()> {
    // SAFETY: sigaction(2) reads and writes of dispositions, and sigset operations, on
    // valid structures.
    unsafe {
        let mut previous: [libc::sigaction; FAULTS.len()] = std::mem::zeroed();
        for (&(sig, _), was) in FAULTS.iter().zip(previous.iter_mut()) {
            if libc::sigaction(sig, std::ptr::null(), was) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let _ = PREVIOUS.set(previous);
        let mut caught: libc::sigaction = std::mem::zeroed();
        caught.sa_sigaction = sent_or_fault
            as extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void)
            as libc::sighandler_t;
        // On the thread's alternate stack, as a stack overflow's SIGSEGV needs one.
        caught.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_RESTART;
        for (sig, _) in FAULTS {
            if libc::sigaction(sig, &caught, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let mut more: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut more);
        libc::sigaddset(&mut more, CARRIER);
        libc::sigaddset(set, CARRIER);
        for (sig, _) in realtime() {
            libc::sigaddset(&mut more, sig);
            libc::sigaddset(set, sig);
        }
        if libc::pthread_sigmask(libc::SIG_BLOCK, &more, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// [`take_rest`] on macOS: nothing ([`FAULTS`]).
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn take_rest(_set: &mut libc::sigset_t) -> io::Result<()> {
    Ok(())
}

/// Says the calling thread is the one that waits for [`CARRIER`], so that a fault signal
/// sent wakes it, whatever other threads block.
pub fn wait_here() {
    // SAFETY: pthread_self(3) has no preconditions.
    let me = unsafe { libc::pthread_self() } as usize;
    WAITER.store(me, std::sync::atomic::Ordering::SeqCst);
}

/// The Linux numbers of the [`FAULTS`] another process sent since last asked.
pub fn sent_faults() -> impl Iterator<Item = u32> {
    let sent = SENT.swap(0, std::sync::atomic::Ordering::SeqCst);
    FAULTS
        .into_iter()
        .enumerate()
        .filter(move |(i, _)| sent & (1 << i) != 0)
        .map(|(_, (_, linux))| linux)
}

/// The Linux number of `sig`, which a process forwards.
pub fn linux_signal(sig: libc::c_int) -> Option<u32> {
    numbered().find(|&(s, _)| s == sig).map(|(_, linux)| linux)
}

/// The signal this system numbers what Linux numbers `linux`, as [`linux_signal`] gives it:
/// none, for one this system has not.
pub fn local_signal(linux: u32) -> Option<libc::c_int> {
    numbered().find(|&(_, l)| l == linux).map(|(sig, _)| sig)
}

/// The signals a process forwards, each with its Linux number.
fn numbered() -> impl Iterator<Item = (libc::c_int, u32)> {
    let faults = cfg!(any(target_os = "linux", target_os = "android")).then_some(FAULTS);
    forwarded(false)
        .chain(realtime())
        .chain(faults.into_iter().flatten())
}

/// The handler that keeps a blocked signal pending for sigwait(3) on XNU; it never runs.
extern "C" fn kept_for_sigwait(_: libc::c_int) {}

/// Gives `sig` [`kept_for_sigwait`], so that XNU keeps it pending for sigwait(3) where its
/// default is to ignore it (SIGWINCH, SIGIO; M31). Whether this process was started
/// ignoring it.
fn keep_for_sigwait(sig: libc::c_int) -> io::Result<bool> {
    // SAFETY: sigaction(2) reads and writes of a disposition, on valid structures.
    unsafe {
        let mut kept: libc::sigaction = std::mem::zeroed();
        kept.sa_sigaction = kept_for_sigwait as extern "C" fn(libc::c_int) as libc::sighandler_t;
        kept.sa_flags = libc::SA_RESTART;
        let mut was: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(sig, &kept, &mut was) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(was.sa_sigaction == libc::SIG_IGN)
    }
}

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
                if keep_for_sigwait(sig)? {
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

/// Readies this process to take SIGWINCH with sigwait(3), as a client that forwards no
/// other signal follows its terminal's size (an exec's): blocked in the calling thread,
/// whose later threads inherit the mask, and kept pending on XNU as [`take_forwarded`]
/// keeps it, where its default would drop it as it is sent. Returns the set to wait on.
pub fn take_resizes() -> io::Result<libc::sigset_t> {
    // SAFETY: sigset operations on a local set, and pthread_sigmask on this thread.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGWINCH);
        let blocked = libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        if blocked != 0 {
            return Err(io::Error::from_raw_os_error(blocked));
        }
        set
    };
    keep_for_sigwait(libc::SIGWINCH)?;
    Ok(set)
}

impl Identity {
    /// A file's identity: its device and inode, its size and its last change.
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
    send_with(sock, kind, payload, fds, None)
}

/// [`send`], judged (runs-11 `TAKE_TIMEOUT`): written as room comes, by sendmsg(2) and send(2)
/// with `MSG_DONTWAIT`, which leave the socket's own mode and timeouts to its other users, and
/// waiting for room in poll(2) until the time `judge` names, where it is asked again. Never a
/// socket's send timeout, whose expiry mid-message loses how much of it went (POSIX send(2):
/// a partial count, or `EAGAIN` once nothing more fits). An error `judge` ends it with leaves
/// the message cut short where it was: its peer never had all of it.
pub fn send_judged(
    sock: &UnixStream,
    kind: u8,
    payload: &[u8],
    fds: &[BorrowedFd<'_>],
    judge: &mut Judge<'_>,
) -> io::Result<()> {
    send_with(sock, kind, payload, fds, Some(judge))
}

/// [`recv`], judged: the next message taken whole as it comes ([`Incoming`]), waiting for more
/// in poll(2) until the time `judge` names, where it is asked again. `None` for the peer's
/// end between messages.
pub fn recv_judged(sock: &UnixStream, judge: &mut Judge<'_>) -> io::Result<Option<Message>> {
    // Its judge asked only once a look finds the message not whole: a due already past, so
    // that a read first, then a poll with no wait, looks before it asks ([`Judge`]).
    let mut due = Some(Instant::now());
    let mut incoming = Incoming::default();
    loop {
        match incoming.take(sock)? {
            Took::Message(m) => return Ok(Some(m)),
            Took::Ended => return Ok(None),
            Took::Partial => {
                while !await_events(sock.as_fd(), libc::POLLIN, due)? {
                    due = judge()?;
                }
            }
        }
    }
}

/// Waits in poll(2) until `fd` has one of `events`, its end or an error, or `due` passes (none:
/// as long as that takes): whether it came. Each wait looks at `fd` at least once, a `due`
/// already past with a poll of 0 ms: readiness is the wait's word, and its judge, asked where
/// none came, judges its party by what it ran meanwhile. A wait that asked its judge without
/// looking refused an idle VM whose socket had room all along, as the judge's own first
/// reading of the VM outlasted W* (batch 26 at 1f71dea19: a daemon's stop SIGTERM refused
/// `Stalled` 50 to 90 µs in, SO_NWRITE 0 before and after, and its run SIGKILLed at its grace's
/// end; 9 in 72 stops, six at once, on an M4 Max). A wait a signal cut short is made again.
fn await_events(fd: BorrowedFd<'_>, events: libc::c_short, due: Option<Instant>) -> io::Result<bool> {
    loop {
        let ms = match due {
            None => -1,
            // Rounded up, so that a poll that times out ends at or past `due`: one of 0 ms only
            // where `due` has passed.
            Some(due) => {
                let left = due.saturating_duration_since(Instant::now());
                libc::c_int::try_from(left.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX)
            }
        };
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events,
            revents: 0,
        };
        // SAFETY: poll(2) on one valid pollfd.
        match unsafe { libc::poll(&mut pfd, 1, ms) } {
            n if n > 0 => return Ok(true),
            0 if due.is_some_and(|due| Instant::now() >= due) => return Ok(false),
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

/// The most a judged send writes at once to `sock`. On macOS a send(2) larger than its
/// socket's room waits for the rest, `MSG_DONTWAIT` or not: a 9,000-byte send with it to an
/// empty socketpair (8 KiB buffers) waited on, where one of 4,000 bytes went at once
/// (docs/research/measurements/runs-fixes/msg-dontwait.c, macOS 26 arm64, runs-11). There a
/// socket is writable, by the definition poll(2) and select(2) follow, once its room is at
/// least its send low-water mark (SO_SNDLOWAT, XNU's `sowriteable`; 2,048 bytes by default),
/// and a write of no more than that waits for nothing: so a judged send writes that much at
/// most, each time once poll(2) says there is room. Elsewhere `MSG_DONTWAIT` writes what fits
/// and returns (Linux, unix_stream_sendmsg): no bound.
fn send_most(sock: &UnixStream) -> io::Result<usize> {
    #[cfg(target_os = "macos")]
    {
        let mut lowat: libc::c_int = 0;
        let mut len = libc::socklen_t::try_from(size_of::<libc::c_int>()).map_err(io::Error::other)?;
        // SAFETY: getsockopt(2) of an int option into a local int, its length given.
        if unsafe {
            libc::getsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDLOWAT,
                (&raw mut lowat).cast(),
                &mut len,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(usize::try_from(lowat).unwrap_or(1).max(1))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = sock;
        Ok(usize::MAX)
    }
}

/// Waits for room on `sock` to write, asking `judge` each time `due` passes without it.
fn await_room(sock: &UnixStream, judge: &mut Judge<'_>, due: &mut Option<Instant>) -> io::Result<()> {
    while !await_events(sock.as_fd(), libc::POLLOUT, *due)? {
        *due = judge()?;
    }
    Ok(())
}

/// [`send`]'s and [`send_judged`]'s message: blocking where no `judge` is given, as `send`
/// always has; else as `send_judged` says.
fn send_with(
    sock: &UnixStream,
    kind: u8,
    payload: &[u8],
    fds: &[BorrowedFd<'_>],
    mut judge: Option<&mut Judge<'_>>,
) -> io::Result<()> {
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
    // A judged send's first write, on macOS no more than its socket's send low-water mark
    // ([`send_most`]), less its descriptors' control data; else all of it.
    let most = match judge {
        Some(_) => send_most(sock)?,
        None => usize::MAX,
    };
    let control_len = if fds.is_empty() { 0 } else { space(fds.len()) };
    let first = most.saturating_sub(control_len).max(1);
    let head_len = HEADER.min(first);
    let mut iov = [
        libc::iovec {
            iov_base: header.as_mut_ptr().cast(),
            iov_len: head_len,
        },
        libc::iovec {
            iov_base: payload.as_ptr() as *mut libc::c_void,
            iov_len: payload.len().min(first.saturating_sub(head_len)),
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
    // A judged send's judge asked only once a look finds no room: a due already past, so that
    // its write first, or (macOS) a poll with no wait, looks before it asks ([`Judge`]).
    let mut due = judge.is_some().then(Instant::now);
    let flags = if judge.is_some() { libc::MSG_DONTWAIT } else { 0 };
    #[cfg(target_os = "macos")]
    if let Some(judge) = judge.as_deref_mut() {
        await_room(sock, judge, &mut due)?;
    }
    let sent = loop {
        // SAFETY: a fully initialized msghdr over live buffers.
        let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, flags) };
        if let Ok(n) = usize::try_from(n) {
            break n;
        }
        let e = io::Error::last_os_error();
        match (e.kind(), judge.as_deref_mut()) {
            (io::ErrorKind::Interrupted, _) => {}
            (io::ErrorKind::WouldBlock, Some(judge)) => await_room(sock, judge, &mut due)?,
            _ => return Err(e),
        }
    };
    // The descriptors went with the first byte; the rest follows as plain bytes.
    if sent < total {
        match judge {
            None => write_rest(&mut &*sock, [&header, payload], sent)?,
            Some(judge) => send_rest(sock, [&header, payload], sent, most, judge, &mut due)?,
        }
    }
    Ok(())
}

/// [`write_rest`], judged: what follows the first `sent` bytes of `parts`, by send(2) with
/// `MSG_DONTWAIT`, at most `most` bytes a write, waiting for room as [`send_judged`] does.
fn send_rest(
    sock: &UnixStream,
    parts: [&[u8]; 2],
    sent: usize,
    most: usize,
    judge: &mut Judge<'_>,
    due: &mut Option<Instant>,
) -> io::Result<()> {
    let mut skip = sent;
    for part in parts {
        let mut rest = part.get(skip.min(part.len())..).unwrap_or_default();
        skip = skip.saturating_sub(part.len());
        while !rest.is_empty() {
            #[cfg(target_os = "macos")]
            await_room(sock, judge, due)?;
            // SAFETY: send(2) of `rest`'s first bytes, a live buffer, at most its length.
            let n = unsafe {
                libc::send(
                    sock.as_raw_fd(),
                    rest.as_ptr().cast(),
                    rest.len().min(most),
                    libc::MSG_DONTWAIT,
                )
            };
            match usize::try_from(n) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => rest = rest.get(n..).unwrap_or_default(),
                Err(_) => {
                    let e = io::Error::last_os_error();
                    match e.kind() {
                        io::ErrorKind::Interrupted => {}
                        io::ErrorKind::WouldBlock => await_room(sock, judge, due)?,
                        _ => return Err(e),
                    }
                }
            }
        }
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

/// Reads what `fd` has into `buf`, as read(2) does; but a socket on macOS is waited for in
/// poll(2), never in the read. There a read waiting on a Unix stream socket can sleep
/// through its peer's shutdown(SHUT_WR), and then on a stream that has ended, for good
/// where the peer waits for this side's end: XNU's soreceive lets go of the socket to take
/// its peer's lock first (uipc_rcvd), a half-close landing then finds no reader to wake,
/// and the reader looks only for data before it sleeps. Poll sees the end. Linux's reader
/// looks for the end under the lock its waker takes, and missed none; a pipe or a
/// terminal is no socket (docs/research/platform-measurements.md M236). Those are read as
/// they are.
///
/// On macOS a socket is read with `MSG_DONTWAIT`, and waited for only where nothing has
/// come, as a read waits: not at all where `fd` is set not to block, and no longer than
/// its receive timeout, `WouldBlock` either way. A FIFO or pipe that waits is waited for in
/// [`await_fifo`], whose select(2) sees a writer's end that a read can sleep through (PM
/// M253). A read a signal cut short is made again.
pub fn read(fd: impl AsFd, buf: &mut [u8]) -> io::Result<usize> {
    let fd = fd.as_fd();
    let mut socket = cfg!(target_os = "macos");
    let mut fifo = false;
    loop {
        if fifo {
            await_fifo(fd)?;
        }
        let n = if socket {
            // SAFETY: recv(2) into `buf`, at most its length.
            unsafe {
                libc::recv(
                    fd.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            }
        } else {
            // SAFETY: read(2) into `buf`, at most its length.
            unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) }
        };
        if let Ok(n) = usize::try_from(n) {
            return Ok(n);
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::ENOTSOCK) if socket => {
                socket = false;
                fifo = waiting_fifo(fd);
            }
            _ if socket && e.kind() == io::ErrorKind::WouldBlock => await_readable(fd, None)?,
            _ => return Err(e),
        }
    }
}

/// Whether `fd` is a FIFO or pipe whose reads wait: one that is set not to is never
/// waited for.
fn waiting_fifo(fd: BorrowedFd<'_>) -> bool {
    // SAFETY: fstat(2) into a zeroed stat buffer, of a descriptor the caller holds open.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: as above; and fcntl(2) reading its flags.
    let (stat, flags) = unsafe {
        (
            libc::fstat(fd.as_raw_fd(), &mut st),
            libc::fcntl(fd.as_raw_fd(), libc::F_GETFL),
        )
    };
    stat == 0 && st.st_mode & libc::S_IFMT == libc::S_IFIFO && flags >= 0 && flags & libc::O_NONBLOCK == 0
}

/// What reads through [`read`]: a socket, a pipe or a terminal read with
/// `BufReader::new(Polled(&conn))`, `io::copy(&mut Polled(&conn), …)` and the like.
#[derive(Debug)]
pub struct Polled<S>(pub S);

impl<S: AsFd> io::Read for Polled<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read(&self.0, buf)
    }
}

/// Ends `sock` both ways, one way at a time: its peer reads its end, and what reads it
/// here, its own. XNU shuts both at once or neither, and neither once the peer has ended
/// its side (ENOTCONN, soshutdownlock_final), so a peer that half-closed first would never
/// read this side's end while another copy of it stays open (PM M236).
pub fn end(sock: &UnixStream) {
    let _ = sock.shutdown(Shutdown::Write);
    let _ = sock.shutdown(Shutdown::Read);
}

/// Whether `sock` has hung up, asked at once: it takes no more writes, its peer gone (its
/// close; on Linux its reads shut too) or this side's own writes ended (on Linux, once its
/// reads have ended as well), poll(2)'s POLLHUP or POLLERR for a socket asked for POLLOUT. A
/// peer that ended only its writes, a half-close, leaves it POLLOUT alone: so what a read's
/// end was, measured on macOS 26 and Linux 7.3 (PM M559). XNU shows no peer's
/// shutdown(SHUT_RDWR), whose socket takes writes on (M236).
pub fn hung_up(sock: &UnixStream) -> io::Result<bool> {
    let mut fds = [libc::pollfd {
        fd: sock.as_raw_fd(),
        events: libc::POLLOUT,
        revents: 0,
    }];
    loop {
        // SAFETY: one pollfd of a descriptor `sock` holds open; a timeout of 0 waits for none.
        if unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) } >= 0 {
            break;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    let revents = fds.first().map_or(0, |fd| fd.revents);
    if revents & libc::POLLNVAL != 0 {
        return Err(io::Error::from_raw_os_error(libc::EBADF));
    }
    Ok(revents & (libc::POLLHUP | libc::POLLERR) != 0)
}

/// Waits in poll(2) until `fd` has something to read, or has ended: until `deadline`
/// where there is one (`TimedOut`); else as a read would, not at all where `fd` is set
/// not to block, and no longer than a socket's receive timeout (`WouldBlock`).
fn await_readable(fd: BorrowedFd<'_>, deadline: Option<Instant>) -> io::Result<()> {
    let (deadline, expired) = match deadline {
        Some(deadline) => (Some(deadline), io::ErrorKind::TimedOut),
        None => {
            // SAFETY: fcntl(2) reading the flags of a descriptor the caller holds open.
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
            if flags >= 0 && flags & libc::O_NONBLOCK != 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let timeout = receive_timeout(fd).and_then(|t| Instant::now().checked_add(t));
            (timeout, io::ErrorKind::WouldBlock)
        }
    };
    loop {
        let ms = match deadline {
            None => -1,
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(expired.into());
                }
                // Rounded up: a wait of 0 ms would return at once, and spin.
                libc::c_int::try_from(left.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX)
            }
        };
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
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

/// Waits until the FIFO or pipe `fd` has something to read, or has ended. On macOS a read(2)
/// of a FIFO sleeping as its writer ends, just after the data it took, can sleep through
/// that end: 6 in 224,000 such ends, raced 8 at once (PM M253), as a read of a Unix stream
/// socket can (M236); select(2) saw all of 192,000. So its reader waits here, then
/// reads what is there. A descriptor past `FD_SETSIZE` is waited on as one below it is
/// (`_DARWIN_UNLIMITED_SELECT`, select(2)). Elsewhere a read sees its writer's end, and
/// this waits for nothing.
pub fn await_fifo(fd: BorrowedFd<'_>) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: the libSystem select(2) that takes descriptor sets of any length, its
        // first argument a bound on the descriptors they hold.
        unsafe extern "C" {
            #[link_name = "select$DARWIN_EXTSN"]
            fn select_any(
                nfds: libc::c_int,
                read: *mut u32,
                write: *mut u32,
                error: *mut u32,
                timeout: *mut libc::timeval,
            ) -> libc::c_int;
        }
        let raw = fd.as_raw_fd();
        let at = usize::try_from(raw).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let bound = raw
            .checked_add(1)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        // An fd_set's bits, 32 to a word, as FD_SET sets them.
        let mut set = vec![0u32; at / 32 + 1];
        loop {
            if let Some(word) = set.get_mut(at / 32) {
                *word = 1 << (at % 32);
            }
            // SAFETY: a set of `bound` descriptors' bits, as long as that, and no timeout:
            // select(2) waits until `fd` is readable, or a signal cuts it short.
            let n = unsafe {
                select_any(
                    bound,
                    set.as_mut_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if n > 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if n < 0 && e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = fd;
        Ok(())
    }
}

/// Waits until the FIFO or pipe `fd`, a write end, has room for some of what is to be written,
/// or its reader has gone, which the write that follows says. On macOS in select(2): there
/// poll(2) for POLLOUT slept through a full FIFO's reader's end, 3 s of 3, where select saw it
/// in 0.13 s (PM M549), as a read can sleep through its writer's end (PM M253). Elsewhere in
/// poll(2), which says POLLERR as a pipe's last reader goes (Linux fs/pipe.c, `pipe_poll`).
pub fn await_fifo_room(fd: BorrowedFd<'_>) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: as `await_fifo`'s.
        unsafe extern "C" {
            #[link_name = "select$DARWIN_EXTSN"]
            fn select_any(
                nfds: libc::c_int,
                read: *mut u32,
                write: *mut u32,
                error: *mut u32,
                timeout: *mut libc::timeval,
            ) -> libc::c_int;
        }
        let raw = fd.as_raw_fd();
        let at = usize::try_from(raw).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let bound = raw
            .checked_add(1)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        // An fd_set's bits, 32 to a word, as FD_SET sets them.
        let mut set = vec![0u32; at / 32 + 1];
        loop {
            if let Some(word) = set.get_mut(at / 32) {
                *word = 1 << (at % 32);
            }
            // SAFETY: a set of `bound` descriptors' bits, as long as that, and no timeout:
            // select(2) waits until `fd` is writable, or a signal cuts it short.
            let n = unsafe {
                select_any(
                    bound,
                    std::ptr::null_mut(),
                    set.as_mut_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if n > 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if n < 0 && e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        loop {
            // SAFETY: poll(2) of one pollfd, of a descriptor `fd` borrows, with no timeout.
            match unsafe { libc::poll(&mut pfd, 1, -1) } {
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
}

/// Waits until one of `fds`, FIFOs or pipes, has something to read or has ended, and says
/// which, in their order: on macOS in select(2), as [`await_fifo`] waits, every descriptor
/// however high; elsewhere in poll(2), which sees a FIFO's end there (PM M253).
pub fn await_any(fds: &[BorrowedFd<'_>]) -> io::Result<Vec<bool>> {
    await_any_within(fds, None)
}

/// The longest one select(2) waits on macOS: XNU refuses a longer timeout with EINVAL
/// (bsd/kern/kern_time.c, `itimerfix`: `tv_sec > 100000000`), checked before it looks at
/// any descriptor (bsd/kern/sys_generic.c, `select_nocancel`). A longer wait is several
/// selects: a daemon's patience for a request doubles with each cut in a row (D222), past
/// this after 27, and every slot's wait then failed at once (PM M542).
#[cfg(target_os = "macos")]
const SELECT_MOST: Duration = Duration::from_secs(100_000_000);

/// [`await_any`], waiting no longer than `timeout` where one is given: then none is ready.
pub fn await_any_within(fds: &[BorrowedFd<'_>], timeout: Option<Duration>) -> io::Result<Vec<bool>> {
    // None past what an Instant holds: for ever.
    let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
    let left = || deadline.map(|d| d.saturating_duration_since(Instant::now()));
    #[cfg(target_os = "macos")]
    {
        // SAFETY: as `await_fifo`'s.
        unsafe extern "C" {
            #[link_name = "select$DARWIN_EXTSN"]
            fn select_any(
                nfds: libc::c_int,
                read: *mut u32,
                write: *mut u32,
                error: *mut u32,
                timeout: *mut libc::timeval,
            ) -> libc::c_int;
        }
        let at: Vec<usize> = fds
            .iter()
            .map(|fd| {
                usize::try_from(fd.as_raw_fd()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
            })
            .collect::<io::Result<_>>()?;
        let most = at
            .iter()
            .copied()
            .max()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let bound =
            libc::c_int::try_from(most + 1).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut set = vec![0u32; most / 32 + 1];
        loop {
            set.iter_mut().for_each(|w| *w = 0);
            for fd in &at {
                if let Some(word) = set.get_mut(fd / 32) {
                    *word |= 1 << (fd % 32);
                }
            }
            let wait = left();
            let cut = wait.is_some_and(|l| l > SELECT_MOST);
            let mut tv = wait.map(|l| {
                let l = l.min(SELECT_MOST);
                libc::timeval {
                    tv_sec: libc::time_t::try_from(l.as_secs()).unwrap_or(libc::time_t::MAX),
                    tv_usec: libc::suseconds_t::try_from(l.subsec_micros()).unwrap_or(0),
                }
            });
            let tv_at = tv.as_mut().map_or(std::ptr::null_mut(), std::ptr::from_mut);
            // SAFETY: a set of `bound` descriptors' bits, as long as that, and a timeout of
            // our own or none.
            let n = unsafe {
                select_any(
                    bound,
                    set.as_mut_ptr(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    tv_at,
                )
            };
            if n > 0 {
                return Ok(at
                    .iter()
                    .map(|fd| set.get(fd / 32).is_some_and(|w| w & (1 << (fd % 32)) != 0))
                    .collect());
            }
            // A wait cut to select's most goes on to its own deadline.
            if n == 0 && !cut {
                return Ok(vec![false; at.len()]);
            }
            if n == 0 {
                continue;
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let mut polled: Vec<libc::pollfd> = fds
            .iter()
            .map(|fd| libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let n =
            libc::nfds_t::try_from(polled.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        loop {
            // Whole milliseconds, rounded up, so that a wait never ends before its time.
            let ms = left().map_or(-1, |l| {
                libc::c_int::try_from(l.as_nanos().div_ceil(1_000_000)).unwrap_or(libc::c_int::MAX)
            });
            // SAFETY: poll(2) on `n` valid pollfds.
            match unsafe { libc::poll(polled.as_mut_ptr(), n, ms) } {
                0 => return Ok(vec![false; polled.len()]),
                ready if ready > 0 => return Ok(polled.iter().map(|p| p.revents != 0).collect()),
                _ => {}
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

/// A socket's receive timeout (SO_RCVTIMEO), the longest a read of it waits: none for
/// anything else, or for a socket without one.
fn receive_timeout(fd: BorrowedFd<'_>) -> Option<Duration> {
    let mut tv = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut len = libc::socklen_t::try_from(size_of::<libc::timeval>()).ok()?;
    // SAFETY: getsockopt(2) into a timeval, of its length.
    let got = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&raw mut tv).cast(),
            &mut len,
        )
    };
    if got != 0 {
        return None;
    }
    let micros = u32::try_from(tv.tv_usec).ok()?.checked_mul(1000)?;
    let timeout = Duration::new(u64::try_from(tv.tv_sec).ok()?, micros);
    (!timeout.is_zero()).then_some(timeout)
}

/// One `recvmsg` into `buf`, once something arrives, by `deadline` if there is one; on
/// macOS waited for in poll(2), as [`read`] waits. Returns the bytes read, and adds the
/// descriptors that came with them to `fds`, owned and close-on-exec.
fn recv_part(
    sock: &UnixStream,
    buf: &mut [u8],
    fds: &mut Vec<OwnedFd>,
    deadline: Option<Instant>,
) -> io::Result<usize> {
    // Elsewhere a read may wait in the kernel, where no deadline bounds it (M236).
    if deadline.is_none() && !cfg!(target_os = "macos") {
        return recv_msg(sock, buf, fds, 0);
    }
    loop {
        match recv_msg(sock, buf, fds, libc::MSG_DONTWAIT) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => await_readable(sock.as_fd(), deadline)?,
            got => return got,
        }
    }
}

/// Messages from a peer, taken as each comes in whole: what has come of one is kept until
/// the rest does, so a reader never waits on a peer that stops partway through one, and
/// reads nothing past it.
#[derive(Debug, Default)]
pub struct Incoming {
    header: [u8; HEADER],
    /// Bytes of the header that have come, then of the payload.
    have: usize,
    /// Once the header has come, the payload, as long as it says.
    payload: Option<Vec<u8>>,
    fds: Vec<OwnedFd>,
}

/// What [`Incoming::take`] found.
#[derive(Debug)]
pub enum Took {
    Message(Message),
    /// Some of the next message, or none of it, has come.
    Partial,
    /// The peer's end, with nothing of a message before it.
    Ended,
}

impl Incoming {
    /// The next message whole, from what `sock` has now: it waits for nothing.
    pub fn take(&mut self, sock: &UnixStream) -> io::Result<Took> {
        loop {
            let (buf, whole) = match &mut self.payload {
                None => (self.header.get_mut(self.have..).unwrap_or_default(), HEADER),
                Some(payload) => {
                    let len = payload.len();
                    (payload.get_mut(self.have..).unwrap_or_default(), len)
                }
            };
            if self.have < whole {
                match recv_msg(sock, buf, &mut self.fds, libc::MSG_DONTWAIT) {
                    Ok(0) if self.have == 0 && self.payload.is_none() && self.fds.is_empty() => {
                        return Ok(Took::Ended);
                    }
                    Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                    Ok(n) => self.have += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(Took::Partial),
                    Err(e) => return Err(e),
                }
                continue;
            }
            if self.payload.is_none() {
                let [_, l0, l1, l2, l3] = self.header;
                let len = u32::from_be_bytes([l0, l1, l2, l3]) as usize;
                if len > MAX_PAYLOAD {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "message payload too large",
                    ));
                }
                self.payload = Some(vec![0u8; len]);
                self.have = 0;
                continue;
            }
            if self.fds.len() > MAX_FDS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "more descriptors than a message may carry",
                ));
            }
            self.have = 0;
            return Ok(Took::Message(Message {
                kind: self.header[0],
                payload: self.payload.take().unwrap_or_default(),
                fds: std::mem::take(&mut self.fds),
            }));
        }
    }
}

/// One recvmsg(2) of at most `buf.len()` bytes, with `flags`, its descriptors added to
/// `fds`.
fn recv_msg(
    sock: &UnixStream,
    buf: &mut [u8],
    fds: &mut Vec<OwnedFd>,
    flags: libc::c_int,
) -> io::Result<usize> {
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
    let flags = flags | libc::MSG_CMSG_CLOEXEC;
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

/// Whether process `pid` lives: kill(2) with signal 0 asks, one of another user's among
/// those that do (EPERM), and one that has exited does not, whether or not its parent has
/// reaped it (waits-08): a zombie answers kill(2) and keeps its start, so a client that
/// waits on a daemon while it lives would wait for ever on one its supervisor leaves
/// unreaped. Windows' twin asks its handle whether it has ended.
pub(crate) fn alive(pid: u32) -> bool {
    // Zero or less would name a process group, or every process.
    let Some(id) = libc::pid_t::try_from(pid).ok().filter(|&pid| pid > 0) else {
        return false;
    };
    // SAFETY: kill(2) with signal 0 only asks whether the process exists.
    let exists =
        unsafe { libc::kill(id, 0) } == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    exists && !exited(pid)
}

/// Whether process `pid` has exited, unreaped: on Linux once its whole thread group has
/// ([`ended_in`] of its `/proc/<pid>/stat`). One whose state cannot be read (gone meanwhile,
/// or hidden) is not known to have.
#[cfg(target_os = "linux")]
fn exited(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| ended_in(&stat))
}

/// Whether a `/proc/<pid>/stat` line says its process has exited: its state, field 3, after
/// the last ')', `Z` (zombie) or `X` (dead), and no thread of it left but that one, its thread
/// count, field 20, at most 1 (proc(5); fs/proc/array.c `do_task_stat`). A thread-group leader
/// that exits before its other threads stays a zombie while they run, hold its files and do
/// its work, until the last of them has gone (kernel/exit.c: "we don't reap group leaders
/// with subthreads"; fs/file.c `put_files_struct`), and the count holds it until then
/// (kernel/exit.c `__exit_signal`): it has not exited (ROOT's ruling of 2026-11-17, D185).
/// A line whose count cannot be read says no exit.
#[cfg(target_os = "linux")]
fn ended_in(stat: &str) -> bool {
    let mut fields = stat
        .rfind(')')
        .and_then(|i| stat.get(i.saturating_add(1)..))
        .unwrap_or_default()
        .split_ascii_whitespace();
    let ended = fields.next().is_some_and(|state| matches!(state, "Z" | "X"));
    // Field 20, the 17th after the state.
    ended
        && fields
            .nth(16)
            .and_then(|n| n.parse::<u64>().ok())
            .is_some_and(|n| n <= 1)
}

/// Whether process `pid` has exited, unreaped: on macOS `proc_pid_rusage`'s
/// `ri_proc_exit_abstime`, set as its exit begins (XNU's `proc_prepareexit`, which a thread
/// that ends alone never calls: a process whose main thread has ended lives while its other
/// threads run), as crates/api's judge reads a process ending. One whose rusage cannot be
/// read (another user's) is not known to have.
#[cfg(not(target_os = "linux"))]
fn exited(pid: u32) -> bool {
    let Ok(pid) = libc::c_int::try_from(pid) else {
        return false;
    };
    // SAFETY: a rusage_info_v0, plain data, for proc_pid_rusage to fill.
    let mut info: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
    // SAFETY: proc_pid_rusage(3) of `pid` into a rusage_info_v0 of ours, which RUSAGE_INFO_V0
    // names.
    let r = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, (&raw mut info).cast()) };
    r == 0 && info.ri_proc_exit_abstime != 0
}

/// When process `pid` started ([`crate::started`]): on Linux its `/proc/<pid>/stat`
/// starttime, field 22, which follows the last ')' (its command, field 2, may hold any byte
/// but NUL, a ')' too).
#[cfg(target_os = "linux")]
pub(crate) fn started(pid: u32) -> io::Result<u64> {
    let at = format!("/proc/{pid}/stat");
    let stat = std::fs::read_to_string(&at)?;
    stat.rfind(')')
        .and_then(|i| stat.get(i.saturating_add(1)..))
        .and_then(|fields| fields.split_ascii_whitespace().nth(19))
        .and_then(|f| f.parse().ok())
        .ok_or_else(|| io::Error::other(format!("{at}: no starttime in {stat:?}")))
}

/// When process `pid` started ([`crate::started`]): on macOS `proc_pid_rusage`'s
/// `ri_proc_start_abstime`.
#[cfg(not(target_os = "linux"))]
pub(crate) fn started(pid: u32) -> io::Result<u64> {
    let pid = libc::c_int::try_from(pid).map_err(io::Error::other)?;
    // SAFETY: a rusage_info_v0, plain data, for proc_pid_rusage to fill.
    let mut info: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
    // SAFETY: proc_pid_rusage(3) of `pid` into a rusage_info_v0 of ours, which RUSAGE_INFO_V0
    // names.
    let r = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, (&raw mut info).cast()) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.ri_proc_start_abstime)
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

/// This process's peak commit charge so far, in KiB (windows.rs `peak_commit_kib`): none
/// on Unix, which charges a process its memory as it touches it, what [`peak_rss_kib`]
/// counts, not as it commits it; 0.
pub fn peak_commit_kib() -> u64 {
    0
}

/// This process's own memory now, in KiB ([`footprint`]); 0 where unknown.
pub fn footprint_kib() -> u64 {
    footprint().map_or(0, |bytes| bytes / 1024)
}

/// This process's own memory now, in bytes, as memory per VM is counted (PM M30's
/// `phys_footprint`): macOS's physical footprint, its private and copied pages, without the
/// file pages it shares with every process mapping them, a template's among them; Linux's
/// anonymous resident pages (`RssAnon`, proc(5)), which a written page of a private file
/// mapping becomes. As the daemon reads a VM process's (`platform::process_usage`). None
/// where it cannot be read.
pub fn footprint() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                let line = status.lines().find_map(|l| l.strip_prefix("RssAnon:"))?;
                line.trim()
                    .strip_suffix("kB")?
                    .trim()
                    .parse::<u64>()
                    .ok()?
                    .checked_mul(1024)
            })
    }
    #[cfg(target_vendor = "apple")]
    {
        // SAFETY: proc_pid_rusage(2) of this process into a zeroed rusage_info_v2, the
        // flavor it is asked for, a valid out-parameter.
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        let got = unsafe {
            libc::proc_pid_rusage(
                libc::getpid(),
                libc::RUSAGE_INFO_V2,
                (&raw mut info).cast::<libc::rusage_info_t>(),
            )
        };
        (got == 0).then_some(info.ri_phys_footprint)
    }
    #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
    None
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
/// reaped, so a signal never reaches another process that reuses the pid. One that this
/// process's spawner made (`by`, which of its spawners: [`start_spawner`]) is that
/// spawner's child, which reaps it when `wait` asks, and not before; it is watched and
/// signalled by its pid all the same.
#[derive(Debug)]
pub struct Child {
    pid: libc::pid_t,
    reaped: Mutex<bool>,
    by: Option<u64>,
}

impl Child {
    /// Child `pid`, made by this process or by its spawner `by`.
    pub(crate) fn new(pid: libc::pid_t, by: Option<u64>) -> Child {
        Child {
            pid,
            reaped: Mutex::new(false),
            by,
        }
    }

    pub fn id(&self) -> u32 {
        self.pid.unsigned_abs()
    }

    /// Waits for the child to end, without reaping it (WNOWAIT): its pid stays the child's,
    /// for `kill` to look at, until [`wait`](Self::wait) takes its status.
    pub fn ended(&self) -> io::Result<()> {
        self.ended_status().map(drop)
    }

    /// The status of a child that has ended, as [`wait`](Self::wait) gives it, without
    /// reaping it: until it is reaped, its pid stays its own, and no new process's.
    pub fn ended_status(&self) -> io::Result<i32> {
        let status = match self.by {
            Some(by) => crate::spawner::status(by, self.pid, true)?,
            None => status_of(self.pid, true)?,
        };
        status.ok_or_else(|| io::Error::other("a child waited for was said to run"))
    }

    /// Waits for the child to end: its exit status, or 128 plus the signal that ended it.
    /// Its end is waited for first, holding nothing; then it is reaped under the lock that
    /// [`kill`](Self::kill) takes, which waits on nothing: a child of this process's own is
    /// reaped without waiting (WNOHANG), and one its end was wrongly said of is waited for
    /// again, the lock let go of. So a signal is never held behind a reap that waits
    /// (PM M583).
    pub fn wait(&self) -> io::Result<i32> {
        loop {
            self.ended()?;
            let mut reaped = self.reaped.lock().unwrap_or_else(PoisonError::into_inner);
            let status = match self.by {
                Some(by) => Some(crate::spawner::reap(by, self.pid)?),
                None => reap_now(self.pid)?,
            };
            if let Some(status) = status {
                *reaped = true;
                return Ok(status);
            }
        }
    }

    /// Whether the child has ended, without waiting: its status as [`Child::wait`] gives
    /// it once it has, `None` while it runs. A child of this process's own is reaped
    /// without waiting too: on macOS waitid(2) once said a running child had ended, and a
    /// build's start waited in waitpid(2) for 23 minutes as its builder ran on, its
    /// connection never accepted (PM M442).
    pub fn try_wait(&self) -> Option<i32> {
        let ended = match self.by {
            Some(by) => crate::spawner::status(by, self.pid, false),
            None => status_of(self.pid, false),
        };
        ended.ok().flatten()?;
        if self.by.is_some() {
            return self.wait().ok();
        }
        let mut reaped = self.reaped.lock().unwrap_or_else(PoisonError::into_inner);
        let status = reap_now(self.pid).ok().flatten()?;
        *reaped = true;
        Some(status)
    }

    /// Sends `signal` to the child, unless it has been reaped.
    pub fn kill(&self, signal: libc::c_int) -> io::Result<()> {
        let reaped = self.reaped.lock().unwrap_or_else(PoisonError::into_inner);
        if *reaped {
            return Ok(());
        }
        // A spawner that has gone left what it made to init, which reaps it: its pid may
        // be another process's.
        if let Some(by) = self.by
            && !crate::spawner::serving(by)
        {
            return Err(io::Error::other("the spawner that made it has gone"));
        }
        // SAFETY: kill(2) of our own child, or our spawner's, not reaped while we hold the
        // lock.
        if unsafe { libc::kill(self.pid, signal) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Sends `signal` to the process group a detached child leads ([`spawn`]'s `detach`:
    /// its group's ID is its pid), and so to every process of that group, unless the child
    /// has been reaped. A group that has no process left is sent nothing.
    pub fn kill_group(&self, signal: libc::c_int) -> io::Result<()> {
        let reaped = self.reaped.lock().unwrap_or_else(PoisonError::into_inner);
        if *reaped {
            return Ok(());
        }
        if let Some(by) = self.by
            && !crate::spawner::serving(by)
        {
            return Err(io::Error::other("the spawner that made it has gone"));
        }
        // SAFETY: kill(2) of the group our unreaped child leads: while it is not reaped, its
        // pid, the group's ID, is no other process's or group's.
        if unsafe { libc::kill(-self.pid, signal) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Whether the child has ended, neither waiting for it nor reaping it.
    pub fn has_ended(&self) -> bool {
        let ended = match self.by {
            Some(by) => crate::spawner::status(by, self.pid, false),
            None => status_of(self.pid, false),
        };
        matches!(ended, Ok(Some(_)))
    }

    /// Ends the process group a detached child leads, as [`kill_group`](Self::kill_group)
    /// does, and returns once every process of it has exited, as Windows' twin returns
    /// (D231): followed by the kernel's word of each one's exit, its pidfd on Linux (readable
    /// from `exit_notify`, after `exit_task_work` has run each of its files' last release),
    /// its kqueue's `NOTE_EXIT` on macOS (posted by `proc_exit` after `fdt_invalidate`; a
    /// pipe it held was seen ending up to microseconds after it, PM M583). Never a pipe's
    /// end, which any process holding a write end of it, one a spawn elsewhere let inherit
    /// it, holds back. Each round lists the processes in it that have not exited (on macOS,
    /// one exiting is listed until it is a zombie: [`member`]), watches each one's exit,
    /// signals the group and waits on each watch, until a round lists none: a process made as
    /// a round signalled is signalled by the next, and one exiting before a round's look, on
    /// macOS past where a watch can be set, is looked at by the next. Never timed (D215):
    /// every wait is on a process SIGKILL has begun to end. Called before the child is reaped, its pid, the
    /// group's ID, no other process's or group's while it holds the lock that reaping takes;
    /// a reaped child's group is gone or another's, and is left be.
    pub fn end_group(&self) -> io::Result<()> {
        let reaped = self.reaped.lock().unwrap_or_else(PoisonError::into_inner);
        if *reaped {
            return Ok(());
        }
        if let Some(by) = self.by
            && !crate::spawner::serving(by)
        {
            return Err(io::Error::other("the spawner that made it has gone"));
        }
        loop {
            let members = group_members(self.pid)?;
            if members.is_empty() {
                return Ok(());
            }
            // Each watched before the round signals it: XNU lets no watch be set on a process
            // whose exit has begun (`proc_prepareexit` hides it from `proc_find`, which
            // `filt_procattach` asks), so one signalled first could be waited on by nothing.
            // Its end watched, then seen still the group's: a pid freed and taken by another
            // process meanwhile is not waited on. One already exiting has no watch, and the
            // next round looks at it again.
            let mut ends = Vec::with_capacity(members.len());
            for (pid, _) in members {
                if let Some(end) = exit_watch(pid)?
                    && member(pid, self.pid).is_some()
                {
                    ends.push(end);
                }
            }
            // SAFETY: kill(2) of the group our unreaped child leads, as `kill_group`'s.
            if unsafe { libc::kill(-self.pid, libc::SIGKILL) } != 0 {
                let e = io::Error::last_os_error();
                // A group with no process left to signal: Linux says ESRCH, XNU EPERM, as it
                // says of one it may not signal. XNU's killpg passes over its zombies and the
                // processes whose exit has begun (`killpg1`'s filter; `pgrp_iterate`'s
                // `proc_find`, refused from `proc_prepareexit` on; xnu-12377.1.9 kern_sig.c,
                // kern_proc.c), which [`member`] lists until they are zombies: those are
                // ending, and the next round looks at them again. Only a process still
                // running that it could not signal is refused.
                if group_members(self.pid)?
                    .iter()
                    .any(|&(_, standing)| standing == Standing::Running)
                {
                    return Err(e);
                }
            }
            for end in &ends {
                await_any(&[end.as_fd()])?;
            }
            // Nothing to wait on: each listed process was already exiting, past where a watch
            // can be set, and no OS word comes of its becoming a zombie but the look again.
            if ends.is_empty() {
                std::thread::yield_now();
            }
        }
    }
}

/// How a process of a group that has not exited stands ([`member`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Standing {
    /// Its exit not begun: a signal reaches it.
    Running,
    /// Its exit begun, not yet a zombie (Linux `PF_EXITING`, XNU `P_LEXIT`): XNU's killpg
    /// passes over it once `proc_prepareexit` has marked it, and it ends of itself.
    Exiting,
}

/// A descriptor readable once process `pid` has ended: a kqueue of its `EVFILT_PROC`
/// `NOTE_EXIT` on macOS (kqueue(2); a kqueue's descriptor is selectable), its pidfd on
/// Linux (pidfd_open(2), readable once it has exited, Linux 5.3). `None` where no process
/// has that pid. Neither is inherited by a child.
pub(crate) fn exit_watch(pid: libc::pid_t) -> io::Result<Option<OwnedFd>> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: kqueue(2) takes no arguments.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let queue = unsafe { OwnedFd::from_raw_fd(raw) };
        let change = libc::kevent {
            ident: pid.unsigned_abs() as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: kevent(2) reading one change, writing no event.
        if unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } < 0
        {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(e);
        }
        Ok(Some(queue))
    }
    #[cfg(not(target_os = "macos"))]
    {
        // SAFETY: pidfd_open(2) of a process, without flags: its descriptor is close-on-exec.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(e);
        }
        let raw = RawFd::try_from(raw).map_err(|_| io::Error::other("a descriptor out of range"))?;
        // SAFETY: a descriptor just made, owned by nothing else.
        Ok(Some(unsafe { OwnedFd::from_raw_fd(raw) }))
    }
}

/// The processes of group `pgid` that have not exited, and how each stands: on Linux each of
/// `/proc`'s whose `stat` says so ([`member`]); one that cannot be read (gone meanwhile) is
/// not one.
#[cfg(target_os = "linux")]
fn group_members(pgid: libc::pid_t) -> io::Result<Vec<(libc::pid_t, Standing)>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let Some(pid) = entry
            .ok()
            .and_then(|e| e.file_name().to_str().and_then(|n| n.parse::<libc::pid_t>().ok()))
        else {
            continue;
        };
        if let Some(standing) = member(pid, pgid) {
            found.push((pid, standing));
        }
    }
    Ok(found)
}

/// The processes of group `pgid` that have not exited, and how each stands: on macOS those
/// proc_listpids(3) lists by group (`PROC_PGRP_ONLY`, sys/proc_info.h), asked again with
/// twice the room while its answer fills all it was given, each then seen a member
/// ([`member`]).
#[cfg(not(target_os = "linux"))]
fn group_members(pgid: libc::pid_t) -> io::Result<Vec<(libc::pid_t, Standing)>> {
    /// sys/proc_info.h's `PROC_PGRP_ONLY`.
    const PROC_PGRP_ONLY: u32 = 2;
    let group = u32::try_from(pgid).map_err(io::Error::other)?;
    let mut room = 64usize;
    loop {
        let mut pids: Vec<libc::pid_t> = vec![0; room];
        let bytes = room
            .checked_mul(size_of::<libc::pid_t>())
            .and_then(|b| libc::c_int::try_from(b).ok())
            .ok_or_else(|| io::Error::other("a process group past what a listing holds"))?;
        // SAFETY: proc_listpids(3) into `pids`, `bytes` long, of the pids of group `group`.
        let n = unsafe { libc::proc_listpids(PROC_PGRP_ONLY, group, pids.as_mut_ptr().cast(), bytes) };
        let filled = usize::try_from(n).map_err(|_| io::Error::last_os_error())? / size_of::<libc::pid_t>();
        if filled < room {
            pids.truncate(filled);
            return Ok(pids
                .into_iter()
                .filter(|&pid| pid > 0)
                .filter_map(|pid| member(pid, pgid).map(|standing| (pid, standing)))
                .collect());
        }
        room = room
            .checked_mul(2)
            .ok_or_else(|| io::Error::other("a process group past what a listing holds"))?;
    }
}

/// How process `pid` stands where it is in group `pgid` and has not exited, `None` where
/// not: on Linux its `/proc/<pid>/stat`, after the last ')', its state (field 3) neither `Z`
/// nor `X`, its group (field 5) `pgid`, and its flags (field 9) `PF_EXITING` once its exit
/// has begun (proc(5); include/linux/sched.h).
#[cfg(target_os = "linux")]
fn member(pid: libc::pid_t, pgid: libc::pid_t) -> Option<Standing> {
    /// include/linux/sched.h's `PF_EXITING`: set by `exit_signals` as `do_exit` begins.
    const PF_EXITING: u32 = 0x4;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let mut fields = stat
        .rfind(')')
        .and_then(|i| stat.get(i.saturating_add(1)..))
        .unwrap_or_default()
        .split_ascii_whitespace();
    let state = fields.next();
    let group = fields.nth(1).and_then(|g| g.parse::<libc::pid_t>().ok());
    let flags = fields.nth(3).and_then(|f| f.parse::<u32>().ok());
    if matches!(state, None | Some("Z" | "X")) || group != Some(pgid) {
        return None;
    }
    Some(if flags.is_some_and(|f| f & PF_EXITING != 0) {
        Standing::Exiting
    } else {
        Standing::Running
    })
}

/// How process `pid` stands where it is in group `pgid` and has not exited, `None` where
/// not: on macOS its `proc_bsdinfo` (proc_pidinfo(3), `PROC_PIDTBSDINFO`) names that group
/// and no zombie, and `PROC_FLAG_INEXIT` once its exit has begun (`P_LEXIT`, set by
/// `exit1_internal` before `proc_prepareexit`; xnu-12377.1.9 kern_exit.c, proc_info.c). Not
/// its rusage's exit ([`exited`]): XNU sets that as `proc_exit` begins, before it closes the
/// process's descriptors (`fdt_invalidate`) and posts `NOTE_EXIT`, which is what is waited on.
/// Asked with a nonzero `arg`, which has XNU look among exiting processes too
/// (`proc_find_zombref`): from `proc_prepareexit` on, `proc_find` finds none, and asked with
/// none, an exiting process with its files still open read as gone, its group's end told
/// before it became a zombie that waitid(2) sees (xnu-12377.1.9 `proc_pidinfo_internal`'s
/// `findzomb`, `proc_prepareexit`'s `P_REF_DEAD`; 1 of 8 whole-lib runs on an M4 Max).
#[cfg(not(target_os = "linux"))]
fn member(pid: libc::pid_t, pgid: libc::pid_t) -> Option<Standing> {
    /// Any nonzero `arg` has `PROC_PIDTBSDINFO` look among exiting processes.
    const EXITING_TOO: u64 = 1;
    /// sys/proc_info.h's `PROC_FLAG_INEXIT`: "process is working its way in exit()".
    const PROC_FLAG_INEXIT: u32 = 4;
    // SAFETY: a proc_bsdinfo, plain data, for proc_pidinfo to fill.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: proc_pidinfo(3) of `pid` into a proc_bsdinfo of ours, `size` long, as
    // PROC_PIDTBSDINFO names.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            EXITING_TOO,
            (&raw mut info).cast(),
            size,
        )
    };
    if n != size || !u32::try_from(pgid).is_ok_and(|g| info.pbi_pgid == g) || info.pbi_status == libc::SZOMB {
        return None;
    }
    Some(if info.pbi_flags & PROC_FLAG_INEXIT != 0 {
        Standing::Exiting
    } else {
        Standing::Running
    })
}

/// Whether this process's child `pid` has ended, waiting for it to if `wait`: its status
/// as [`Child::wait`] gives it, without reaping it (WNOWAIT); `None` while it runs.
pub(crate) fn status_of(pid: libc::pid_t, wait: bool) -> io::Result<Option<i32>> {
    let flags = libc::WEXITED | libc::WNOWAIT | if wait { 0 } else { libc::WNOHANG };
    loop {
        // SAFETY: an all-zero siginfo_t is valid; waitid(2) fills it for our child.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: as above; WNOHANG returns at once, with si_pid 0 if nothing ended.
        if unsafe { libc::waitid(libc::P_PID, pid.unsigned_abs(), &mut info, flags) } == 0 {
            // SAFETY: si_pid and si_status are set by waitid when it reports a child.
            let (ended, status) = unsafe { (info.si_pid(), info.si_status()) };
            if ended == 0 {
                return Ok(None);
            }
            // XNU's waitid(2) says a stopped child under WEXITED alone (PM M442, M583): no end.
            // Its end is then waited for by the kernel's own word of it, and asked again.
            if !matches!(
                info.si_code,
                libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
            ) {
                if !wait {
                    return Ok(None);
                }
                if let Some(end) = exit_watch(pid)? {
                    await_any(&[end.as_fd()])?;
                }
                continue;
            }
            return Ok(Some(if info.si_code == libc::CLD_EXITED {
                status
            } else {
                128 + status
            }));
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Reaps this process's child `pid`, once it has ended: its exit status, or 128 plus the
/// signal that ended it.
pub(crate) fn reap(pid: libc::pid_t) -> io::Result<i32> {
    reaped(pid, 0).map(|status| status.unwrap_or_default())
}

/// Reaps this process's child `pid` if it has ended, without waiting: its status as
/// [`reap`] gives it, `None` while it runs.
fn reap_now(pid: libc::pid_t) -> io::Result<Option<i32>> {
    reaped(pid, libc::WNOHANG)
}

/// waitpid(2) for child `pid` with `flags`: its status, `None` where WNOHANG found it
/// running.
fn reaped(pid: libc::pid_t, flags: libc::c_int) -> io::Result<Option<i32>> {
    let mut status = 0;
    loop {
        // SAFETY: waitpid(2) for our own child into a local.
        let r = unsafe { libc::waitpid(pid, &mut status, flags) };
        if r == pid {
            break;
        }
        if r == 0 {
            return Ok(None);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    Ok(Some(if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
    }))
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
    // A process with a spawner has its children made there (crate::spawner, PM M158).
    if let Some(made) = crate::spawner::spawn(program, args, fds, detach, &env) {
        return made;
    }
    spawn_here(program, args, fds, detach, env)
}

/// Every descriptor this process holds above stdio made close-on-exec, once, before its
/// first child: what it was given open by whoever started it (a shell's, an IDE's, a CI
/// runner's: adacachy's kiln runs inherit perf_event descriptors) is no child's. macOS's
/// spawn closes all but those named (`POSIX_SPAWN_CLOEXEC_DEFAULT`); Linux's has no such
/// flag, so its children would hold them. Each child's own are placed by dup2, which
/// clears the flag on the copy (dup2(2)), so none it is given is lost. `close_range(2)`'s
/// CLOSE_RANGE_CLOEXEC (Linux 5.11) marks them at once; a kernel without it has each
/// marked as /proc/self/fd lists it.
#[cfg(target_os = "linux")]
fn cloexec_inherited() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: close_range(2) setting a flag on descriptors this process holds; it
        // closes none.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            )
        };
        if rc == 0 {
            return;
        }
        let Ok(listed) = std::fs::read_dir("/proc/self/fd") else {
            return;
        };
        for fd in listed
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<RawFd>().ok())
            .filter(|&fd| fd > 2)
        {
            // SAFETY: fcntl(2) adding close-on-exec to a descriptor of this process's; one
            // the listing itself held and has closed fails harmlessly.
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
        }
    });
}

/// [`spawn`]'s child, made by this process.
pub(crate) fn spawn_here(
    program: &Path,
    args: &[&OsStr],
    fds: &[(BorrowedFd<'_>, RawFd)],
    detach: bool,
    env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
) -> io::Result<Child> {
    #[cfg(target_os = "linux")]
    cloexec_inherited();
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
            for (fd, _) in fds {
                let src = fd.as_raw_fd();
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
            // Above every number in play, none is a target.
            for m in moved {
                check(libc::posix_spawn_file_actions_addclose(&mut actions, m))?;
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
            Ok(Child::new(pid, None))
        })();
        libc::posix_spawnattr_destroy(&mut attr);
        libc::posix_spawn_file_actions_destroy(&mut actions);
        result
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
pub(crate) mod tests {
    use std::fs::File;
    use std::io::Read;
    use std::os::fd::AsFd;

    /// A judge that asks again `every` from now, counting how often it was asked, and ends the
    /// wait at its `refuse`th ask, where it has one.
    fn judge_every(
        every: std::time::Duration,
        asked: &std::sync::atomic::AtomicUsize,
        refuse: Option<usize>,
    ) -> impl FnMut() -> std::io::Result<Option<std::time::Instant>> + '_ {
        move || {
            let n = asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if refuse.is_some_and(|at| n >= at) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "judged stalled",
                ));
            }
            Ok(std::time::Instant::now().checked_add(every))
        }
    }

    /// A `/proc/<pid>/stat` line says its process has exited once its state is a zombie's or a
    /// dead process's and no thread of it is left but that one (ROOT's ruling of 2026-11-17,
    /// D185): a zombie leader with threads still running, as a SIGKILLed daemon's may be, has
    /// not. Red before, where its state alone said it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_zombie_leader_with_threads_left_has_not_exited() {
        // `PID (COMM) STATE`, fields 4 to 19 each 0, its thread count (field 20), then fields
        // 21 and 22; its command holds a ')' of its own.
        let line =
            |state: &str, threads: &str| format!("42 (own ) x) {state}{} {threads} 0 7\n", " 0".repeat(16));
        for (state, threads, exited) in [
            ("Z", "3", false),
            ("Z", "2", false),
            ("Z", "1", true),
            ("X", "0", true),
            ("S", "1", false),
            ("R", "5", false),
            ("Z", "x", false),
        ] {
            assert_eq!(
                super::ended_in(&line(state, threads)),
                exited,
                "{state} with {threads} threads"
            );
        }
    }

    /// A judged send to a peer that reads late goes on where it was as room comes, asking its
    /// judge at each due, and its peer reads the message whole (runs-11 `TAKE_TIMEOUT`). Red
    /// before, with a send timeout in its judge's place: a write cut short mid-message by the
    /// timeout returned `WouldBlock` with no word of how much went, so the message was lost.
    #[test]
    fn a_judged_send_goes_on_where_it_was_as_its_peer_reads() {
        let (ours, theirs) = super::UnixStream::pair().unwrap();
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let payload: Vec<u8> = (0..super::MAX_PAYLOAD).map(|i| (i % 251) as u8).collect();
        std::thread::scope(|s| {
            let reader = s.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(200));
                super::recv(&theirs).unwrap().unwrap()
            });
            let mut judge = judge_every(std::time::Duration::from_millis(10), &asked, None);
            super::send_judged(&ours, 7, &payload, &[], &mut judge).unwrap();
            let m = reader.join().unwrap();
            assert_eq!((m.kind, m.payload == payload), (7, true));
        });
        assert!(
            asked.load(std::sync::atomic::Ordering::Relaxed) > 2,
            "its judge was not asked at its dues"
        );
        // Its judge's refusal ends it, the message cut short.
        let (ours, _theirs) = super::UnixStream::pair().unwrap();
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let mut judge = judge_every(std::time::Duration::from_millis(10), &asked, Some(3));
        let e = super::send_judged(&ours, 7, &payload, &[], &mut judge).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut, "{e}");
    }

    /// Do send, judged, to a socket with room, and receive, judged, from one with a message
    /// waiting, each with a judge whose first due has passed as it answers (its first reading
    /// of its party outlasting W*) and that refuses at its next ask. Expect: both go, the judge
    /// never asked, as each wait looks before it asks and finds its socket ready (D215, amended
    /// 2026-11-17; asked once before that). Red before the batch-26 cure: a wait whose due had
    /// passed asked its judge without looking at the socket, and the judge refused a party that
    /// had nothing to do (a daemon's stop SIGTERM refused `Stalled` with its VM's socket empty).
    #[test]
    fn a_judged_wait_looks_before_its_judge_is_asked_again() {
        let past = || {
            let asked = std::sync::atomic::AtomicUsize::new(0);
            move || {
                if asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "judged stalled",
                    ));
                }
                Ok(Some(std::time::Instant::now()))
            }
        };
        let (ours, theirs) = super::UnixStream::pair().unwrap();
        super::send_judged(&ours, 4, &15u32.to_be_bytes(), &[], &mut past()).unwrap();
        let m = super::recv_judged(&theirs, &mut past()).unwrap().unwrap();
        assert_eq!((m.kind, m.payload.as_slice()), (4, &15u32.to_be_bytes()[..]));
    }

    /// A judged receive waits for its message past its judge's dues while its judge says go on,
    /// takes it whole as it comes in parts, and ends with its judge's refusal or its peer's end.
    #[test]
    fn a_judged_receive_waits_while_its_judge_says_go_on() {
        use std::io::Write as _;
        let (ours, mut theirs) = super::UnixStream::pair().unwrap();
        let asked = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(60));
                // Its header first, then its payload.
                theirs.write_all(&[9, 0, 0, 0, 3]).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(60));
                theirs.write_all(b"abc").unwrap();
            });
            let mut judge = judge_every(std::time::Duration::from_millis(10), &asked, None);
            let m = super::recv_judged(&ours, &mut judge).unwrap().unwrap();
            assert_eq!((m.kind, m.payload.as_slice()), (9, &b"abc"[..]));
        });
        assert!(asked.load(std::sync::atomic::Ordering::Relaxed) > 2);
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let mut judge = judge_every(std::time::Duration::from_millis(10), &asked, Some(3));
        let e = super::recv_judged(&ours, &mut judge).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
        drop(theirs);
        let mut judge = judge_every(std::time::Duration::from_millis(10), &asked, None);
        assert!(super::recv_judged(&ours, &mut judge).unwrap().is_none());
    }

    use super::*;

    /// SIGWINCH, taken for sigwait(3) by a client that forwards no other signal (an exec's
    /// under `-it`), stays pending as its terminal sends it, and sigwait takes it: XNU drops
    /// it at its default, blocked or not (M31), and no exec's terminal followed its client's
    /// on the Mac (ROOT's HVF run at 41d35bde1: tty.rs `an_execs_terminal_follows_its_clients`
    /// and member_exec's resize case, each exec's command still in sigsuspend).
    #[test]
    fn a_resize_taken_alone_stays_pending_for_sigwait() {
        let set = take_resizes().unwrap();
        // Sent to this thread alone, which blocks it.
        // SAFETY: pthread_kill of this thread; sigpending and sigwait into locals.
        unsafe {
            assert_eq!(libc::pthread_kill(libc::pthread_self(), libc::SIGWINCH), 0);
            let mut pending: libc::sigset_t = std::mem::zeroed();
            assert_eq!(libc::sigpending(&mut pending), 0);
            assert_eq!(
                libc::sigismember(&pending, libc::SIGWINCH),
                1,
                "SIGWINCH was dropped as it was sent"
            );
            let mut sig = 0;
            assert_eq!(libc::sigwait(&set, &mut sig), 0);
            assert_eq!(sig, libc::SIGWINCH);
        }
    }

    /// A wait longer than one select(2) may take on macOS is waited, not refused: a ready
    /// pipe is told ready under a timeout of 2^34 s, past XNU's most, as under any other
    /// (EINVAL before, PM M542). Elsewhere poll(2) took it already.
    #[test]
    fn a_wait_past_selects_most_is_waited_not_refused() {
        let (read, mut write) = std::io::pipe().unwrap();
        std::io::Write::write_all(&mut write, b"x").unwrap();
        let ready = await_any_within(&[read.as_fd()], Some(Duration::from_secs(1 << 34))).unwrap();
        assert_eq!(ready, vec![true]);
        // And one that nothing comes on is told so within its own short wait.
        let (quiet, _held) = std::io::pipe().unwrap();
        let none = await_any_within(&[quiet.as_fd()], Some(Duration::from_millis(1))).unwrap();
        assert_eq!(none, vec![false]);
    }

    /// What [`take_rest`] does to a process, done in a child of its own, this binary run
    /// for [`signal_child`] alone, `case` naming what it does: this process keeps its
    /// signals as they were. Its status, and what it printed, once it ends or is ended
    /// past 30 s.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn signal_case(case: &str) -> (std::process::ExitStatus, String) {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unix::tests::signal_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("SHARDS_SIGNAL_CHILD", case)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                break child.wait().unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let mut out = String::new();
        child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
        (status, out)
    }

    /// [`signal_case`]'s child, which does nothing in a run of the tests. `sent`: a SIGSEGV
    /// another process sends wakes the thread waiting on the set, which takes it as Linux's
    /// 11. `fault`: one a fault of its own raises ends it, as it would have.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn signal_child() {
        let Ok(case) = std::env::var("SHARDS_SIGNAL_CHILD") else {
            return;
        };
        // SAFETY: an all-zero sigset_t, emptied: plain sigset operations on a local set.
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        unsafe { libc::sigemptyset(&mut set) };
        take_rest(&mut set).unwrap();
        match case.as_str() {
            "sent" => {
                let (ready, waiting) = std::sync::mpsc::channel();
                let waiter = std::thread::spawn(move || {
                    wait_here();
                    ready.send(()).unwrap();
                    let mut sig = 0;
                    // SAFETY: sigwait(3) on a valid set.
                    assert_eq!(unsafe { libc::sigwait(&set, &mut sig) }, 0);
                    (sig, sent_faults().collect::<Vec<u32>>())
                });
                waiting.recv().unwrap();
                // SAFETY: kill(2) of this process, as another would send it.
                unsafe { libc::kill(libc::getpid(), libc::SIGSEGV) };
                let (sig, sent) = waiter.join().unwrap();
                println!("woken {} sent {sent:?}", sig == CARRIER);
            }
            "fault" => {
                // SAFETY: an anonymous page made unreadable, then read: a fault of this
                // process's own.
                unsafe {
                    let page = libc::mmap(
                        std::ptr::null_mut(),
                        1 << 14,
                        libc::PROT_NONE,
                        libc::MAP_PRIVATE | libc::MAP_ANON,
                        -1,
                        0,
                    );
                    assert_ne!(page, libc::MAP_FAILED);
                    std::ptr::read_volatile(page.cast::<u8>());
                }
                println!("read an unreadable page");
            }
            _ => {}
        }
    }

    /// On Linux, a signal a fault raises, sent by another process, is forwarded as any
    /// other signal is, and one a fault of the process's own raises ends it, not looping on the
    /// fault as it would with the signal blocked (review 8.17).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn fault_signals_sent_are_forwarded_and_faults_end_the_process() {
        use std::os::unix::process::ExitStatusExt as _;
        let (status, out) = signal_case("sent");
        assert!(status.success(), "{status:?} {out}");
        assert!(out.contains("woken true sent [11]"), "{out}");
        let (status, out) = signal_case("fault");
        assert!(
            matches!(status.signal(), Some(libc::SIGSEGV | libc::SIGBUS)),
            "{status:?} {out}"
        );
        assert_eq!(linux_signal(libc::SIGSYS), Some(31));
        assert_eq!(linux_signal(libc::SIGURG), None, "the carrier is forwarded");
    }

    /// On macOS, where a fault's signal sent cannot be told from a fault's own, those
    /// signals keep their actions, and none is forwarded (review 8.17).
    #[cfg(target_vendor = "apple")]
    #[test]
    fn fault_signals_keep_their_actions_on_macos() {
        let action = |sig| {
            // SAFETY: sigaction(2) reading a disposition into a zeroed struct.
            let mut was: libc::sigaction = unsafe { std::mem::zeroed() };
            // SAFETY: as above.
            assert_eq!(unsafe { libc::sigaction(sig, std::ptr::null(), &mut was) }, 0);
            (was.sa_sigaction, was.sa_flags)
        };
        let before: Vec<_> = FAULTS.iter().map(|&(sig, _)| action(sig)).collect();
        // SAFETY: an all-zero sigset_t: a local set.
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        take_rest(&mut set).unwrap();
        let after: Vec<_> = FAULTS.iter().map(|&(sig, _)| action(sig)).collect();
        assert_eq!(before, after);
        assert_eq!(linux_signal(libc::SIGSEGV), None);
        assert_eq!(linux_signal(libc::SIGTERM), Some(15));
    }

    /// A signal forwarded by its Linux number is this system's own again at the other end:
    /// macOS's SIGUSR1 is 30, Linux's 10.
    #[test]
    fn a_signals_linux_number_names_it_on_every_system() {
        for (sig, linux) in forwarded(false) {
            assert_eq!(local_signal(linux), Some(sig), "{sig} as {linux}");
        }
        assert_eq!(local_signal(10), Some(libc::SIGUSR1));
        assert_eq!(local_signal(15), Some(libc::SIGTERM));
        assert_eq!(local_signal(0), None);
        assert_eq!(local_signal(9), None, "SIGKILL is no client's to send");
    }

    /// Linux's real-time signals Docker names are forwarded as their own numbers, and none
    /// below 34, which the C library keeps; SIGPWR and SIGSTKFLT too.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn linux_names_its_own_real_time_signals() {
        let top = libc::SIGRTMAX();
        assert_eq!(linux_signal(top), u32::try_from(top).ok());
        assert_eq!(linux_signal(33), None);
        assert_eq!(linux_signal(libc::SIGPWR), Some(30));
        assert_eq!(linux_signal(libc::SIGSTKFLT), Some(16));
    }

    /// A pipe whose ends are close-on-exec, as every descriptor shards holds is: on Linux
    /// `spawn` relies on that to give a child nothing it was not given.
    fn pipe() -> (File, File) {
        let (r, w) = io::pipe().unwrap();
        (File::from(OwnedFd::from(r)), File::from(OwnedFd::from(w)))
    }

    /// What has come of a message waits for the rest, however it is cut; nothing past a
    /// message is read with it; its descriptors come with it; an end is told from a
    /// message cut short by one.
    #[test]
    fn messages_are_taken_whole_as_they_come() {
        use std::io::Write as _;
        let frame = |kind: u8, payload: &[u8]| {
            let mut f = vec![kind];
            f.extend((payload.len() as u32).to_be_bytes());
            f.extend_from_slice(payload);
            f
        };
        let whole = |took: Took| match took {
            Took::Message(m) => Some(m),
            Took::Partial | Took::Ended => None,
        };
        let first = frame(3, b"payload");
        let second = frame(4, b"");
        for cut in 0..=first.len() {
            let (mut a, b) = UnixStream::pair().unwrap();
            let mut incoming = Incoming::default();
            a.write_all(&first[..cut]).unwrap();
            if cut < first.len() {
                assert!(
                    matches!(incoming.take(&b).unwrap(), Took::Partial),
                    "cut at {cut}"
                );
                a.write_all(&first[cut..]).unwrap();
            }
            a.write_all(&second).unwrap();
            let m = whole(incoming.take(&b).unwrap());
            assert!(m.is_some(), "cut at {cut}: not whole");
            let m = m.unwrap();
            assert_eq!(
                (m.kind, m.payload.as_slice()),
                (3, &b"payload"[..]),
                "cut at {cut}"
            );
            let m = whole(incoming.take(&b).unwrap()).unwrap();
            assert_eq!((m.kind, m.payload.len()), (4, 0));
            assert!(matches!(incoming.take(&b).unwrap(), Took::Partial));
            // Ended as a socket, not by closing a descriptor: a child spawned meanwhile by
            // another test may have inherited a copy (std makes a pair close-on-exec only
            // after making it, on macOS).
            a.shutdown(std::net::Shutdown::Write).unwrap();
            assert!(matches!(incoming.take(&b).unwrap(), Took::Ended));
        }
        // Descriptors come with their message.
        let (a, b) = UnixStream::pair().unwrap();
        let (mut r, w) = pipe();
        send(&a, 5, b"fd", &[w.as_fd()]).unwrap();
        drop(w);
        let m = whole(Incoming::default().take(&b).unwrap()).unwrap();
        assert_eq!((m.kind, m.fds.len()), (5, 1));
        File::from(m.fds.into_iter().next().unwrap())
            .write_all(b"through")
            .unwrap();
        let mut got = [0u8; 7];
        r.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"through");
        // An end partway through a message is no end of the stream.
        let (mut a, b) = UnixStream::pair().unwrap();
        let mut incoming = Incoming::default();
        a.write_all(&first[..6]).unwrap();
        a.shutdown(std::net::Shutdown::Write).unwrap();
        let cut = incoming.take(&b).unwrap_err();
        assert_eq!(cut.kind(), io::ErrorKind::UnexpectedEof);
        // A length past the limit is refused before anything is allocated for it.
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&[1, 0xff, 0xff, 0xff, 0xff]).unwrap();
        let big = Incoming::default().take(&b).unwrap_err();
        assert_eq!(big.kind(), io::ErrorKind::InvalidData);
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

    /// A socket whose peer has gone has hung up, and one whose peer ended its writes alone has
    /// not, its reads ending alike (D251's cut and half-close); this side's own end of writes
    /// before its peer's hangs it up too.
    #[test]
    fn hung_up_tells_a_peer_gone_from_a_half_close() {
        let mut byte = [0u8; 1];
        let (here, peer) = UnixStream::pair().unwrap();
        assert!(!hung_up(&here).unwrap(), "open both ways");
        peer.shutdown(Shutdown::Write).unwrap();
        assert_eq!(read(&here, &mut byte).unwrap(), 0, "its end");
        assert!(!hung_up(&here).unwrap(), "a half-close");
        (&here).write_all(b"on").unwrap();

        let (here, peer) = UnixStream::pair().unwrap();
        drop(peer);
        assert_eq!(read(&here, &mut byte).unwrap(), 0, "its end");
        assert!(hung_up(&here).unwrap(), "its peer gone");
        assert_eq!(
            (&here).write_all(b"x").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );

        let (here, peer) = UnixStream::pair().unwrap();
        here.shutdown(Shutdown::Write).unwrap();
        peer.shutdown(Shutdown::Write).unwrap();
        assert_eq!(read(&here, &mut byte).unwrap(), 0, "its end");
        assert!(hung_up(&here).unwrap(), "its own writes ended, then its peer's");
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
    pub(crate) fn children_get_the_descriptors_they_are_given() {
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
    pub(crate) fn children_get_swapped_descriptors_each_where_it_belongs() {
        let (mut ra, wa) = pipe();
        let (mut rb, wb) = pipe();
        let (a, b) = (wa.as_raw_fd(), wb.as_raw_fd());
        // a's writer goes to b's number, and b's to a's. Written through /dev/fd: dash,
        // Debian's sh, takes no descriptor above 9 in `>&N` ("Bad fd number"). The 64
        // numbers above both are scanned upward by the shell itself: BSD seq counts down
        // when its first number is the larger, which scanned the given two once the
        // suite's other tests had them above 64.
        let top = a.max(b);
        let script = format!(
            "echo A >/dev/fd/{b}; echo B >/dev/fd/{a}; fd={}; while [ $fd -le {} ]; do [ -e /dev/fd/$fd ] && echo leaked $fd >/dev/fd/{b}; fd=$((fd+1)); done; exit 0",
            top + 1,
            top + 64
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
    pub(crate) fn a_descriptor_given_its_own_number_reaches_the_child() {
        let (mut r, w) = pipe();
        let n = w.as_raw_fd();
        // SAFETY: fcntl(2) on a descriptor we own: close-on-exec, as ours all are.
        unsafe { libc::fcntl(n, libc::F_SETFD, libc::FD_CLOEXEC) };
        let script = format!("echo kept >/dev/fd/{n}");
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
    pub(crate) fn children_get_nothing_else() {
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
    pub(crate) fn a_child_spawned_in_an_environment_has_it_alone() {
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
    pub(crate) fn statuses_and_signals() {
        let child = spawn(
            Path::new("/bin/sh"),
            &["-c".as_ref(), "exit 7".as_ref()],
            &[],
            false,
        )
        .unwrap();
        assert_eq!(child.wait().unwrap(), 7);
        // Not through sh: dash forks its one command, which would outlive the shell
        // killed here, and hold this process's stdout for 30 s.
        let child = spawn(Path::new("/bin/sleep"), &["30".as_ref()], &[], false).unwrap();
        child.kill(libc::SIGTERM).unwrap();
        assert_eq!(child.wait().unwrap(), 128 + libc::SIGTERM);
    }

    /// Waits until the kernel says child `child` has stopped, leaving it to be waited for.
    fn stopped(child: &Child) {
        loop {
            // SAFETY: an all-zero siginfo_t, for waitid(2) to fill for our child.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: waitid(2) of our child's stop, which it leaves to be waited for again.
            let r =
                unsafe { libc::waitid(libc::P_PID, child.id(), &mut info, libc::WSTOPPED | libc::WNOWAIT) };
            if r == 0 && info.si_code == libc::CLD_STOPPED {
                return;
            }
            assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::Interrupted);
        }
    }

    /// Do wait for a child's end on another thread, stop the child (SIGSTOP) and see it
    /// stopped, then ask whether it has ended and kill its group. Expect: a stopped child
    /// has not ended, the kill is sent though a reaper waits, and the wait returns once the
    /// child has ended, its status SIGKILL's. Red on macOS where `ended`'s waitid(2), asked
    /// for exits alone, woke at the stop and took it for an end (PM M442, M583), and `wait`
    /// then reaped in a wait4(2) that waited, holding the lock `kill_group` takes: the kill
    /// was never sent, and the test hangs.
    #[test]
    fn a_stopped_child_waited_for_is_killed_at_once_and_reaped_at_its_end() {
        let child = spawn(Path::new("/bin/sleep"), &["600".as_ref()], &[], true).unwrap();
        std::thread::scope(|s| {
            let waiting = s.spawn(|| child.wait());
            child.kill(libc::SIGSTOP).unwrap();
            stopped(&child);
            let (ended, waited) = (child.has_ended(), child.try_wait());
            child.kill_group(libc::SIGKILL).unwrap();
            assert_eq!(waiting.join().unwrap().unwrap(), 128 + libc::SIGKILL);
            assert!(!ended, "a stopped child was said to have ended");
            assert_eq!(waited, None);
        });
    }

    /// Do start a child of a group of its own that starts another and tells its pid, stop
    /// the child, then end its group (`end_group`) before it is reaped. Expect: `end_group`
    /// returns once both have exited, and the child's status is SIGKILL's.
    #[test]
    fn a_groups_end_follows_every_process_of_it_to_its_end() {
        let (read, write) = io::pipe().unwrap();
        let child = spawn(
            Path::new("/bin/sh"),
            &[
                "-c".as_ref(),
                "sleep 600 3>&- & echo $! >&3; exec 3>&-; wait".as_ref(),
            ],
            &[(write.as_fd(), 3)],
            true,
        )
        .unwrap();
        drop(write);
        let mut said = String::new();
        (&read).read_to_string(&mut said).unwrap();
        let other: u32 = said.trim().parse().unwrap();
        child.kill(libc::SIGSTOP).unwrap();
        stopped(&child);
        assert!(alive(other));
        child.end_group().unwrap();
        assert!(
            !alive(other),
            "end_group returned while a process of its group ran"
        );
        assert!(child.has_ended());
        assert_eq!(child.wait().unwrap(), 128 + libc::SIGKILL);
    }

    /// Do start a child of a group of its own that starts `dd` with a 512 MiB buffer, which
    /// its reads of /dev/zero fill, and exits, unreaped; kill `dd`, and once it is seen
    /// exiting (its address space's teardown the span), end the group. Expect: `end_group`
    /// returns Ok once `dd` has exited. Red on macOS where a group of a zombie and processes
    /// exiting alone, which XNU's killpg passes over and answers EPERM, was refused as a
    /// process that could not be signalled (batch 26, api_image.rs's stop mid-boot:
    /// `Os { what: "the stopped run's processes", kind: PermissionDenied }`).
    #[test]
    fn a_groups_end_follows_a_process_already_exiting_to_its_end() {
        /// dd's `bs`.
        const BUFFER: u64 = 512 << 20;
        let mut exiting_seen = 0;
        for _ in 0..20 {
            let (read, write) = io::pipe().unwrap();
            let child = spawn(
                Path::new("/bin/sh"),
                &[
                    "-c".as_ref(),
                    "dd if=/dev/zero of=/dev/null bs=536870912 3>&- 2>/dev/null & echo $! >&3".as_ref(),
                ],
                &[(write.as_fd(), 3)],
                true,
            )
            .unwrap();
            drop(write);
            let mut said = String::new();
            (&read).read_to_string(&mut said).unwrap();
            let dd: libc::pid_t = said.trim().parse().unwrap();
            while !child.has_ended() {
                std::thread::yield_now();
            }
            // Its buffer filled, its exit's teardown long.
            while member(dd, child.pid).is_some() && resident(dd) < BUFFER {
                std::thread::yield_now();
            }
            // SAFETY: kill(2) of the dd this test started, which its group still holds.
            assert_eq!(unsafe { libc::kill(dd, libc::SIGKILL) }, 0);
            let seen = loop {
                match member(dd, child.pid) {
                    Some(Standing::Exiting) => break true,
                    Some(Standing::Running) => std::thread::yield_now(),
                    None => break false,
                }
            };
            child.end_group().unwrap();
            assert_eq!(
                member(dd, child.pid),
                None,
                "end_group returned while dd had not exited"
            );
            assert_eq!(child.wait().unwrap(), 0);
            if seen {
                exiting_seen += 1;
            }
        }
        assert!(
            exiting_seen > 0,
            "dd was never seen exiting: the case was not made"
        );
    }

    /// The bytes of process `pid` resident: its rusage's (macOS), its statm's (Linux).
    fn resident(pid: libc::pid_t) -> u64 {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: sysconf(3) of the page size.
            let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap();
            std::fs::read_to_string(format!("/proc/{pid}/statm"))
                .ok()
                .and_then(|m| m.split_ascii_whitespace().nth(1)?.parse::<u64>().ok())
                .map_or(0, |pages| pages * page)
        }
        #[cfg(not(target_os = "linux"))]
        {
            // SAFETY: a rusage_info_v4, plain data, for proc_pid_rusage(2) to fill.
            let mut info: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
            // SAFETY: proc_pid_rusage(2) of `pid` into a rusage_info_v4 of ours, as its
            // flavor names.
            let r = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, (&raw mut info).cast()) };
            if r == 0 { info.ri_resident_size } else { 0 }
        }
    }

    #[test]
    pub(crate) fn detached_children_lead_their_own_session() {
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
        // The kernel closes what a message carried past the receiver's room after `recv`
        // returns: on this Mac, with the crate's tests beside it, the end of the pipe came
        // p50 1.2 ms, p90 1.6 ms, p99 49.8 ms after (n=60, 2026-10-14), and past 1 s while a
        // whole workspace's tests ran (gate of 1e072493's successor). A leak is what never
        // closes: so the wait is long.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
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
        // Room for its 200 beside the other tests' descriptors: a test binary keeps the
        // soft limit it was started with, 256 from a macOS shell (launchd's), where the
        // daemon raises its own. macOS takes no more than OPEN_MAX (setrlimit(2)).
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit(2) and setrlimit(2) of this process, into and from a local.
        unsafe {
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim), 0);
            lim.rlim_cur = lim.rlim_cur.max(lim.rlim_max.min(10240));
            assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &lim), 0);
        }
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
    pub(crate) fn a_reaped_child_is_never_signalled() {
        let child =
            std::sync::Arc::new(spawn(Path::new("/bin/sleep"), &["30".as_ref()], &[], false).unwrap());
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

    /// A read, or a message's, entering the kernel just as its peer ends its side sees the
    /// end: on macOS one that waited in recv(2) missed it in 360 of 50,000 such races, and
    /// waited on (PM M236). The two meet at points spread around the read's start, the
    /// same each run.
    #[test]
    fn a_read_sees_its_peers_end_that_lands_as_it_starts() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        static GO: AtomicUsize = AtomicUsize::new(usize::MAX);
        let spin = |n: u64| {
            for i in 0..n {
                std::hint::black_box(i);
            }
        };
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % 400
        };
        for round in 0..20_000 {
            let (a, b) = UnixStream::pair().unwrap();
            let (reader_spins, peer_spins) = (next(), next());
            let (said, heard) = std::sync::mpsc::channel();
            // Not scoped: a reader that misses the end fails the test rather than hold it.
            std::thread::spawn(move || {
                while GO.load(Ordering::Acquire) != round {
                    std::hint::spin_loop();
                }
                spin(reader_spins);
                let got = if round % 2 == 0 {
                    read(&b, &mut [0u8; 64]).map(|n| n == 0)
                } else {
                    recv(&b).map(|m| m.is_none())
                };
                let _ = said.send(got.map_err(|e| e.kind()));
            });
            let peer = std::thread::spawn(move || {
                while GO.load(Ordering::Acquire) != round {
                    std::hint::spin_loop();
                }
                spin(peer_spins);
                a.shutdown(Shutdown::Write).unwrap();
                a
            });
            GO.store(round, Ordering::Release);
            let a = peer.join().unwrap();
            assert_eq!(
                heard.recv_timeout(Duration::from_secs(10)),
                Ok(Ok(true)),
                "round {round}: the reader missed its peer's end"
            );
            drop(a);
        }
    }

    /// [`read`] waits as a read would: until its socket's receive timeout, and not at all
    /// where it is set not to block, as messages do; and it reads a pipe once something
    /// comes, and a socket to its end through [`Polled`]. Nothing comes on these sockets,
    /// so a read that waited past its timeout would never end: `WouldBlock` is its proof,
    /// and its time is held only to the timeout from below. A ceiling would be a chosen
    /// multiple, and a host that pauses the process (a cgroup frozen by a pacer) passes it.
    #[test]
    fn a_read_waits_as_a_read_would() {
        use std::time::Duration;
        let (_a, b) = UnixStream::pair().unwrap();
        let timeout = Duration::from_millis(100);
        b.set_read_timeout(Some(timeout)).unwrap();
        for message in [false, true] {
            let start = Instant::now();
            let failed = if message {
                recv(&b).unwrap_err()
            } else {
                read(&b, &mut [0u8; 8]).unwrap_err()
            };
            assert_eq!(failed.kind(), io::ErrorKind::WouldBlock);
            let waited = start.elapsed();
            assert!(waited >= timeout, "{waited:?}");
        }
        // Set not to block, with no timeout left: one that waited would wait for ever.
        b.set_read_timeout(None).unwrap();
        b.set_nonblocking(true).unwrap();
        assert_eq!(
            read(&b, &mut [0u8; 8]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(recv(&b).unwrap_err().kind(), io::ErrorKind::WouldBlock);

        let (r, mut w) = pipe();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            w.write_all(b"late").unwrap();
            w
        });
        let mut got = [0u8; 8];
        assert_eq!(read(&r, &mut got).unwrap(), 4);
        assert_eq!(&got[..4], b"late");
        drop(writer.join().unwrap());

        // Ended as a socket: a child spawned meanwhile by another test may hold a copy.
        let (a, b) = UnixStream::pair().unwrap();
        (&a).write_all(b"whole").unwrap();
        a.shutdown(Shutdown::Write).unwrap();
        let mut all = String::new();
        Polled(&b).read_to_string(&mut all).unwrap();
        assert_eq!(all, "whole");
    }

    /// [`end`] reaches a peer that ended its side first, where SHUT_RDWR at once says
    /// ENOTCONN on macOS and shuts nothing (PM M236); and it ends a read waiting here.
    #[test]
    fn ending_a_stream_reaches_a_peer_that_ended_its_side_first() {
        use std::time::Duration;
        let (a, b) = UnixStream::pair().unwrap();
        // Before the shutdowns: XNU takes no option on a socket shut both ways (EINVAL).
        b.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        b.shutdown(Shutdown::Write).unwrap();
        assert_eq!(read(&a, &mut [0u8; 8]).unwrap(), 0);
        end(&a);
        assert_eq!(
            read(&b, &mut [0u8; 8]).map_err(|e| e.kind()),
            Ok(0),
            "its peer read no end"
        );

        let (a, _b) = UnixStream::pair().unwrap();
        std::thread::scope(|s| {
            let waiting = s.spawn(|| read(&a, &mut [0u8; 8]).map_err(|e| e.kind()));
            std::thread::sleep(Duration::from_millis(50));
            end(&a);
            assert_eq!(waiting.join().unwrap(), Ok(0));
        });
    }
}

/// Names this process after the file it runs, as ps(1), top(1) and pgrep(1) show it (its
/// `comm`, prctl(2) PR_SET_NAME): a helper `shards` starts through its own descriptor,
/// `/proc/PID/fd/N` (shards `helpers::Program::path`), is named `N` by its exec, which takes the
/// name from the path's last part (fs/exec.c `__set_task_comm` of `bprm->filename`'s
/// basename). Its file's own name is `/proc/self/exe`'s; Linux keeps the first 15 bytes of
/// it (TASK_COMM_LEN, 16 with its NUL).
#[cfg(target_os = "linux")]
pub fn name_after_executable() -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let exe = std::env::current_exe()?;
    let name = exe
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "an executable with no name"))?;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "an executable's name with NUL"))?;
    // SAFETY: prctl(2) PR_SET_NAME reads a NUL-terminated string, which it copies.
    if unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
#[allow(clippy::unwrap_used)]
mod name_tests {
    /// A thread that names itself after its executable is shown by that name, as much of it
    /// as Linux keeps (15 bytes): a helper started through `/proc/PID/fd/N` is shown as `N`
    /// until it does (the VM process at 8cdbf4a05, whose `comm` read `5`, so the tests' VM
    /// processes, found by name, were none). Do: name a thread of this test process, whose
    /// own name the harness set to the test's. Expect: its comm is this executable's name.
    #[test]
    fn a_process_named_after_its_executable_shows_its_files_name() {
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_str().unwrap();
        let want = name.get(..name.len().min(15)).unwrap().to_string();
        let shown = std::thread::spawn(|| {
            super::name_after_executable().unwrap();
            std::fs::read_to_string("/proc/thread-self/comm").unwrap()
        })
        .join()
        .unwrap();
        assert_eq!(shown.trim_end(), want);
    }
}
