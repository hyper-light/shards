//! Code mode (docs/design/architecture.md D136; AGENTFILE_ARCH.md §8, the answer to Q17): a
//! program its caller sends an instance, JavaScript that QuickJS-NG runs in a process of
//! its own, calling the instance's tools as the caller's MCP calls do, many in one round
//! trip, with the caller's grants and no more.
//!
//! - **The helper** is this binary again, which the instance starts as it begins
//!   (`shards-server sandbox`), before it confines itself, as the filter it then runs under
//!   forks no process. Exec'd fresh, it holds none of the instance's keys, nor any
//!   descriptor but its control socket, and confines itself at once to that socket and to
//!   forking, reaping and killing its workers.
//! - **A worker**, forked by the helper for each program, holds one socket, the program's,
//!   and runs under a filter that lets it read and write, manage its memory, read the clock
//!   and exit: nothing else. Each tool it calls, it asks the instance for over that socket,
//!   which calls it as it calls its MCP tools, under the caller's grants.
//!
//! The control socket carries a message each: the instance's `R` (an ID, and the program's
//! socket) and `S` (an ID: kill its worker), and the helper's `E` (an ID, and how its worker
//! ended, as `waitid` says). A program's socket carries frames: a kind, its length (a u32,
//! big-endian) and its bytes: the instance's `P`, the program, then `R`, each call's answer;
//! the worker's `C`, each call, then `D`, the value it returned as JSON, or `F`, why it
//! failed.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use rquickjs::{CatchResultExt, Context, FromJs, Function, Promise, Runtime};

/// A program's frames.
pub const PROGRAM: u8 = b'P';
pub const REPLY: u8 = b'R';
pub const CALL: u8 = b'C';
pub const DONE: u8 = b'D';
pub const FAILED: u8 = b'F';

/// The control socket's messages.
pub const RUN: u8 = b'R';
pub const STOP: u8 = b'S';
pub const ENDED: u8 = b'E';

/// ENDED's code for a worker that ended, as its pidfd said, whose status waitid did not give
/// (the kernel reaped it, SIGCHLD ignored): none of waitid's (`CLD_*`, 1 to 6, include/uapi/
/// asm-generic/siginfo.h) nor creation refusal's -1. Its status is waitid's errno, or 0.
pub const END_UNKNOWN: i32 = 0;

/// The most a frame holds: what its length, a big-endian u32, says at most. A worker's
/// frames, its calls and its value, are held as they come, as a request's body is
/// (`server::HEAD_MAX`): the worker is its instance's child, in its caller's domain, whose
/// `memory.max` bounds what either is made to hold (D60, D136). The instance's frames, the
/// program and what its calls return, are bounded by what the instance holds, and the
/// worker reads them whole.
pub const FRAME_MAX: usize = u32::MAX as usize;

/// Where the helper finds its control socket.
pub const CONTROL_FD: RawFd = 3;

/// What the program runs in: `shards`, its tools, each an async function that asks the
/// instance and throws what it refuses; the native call it is made of taken out of reach.
/// A broker tool's typed receipt is the cause of what it throws, and, not enumerable, so
/// that the value's JSON is as declared, the `broker` of what it returns.
const PRELUDE: &str = r#"const shards = (() => {
  const call = globalThis.__call;
  delete globalThis.__call;
  const tool = (name, args) => {
    const r = JSON.parse(call(name, JSON.stringify(args)));
    if (r.error !== undefined) {
      throw r.broker === undefined ? new Error(r.error) : new Error(r.error, { cause: r.broker });
    }
    if (r.broker !== undefined && typeof r.value === "object" && r.value !== null) {
      Object.defineProperty(r.value, "broker", { value: r.broker });
    }
    return r.value;
  };
  return Object.freeze({
    whoami: async () => tool("whoami", {}),
    mcp: async () => tool("mcp", {}),
    peers: async () => tool("peers", {}),
    send: async (to, text) => tool("send", { to, text }),
    receive: async () => tool("receive", {}),
    answer: async (to, id, text) => tool("answer", { to, id, text }),
    hand: async (to, path, id) => tool("hand", id === undefined ? { to, path } : { to, path, id }),
  });
})();
"#;

fn os_err(what: &str) -> String {
    format!("{what}: {}", io::Error::last_os_error())
}

/// Writes all of `bytes` to `fd`, which blocks.
fn write_all(fd: RawFd, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        // SAFETY: write(2) from a buffer of the length given.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        let n = usize::try_from(n).map_err(io::Error::other)?;
        bytes = bytes.get(n..).unwrap_or_default();
    }
    Ok(())
}

/// Fills `buf` from `fd`, which blocks; an end before it is full is an error.
fn read_exact(fd: RawFd, mut buf: &mut [u8]) -> io::Result<()> {
    while !buf.is_empty() {
        // SAFETY: read(2) into a buffer of the length given.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let n = usize::try_from(n).map_err(io::Error::other)?;
        buf = std::mem::take(&mut buf).get_mut(n..).unwrap_or_default();
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameFailure {
    Length,
    Capacity,
}

impl std::fmt::Display for FrameFailure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Length => "the frame length is not representable",
            Self::Capacity => "the frame storage is unavailable",
        })
    }
}

/// Wire storage is reserved fallibly before the helper may acquire its program socket.
pub fn prepare_frame(kind: u8, bytes: &[u8]) -> Result<Vec<u8>, FrameFailure> {
    let len = u32::try_from(bytes.len()).map_err(|_| FrameFailure::Length)?;
    let size = bytes
        .len()
        .checked_add(1 + size_of::<u32>())
        .ok_or(FrameFailure::Length)?;
    let mut out = Vec::new();
    out.try_reserve_exact(size).map_err(|_| FrameFailure::Capacity)?;
    out.push(kind);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(out)
}

/// Existing worker String boundary. An allocation refusal cannot allocate its diagnostic.
pub fn frame(kind: u8, bytes: &[u8]) -> Result<Vec<u8>, String> {
    prepare_frame(kind, bytes).map_err(|failure| {
        let message = match failure {
            FrameFailure::Length => "the frame length is not representable",
            FrameFailure::Capacity => "the frame storage is unavailable",
        };
        let mut text = String::new();
        if text.try_reserve_exact(message.len()).is_ok() {
            text.push_str(message);
        }
        text
    })
}

/// A frame read from a buffer: its kind, its bytes, and how much of the buffer it took.
pub type Parsed<'a> = (u8, &'a [u8], usize);

/// The first whole frame of `buf`, if one is; an error for one past [`FRAME_MAX`].
pub fn parse_frame(buf: &[u8]) -> Result<Option<Parsed<'_>>, String> {
    let Some((&kind, rest)) = buf.split_first() else {
        return Ok(None);
    };
    let Some((len, rest)) = rest.split_first_chunk::<4>() else {
        return Ok(None);
    };
    let len = usize::try_from(u32::from_be_bytes(*len)).map_err(|e| e.to_string())?;
    if len > FRAME_MAX {
        return Err(format!("a frame of {len} bytes, more than {FRAME_MAX}"));
    }
    Ok(rest.get(..len).map(|bytes| (kind, bytes, len + 5)))
}

/// Reads a frame from `fd`, which blocks: one of `max` bytes or fewer.
fn read_frame(fd: RawFd, max: usize) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    read_exact(fd, &mut head)?;
    let [kind, a, b, c, d] = head;
    let len = usize::try_from(u32::from_be_bytes([a, b, c, d])).map_err(io::Error::other)?;
    if len > max {
        return Err(io::Error::other(format!(
            "a frame of {len} bytes, more than {max}"
        )));
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(io::Error::other)?;
    bytes.resize(len, 0);
    read_exact(fd, &mut bytes)?;
    Ok((kind, bytes))
}

/// A filter that allows the calls `allowed` and kills the process for any other, or for a
/// call of another architecture's (on x86_64 an x32 call too): a cBPF program over
/// `seccomp_data` (include/uapi/linux/seccomp.h), as the kernel runs it.
pub fn allowlist(allowed: &[libc::c_long]) -> Result<Vec<libc::sock_filter>, String> {
    const LD_ABS_W: u16 = 0x20;
    const JEQ: u16 = 0x15;
    const RET: u16 = 0x06;
    const KILL_PROCESS: u32 = 0x8000_0000;
    const ALLOW: u32 = 0x7fff_0000;
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xC000_00B7;
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xC000_003E;
    let op = |code: u16, jt: u8, jf: u8, k: u32| libc::sock_filter { code, jt, jf, k };
    let mut head = vec![op(LD_ABS_W, 0, 0, 4), op(JEQ, 0, 0, ARCH), op(LD_ABS_W, 0, 0, 0)];
    #[cfg(target_arch = "x86_64")]
    head.push(op(0x35, 0, 0, 0x4000_0000));
    let checks = allowed.len();
    let kill = head.len() + checks;
    let far = |from: usize, to: usize| u8::try_from(to - from - 1).map_err(|_| "a filter too long to jump");
    if let Some(arch_check) = head.get_mut(1) {
        arch_check.jf = far(1, kill)?;
    }
    #[cfg(target_arch = "x86_64")]
    if let Some(x32) = head.get_mut(3) {
        x32.jt = far(3, kill)?;
    }
    let mut prog = head;
    for &nr in allowed {
        let at = prog.len();
        let nr = u32::try_from(nr).map_err(|_| "a negative call number")?;
        prog.push(op(JEQ, far(at, kill + 1)?, 0, nr));
    }
    prog.push(op(RET, 0, 0, KILL_PROCESS));
    prog.push(op(RET, 0, 0, ALLOW));
    Ok(prog)
}

/// Loads `prog` over this process, which `no_new_privs` lets it (init set it before the
/// instance's exec, and execs keep it).
fn confine(prog: &[libc::sock_filter]) -> Result<(), String> {
    let fprog = libc::sock_fprog {
        len: u16::try_from(prog.len()).map_err(|_| "a filter too long")?,
        filter: prog.as_ptr().cast_mut(),
    };
    // SAFETY: prctl(2) with constant arguments, then seccomp(2) with a sock_fprog over a
    // program that outlives the call.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(os_err("no_new_privs"));
        }
        if libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0,
            &raw const fprog,
        ) != 0
        {
            return Err(os_err("a sandbox's seccomp filter"));
        }
    }
    Ok(())
}

/// What a worker may call: read and write its socket, manage its memory, read the clock and
/// the kernel's randomness, and end; what musl's fork leaves the child to call (`gettid`,
/// signal masks); nothing else.
fn worker_calls() -> Vec<libc::c_long> {
    vec![
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_close,
        libc::SYS_mmap,
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_brk,
        libc::SYS_madvise,
        libc::SYS_futex,
        libc::SYS_clock_gettime,
        libc::SYS_getrandom,
        libc::SYS_gettid,
        libc::SYS_set_tid_address,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_exit,
        libc::SYS_exit_group,
    ]
}

/// What the helper may call besides a worker's: its control socket's messages and their
/// descriptors, polling, atomic worker/pidfd creation, killing and reaping, and parent
/// identity/death setup before the worker's stricter filter, and what a worker calls to
/// confine itself further before it reads its program (`prctl`, `seccomp`), which its
/// own filter then refuses. And `restart_syscall`, which the kernel has a process call to
/// resume a wait a stop broke into (poll's, Linux fs/select.c): a job-control stop, a
/// cgroup's freeze (init's pause and commit) or a debugger's attach. It resumes only that
/// call (kernel/signal.c `restart_block`), so it allows no other.
fn helper_calls() -> Vec<libc::c_long> {
    let mut calls = worker_calls();
    calls.extend([
        libc::SYS_recvmsg,
        libc::SYS_sendmsg,
        libc::SYS_ppoll,
        libc::SYS_restart_syscall,
        libc::SYS_clone,
        libc::SYS_getpid,
        libc::SYS_getppid,
        libc::SYS_pidfd_send_signal,
        libc::SYS_waitid,
        libc::SYS_fcntl,
        libc::SYS_prctl,
        libc::SYS_seccomp,
    ]);
    #[cfg(target_arch = "x86_64")]
    calls.push(libc::SYS_poll);
    calls
}

/// A control message: its bytes, and the descriptor it carries, if any.
pub fn recv_control(fd: RawFd) -> io::Result<Option<(Vec<u8>, Option<OwnedFd>)>> {
    let mut bytes = [0u8; 64];
    // Room for one descriptor (CMSG_SPACE of one int), aligned.
    let mut control = [0u64; 4];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    // SAFETY: an all-zero msghdr is valid, and its fields are set below.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    // SAFETY: recvmsg(2) into buffers that outlive the call.
    let n = unsafe { libc::recvmsg(fd, &raw mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Ok(None);
    }
    let mut got = None;
    // SAFETY: walking the control messages recvmsg filled, as CMSG_FIRSTHDR and
    // CMSG_NXTHDR walk them, reading one descriptor from an SCM_RIGHTS message.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&raw const msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let fd = std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<RawFd>());
                got = Some(OwnedFd::from_raw_fd(fd));
            }
            c = libc::CMSG_NXTHDR(&raw const msg, c);
        }
    }
    let len = usize::try_from(n).map_err(io::Error::other)?;
    Ok(Some((bytes.get(..len).unwrap_or_default().to_vec(), got)))
}

/// Sends a control message, with `with` passed beside it.
pub fn send_control(fd: RawFd, bytes: &[u8], with: Option<RawFd>) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let mut control = [0u64; 4];
    // SAFETY: an all-zero msghdr is valid, and its fields are set below.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    if let Some(passed) = with {
        msg.msg_control = control.as_mut_ptr().cast();
        // SAFETY: CMSG_SPACE and CMSG_LEN of one descriptor's size; the header CMSG_FIRSTHDR
        // finds lies in `control`, which is large enough for it.
        unsafe {
            msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as _;
            let c = libc::CMSG_FIRSTHDR(&raw const msg);
            if c.is_null() {
                return Err(io::Error::other("no room for a descriptor"));
            }
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<RawFd>(), passed);
        }
    }
    // SAFETY: sendmsg(2) from buffers that outlive the call.
    if unsafe { libc::sendmsg(fd, &raw const msg, libc::MSG_NOSIGNAL) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The helper: `shards-server sandbox`, its control socket at [`CONTROL_FD`].
pub fn helper() -> std::process::ExitCode {
    match serve_helper() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            use std::io::Write as _;
            let _ = writeln!(io::stderr(), "shards-server sandbox: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// The worker remains owned by its pidfd until waitid actually reaps it.
struct Worker {
    id: u64,
    pidfd: OwnedFd,
    phase: WorkerPhase,
}

#[derive(Clone, Copy)]
enum WorkerPhase {
    Running,
    Stopping,
    SignalUnknown(io::ErrorKind, Option<i32>),
}

#[derive(Debug)]
enum HelperCause {
    Setup(String),
    Io(&'static str, io::ErrorKind, Option<i32>),
    Control,
    Closing,
}

impl HelperCause {
    fn io(operation: &'static str, error: io::Error) -> Self {
        Self::Io(operation, error.kind(), error.raw_os_error())
    }
}

#[derive(Debug)]
struct CleanupUnknown {
    count: usize,
    first_id: u64,
    cause: HelperCause,
}

#[derive(Debug)]
struct HelperFailure {
    cause: HelperCause,
    cleanup: Option<CleanupUnknown>,
}

impl From<HelperCause> for HelperFailure {
    fn from(cause: HelperCause) -> Self {
        Self { cause, cleanup: None }
    }
}

impl std::fmt::Display for HelperCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Setup(why) => f.write_str(why),
            Self::Io(operation, kind, code) => write!(f, "{operation}: {kind:?}, OS {code:?}"),
            Self::Control => f.write_str("a control message it does not know"),
            Self::Closing => f.write_str("ending its owned workers"),
        }
    }
}

impl std::fmt::Display for HelperFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.cause)?;
        if let Some(unknown) = &self.cleanup {
            write!(
                f,
                "; {} workers not known reaped, first {}: {}",
                unknown.count, unknown.first_id, unknown.cause
            )?;
        }
        Ok(())
    }
}

impl Worker {
    fn stop(&mut self) {
        if !matches!(self.phase, WorkerPhase::Running) {
            return;
        }
        // SAFETY: this owned pidfd names only this worker, even after its PID is reused.
        if unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                libc::SIGKILL,
                0,
                0,
            )
        } == 0
        {
            self.phase = WorkerPhase::Stopping;
            return;
        }
        let error = io::Error::last_os_error();
        self.phase = if error.raw_os_error() == Some(libc::ESRCH) {
            WorkerPhase::Stopping
        } else {
            WorkerPhase::SignalUnknown(error.kind(), error.raw_os_error())
        };
    }

    fn reap(&self, options: i32) -> io::Result<Option<(i32, i32)>> {
        // SAFETY: zero is a valid initial siginfo_t for waitid to fill.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid writes through this live siginfo_t and names our owned child.
        if unsafe {
            libc::waitid(
                libc::P_PIDFD,
                self.pidfd.as_raw_fd() as libc::id_t,
                &raw mut info,
                libc::WEXITED | options,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: these child-status fields were filled by the successful waitid.
        let (pid, status) = unsafe { (info.si_pid(), info.si_status()) };
        if pid == 0 {
            return Ok(None);
        }
        Ok(Some((info.si_code, status)))
    }

    fn said(control: RawFd, id: u64, code: i32, status: i32) -> Result<(), HelperCause> {
        use io::Write as _;
        // Existing ENDED wire: kind, u64 ID, i32 wait code, i32 status.
        let mut bytes = [0; size_of::<u8>() + size_of::<u64>() + 2 * size_of::<i32>()];
        let mut out = bytes.as_mut_slice();
        for part in [
            [ENDED].as_slice(),
            id.to_be_bytes().as_slice(),
            code.to_be_bytes().as_slice(),
            status.to_be_bytes().as_slice(),
        ] {
            out.write_all(part)
                .map_err(|e| HelperCause::io("encoding a worker's end", e))?;
        }
        send_control(control, &bytes, None).map_err(|e| HelperCause::io("saying a worker ended", e))
    }
}

fn serve_helper() -> Result<(), HelperFailure> {
    // SAFETY: a constant NUL-terminated thread name of fewer than 16 bytes.
    unsafe { libc::prctl(libc::PR_SET_NAME, c"shards-sandbox".as_ptr()) };
    confine(&allowlist(&helper_calls()).map_err(HelperCause::Setup)?).map_err(HelperCause::Setup)?;
    let control = CONTROL_FD;
    let mut workers: Vec<Worker> = Vec::new();
    let mut set: Vec<libc::pollfd> = Vec::new();
    set.try_reserve(1)
        .map_err(|_| HelperCause::Io("its control poll slot", io::ErrorKind::OutOfMemory, None))?;
    let result = (|| -> Result<(), HelperCause> {
        loop {
            // Every birth reserves its next poll slot before clone. Rebuilding this
            // derived readiness view therefore allocates nothing after a birth.
            set.clear();
            for fd in std::iter::once(control).chain(workers.iter().map(|w| w.pidfd.as_raw_fd())) {
                set.push(libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            // SAFETY: poll of this process's initialized pollfd array.
            if unsafe { libc::poll(set.as_mut_ptr(), set.len() as libc::nfds_t, -1) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(HelperCause::io("polling", error));
            }
            // Reverse removal preserves the remaining indices in this readiness view.
            for (i, ready) in set.iter().skip(1).enumerate().rev() {
                if ready.revents == 0 {
                    continue;
                }
                let w = workers.get(i).ok_or(HelperCause::Control)?;
                // Its pidfd says it ended. Where waitid gives no status of it, it is said to
                // have ended unknown and is waited for no more; the others are served on.
                let (code, status) = match w.reap(0) {
                    Ok(Some(ended)) => ended,
                    Ok(None) => (END_UNKNOWN, 0),
                    Err(e) => (END_UNKNOWN, e.raw_os_error().unwrap_or(0)),
                };
                // get(i) above checked this index; no mutation intervened.
                let w = workers.swap_remove(i);
                Worker::said(control, w.id, code, status)?;
            }
            if set.first().is_some_and(|p| p.revents == 0) {
                continue;
            }
            let Some((msg, fd)) =
                recv_control(control).map_err(|e| HelperCause::io("its control socket", e))?
            else {
                return Ok(());
            };
            let id = msg
                .get(1..9)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_be_bytes)
                .unwrap_or(0);
            match (msg.first(), fd) {
                (Some(&RUN), Some(program)) => {
                    // At most one RUN is consumed in this iteration; set still holds
                    // at least the current population plus control. Admit both arrays
                    // before the kernel can create another worker.
                    if workers.try_reserve(1).and_then(|()| set.try_reserve(1)).is_err() {
                        Worker::said(control, id, -1, -1)?;
                        continue;
                    }
                    let mut pidfd: libc::c_int = -1;
                    // SAFETY: getpid identifies this freshly exec'd, single-threaded helper.
                    let parent = unsafe { libc::getpid() };
                    // clone(2), which carries this request whole, as glibc's fork calls it
                    // (sysdeps/unix/sysv/linux/arch-fork.h); not clone3, which Docker's
                    // default seccomp profile answers ENOSYS (crates/seccomp/data/default.json).
                    // SAFETY: fork-like clone from the fresh single-threaded helper.
                    // CLONE_PIDFD reserves/stores the parent handle, through the third
                    // argument (parent_tid), before publishing a child (Linux 6.18.48
                    // kernel/fork.c, the clone entry and copy_process). No stack, TLS or
                    // child TID: the last two arguments are 0 in either order an
                    // architecture takes them.
                    let pid = unsafe {
                        libc::syscall(
                            libc::SYS_clone,
                            (libc::CLONE_PIDFD | libc::SIGCHLD) as libc::c_long,
                            0 as libc::c_long,
                            &raw mut pidfd,
                            0 as libc::c_long,
                            0 as libc::c_long,
                        )
                    };
                    if pid < 0 {
                        Worker::said(control, id, -1, -1)?;
                        continue;
                    }
                    if pid == 0 {
                        // SAFETY: raw setup precedes all worker allocation/instructions.
                        // Arming then checking the pre-birth parent closes the already-
                        // dead-parent race; the worker's filter forbids resetting it.
                        unsafe {
                            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0
                                || libc::getppid() != parent
                            {
                                libc::_exit(1);
                            }
                            libc::close(control);
                            libc::prctl(libc::PR_SET_NAME, c"shards-program".as_ptr());
                        }
                        for w in &workers {
                            // SAFETY: these inherited copies are not used by this child.
                            unsafe { libc::close(w.pidfd.as_raw_fd()) };
                        }
                        let code = worker(program);
                        // SAFETY: bypass inherited parent destructors on this child branch.
                        unsafe { libc::_exit(code) };
                    }
                    drop(program);
                    // SAFETY: successful CLONE_PIDFD gave this parent a valid owned fd.
                    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
                    workers.push(Worker {
                        id,
                        pidfd,
                        phase: WorkerPhase::Running,
                    });
                }
                (Some(&STOP), _) => {
                    if let Some(w) = workers.iter_mut().find(|w| w.id == id) {
                        w.stop();
                        if let WorkerPhase::SignalUnknown(kind, code) = w.phase {
                            return Err(HelperCause::Io("stopping a worker", kind, code));
                        }
                    }
                }
                _ => return Err(HelperCause::Control),
            }
        }
    })();
    // Close every worker's execution before waiting for any one worker. A signal
    // failure never leads to an indefinite wait for that still-running worker.
    for w in &mut workers {
        w.stop();
    }
    let mut cleanup: Option<CleanupUnknown> = None;
    for w in &workers {
        let options = if matches!(w.phase, WorkerPhase::SignalUnknown(..)) {
            libc::WNOHANG
        } else {
            0
        };
        let failure = match w.reap(options) {
            Ok(Some(_)) => continue,
            Ok(None) => match w.phase {
                WorkerPhase::SignalUnknown(kind, code) => HelperCause::Io("signalling a worker", kind, code),
                _ => HelperCause::Closing,
            },
            Err(e) => HelperCause::io("reaping a worker", e),
        };
        if let Some(unknown) = &mut cleanup {
            // At most workers.len() entries contribute; saturation cannot hide uncertainty.
            unknown.count = unknown.count.saturating_add(1);
        } else {
            cleanup = Some(CleanupUnknown {
                count: 1,
                first_id: w.id,
                cause: failure,
            });
        }
    }
    match (result, cleanup) {
        (Ok(()), None) => Ok(()),
        (Err(cause), cleanup) => Err(HelperFailure { cause, cleanup }),
        (Ok(()), cleanup) => Err(HelperFailure {
            cause: HelperCause::Closing,
            cleanup,
        }),
    }
}

/// A worker: runs the program its socket brings, and says what it came to; its exit status.
fn worker(socket: OwnedFd) -> i32 {
    let fd = socket.as_raw_fd();
    if let Err(e) = allowlist(&worker_calls()).and_then(|p| confine(&p)) {
        if let Ok(f) = frame(FAILED, e.as_bytes()) {
            let _ = write_all(fd, &f);
        }
        return 1;
    }
    let (kind, bytes) = match read_frame(fd, usize::MAX) {
        Ok(f) => f,
        Err(_) => return 1,
    };
    if kind != PROGRAM {
        return 1;
    }
    let said = match run(fd, &String::from_utf8_lossy(&bytes)) {
        Ok(value) if value.len() <= FRAME_MAX => frame(DONE, value.as_bytes()),
        Ok(value) => frame(
            FAILED,
            format!(
                "its value is {} bytes of JSON, more than the {FRAME_MAX} a value holds",
                value.len()
            )
            .as_bytes(),
        ),
        Err(e) => frame(FAILED, clipped(&e, FRAME_MAX).as_bytes()),
    };
    match said.map(|said| write_all(fd, &said)) {
        Ok(Ok(())) => 0,
        _ => 1,
    }
}

/// The most of `s` that `max` bytes hold, cut where a character begins.
fn clipped(s: &str, max: usize) -> &str {
    let end = (0..=max.min(s.len()))
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    s.get(..end).unwrap_or_default()
}

/// Asks the instance, over `fd`, to call tool `name` with `args` (JSON): its answer, JSON
/// with its `value` or its `error`; a call past what a frame holds, or one the instance
/// cannot be asked, an `error` of its own.
fn ask(fd: RawFd, name: &str, args: &str) -> String {
    let call = format!(r#"{{"name":{},"args":{args}}}"#, crate::server::quote(name));
    let asked = if call.len() > FRAME_MAX {
        Err(format!(
            "a call of {} bytes, more than the {FRAME_MAX} a call holds",
            call.len()
        ))
    } else {
        frame(CALL, call.as_bytes()).and_then(|f| {
            write_all(fd, &f).map_err(|e| format!("asking the instance: {e}"))?;
            match read_frame(fd, usize::MAX).map_err(|e| format!("the instance's answer: {e}"))? {
                (REPLY, bytes) => String::from_utf8(bytes).map_err(|e| format!("the instance's answer: {e}")),
                (other, _) => Err(format!("the instance answered with frame {other}")),
            }
        })
    };
    asked.unwrap_or_else(|why| format!(r#"{{"error":{}}}"#, crate::server::quote(&why)))
}

/// Evaluates `text`, a script named `name` whose first line is numbered `line`, strictly
/// (QuickJS-NG's `JS_Eval2`, which numbers lines from where it is told, but for 0, which
/// it reads as 1, and records no position on a line numbered below 0).
fn eval<'js>(
    ctx: &rquickjs::Ctx<'js>,
    text: &str,
    name: &std::ffi::CStr,
    line: i32,
) -> rquickjs::Result<rquickjs::Value<'js>> {
    use rquickjs::qjs;
    let mut input = Vec::with_capacity(text.len() + 1);
    input.extend_from_slice(text.as_bytes());
    input.push(0);
    let mut options = qjs::JSEvalOptions {
        version: qjs::JS_EVAL_OPTIONS_VERSION as _,
        eval_flags: (qjs::JS_EVAL_TYPE_GLOBAL | qjs::JS_EVAL_FLAG_STRICT) as _,
        filename: name.as_ptr(),
        line_num: line,
    };
    // SAFETY: JS_Eval2 in this context, over input NUL-terminated past the length given and
    // options that outlive the call; the value it returns is new, and `from_raw` owns it.
    let value = unsafe {
        let raw = qjs::JS_Eval2(
            ctx.as_raw().as_ptr(),
            input.as_ptr().cast(),
            text.len() as _,
            &raw mut options,
        );
        rquickjs::Value::from_raw(ctx.clone(), raw)
    };
    if value.is_exception() {
        return Err(rquickjs::Error::Exception);
    }
    Ok(value)
}

/// Runs `source`, the body of an async function, with `shards` in reach, in strict mode:
/// the value it returned, as JSON (`null` where it returned none), or why it failed. The
/// prelude runs as a script of its own (`shards`); the program as one named `program`,
/// the function it is the body of opening on the lines numbered -1 and 0 before it, so
/// that the lines and columns an error names are the program's as it was sent.
fn run(fd: RawFd, source: &str) -> Result<String, String> {
    let rt = Runtime::new().map_err(|e| format!("the runtime: {e}"))?;
    let ctx = Context::full(&rt).map_err(|e| format!("its context: {e}"))?;
    ctx.with(|ctx| {
        let call = Function::new(ctx.clone(), move |name: String, args: String| {
            ask(fd, &name, &args)
        })
        .map_err(|e| format!("its tools: {e}"))?;
        ctx.globals()
            .set("__call", call)
            .map_err(|e| format!("its tools: {e}"))?;
        eval(&ctx, PRELUDE, c"shards", 1)
            .catch(&ctx)
            .map_err(|e| described(&ctx, e))?;
        let wrapped = format!("(async () => {{\n\n{source}\n}})()");
        let promise = eval(&ctx, &wrapped, c"program", -1)
            .and_then(|v| Promise::from_js(&ctx, v))
            .catch(&ctx)
            .map_err(|e| described(&ctx, e))?;
        let value: rquickjs::Value = promise.finish().catch(&ctx).map_err(|e| match e {
            // Its promise still pending with no job left to run: nothing will settle it.
            rquickjs::CaughtError::Error(rquickjs::Error::WouldBlock) => {
                "the program awaits what nothing will settle".to_string()
            }
            e => described(&ctx, e),
        })?;
        if value.is_undefined() {
            return Ok("null".to_string());
        }
        let json = ctx
            .json_stringify(value)
            .catch(&ctx)
            .map_err(|e| described(&ctx, e))?;
        match json {
            Some(s) => s.to_string().map_err(|e| e.to_string()),
            None => Ok("null".to_string()),
        }
    })
}

/// What a program threw, as JavaScript says an error (`${name}: ${message}`), and the stack
/// it was thrown from; a thrown value that is no error, as JSON.
fn described<'js>(ctx: &rquickjs::Ctx<'js>, caught: rquickjs::CaughtError<'js>) -> String {
    match caught {
        rquickjs::CaughtError::Exception(e) => {
            let name: Option<String> = e.get("name").ok();
            let mut said = format!(
                "{}: {}",
                name.as_deref().unwrap_or("Error"),
                e.message().unwrap_or_default()
            );
            if let Some(stack) = e.stack().filter(|s| !s.trim().is_empty()) {
                said.push('\n');
                said.push_str(stack.trim_end());
            }
            said
        }
        rquickjs::CaughtError::Value(v) => {
            let json = ctx
                .json_stringify(v)
                .ok()
                .flatten()
                .and_then(|s| s.to_string().ok())
                .unwrap_or_else(|| "a value no JSON holds".to_string());
            format!("thrown: {json}")
        }
        rquickjs::CaughtError::Error(e) => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::os::unix::net::UnixStream;

    /// `source` run as a worker runs it, the instance's side played by `answer`, which
    /// answers each call: the calls made, and what the program came to.
    fn ran(source: &str, answer: impl Fn(&str) -> String) -> (Vec<String>, Result<String, String>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        std::thread::scope(|s| {
            let program = s.spawn(move || run(theirs.as_raw_fd(), source));
            let mut calls = Vec::new();
            while let Ok((CALL, bytes)) = read_frame(ours.as_raw_fd(), FRAME_MAX) {
                let call = String::from_utf8(bytes).unwrap();
                let reply = answer(&call);
                calls.push(call);
                write_all(ours.as_raw_fd(), &frame(REPLY, reply.as_bytes()).unwrap()).unwrap();
            }
            // Its end of the socket gone, a program waiting on an answer is told so.
            drop(ours);
            (calls, program.join().unwrap())
        })
    }

    /// A program awaits its tools, each a call over its socket, and returns a value, as
    /// JSON.
    #[test]
    fn a_program_calls_its_tools_over_its_socket() {
        let (calls, came) = ran(
            r#"const me = await shards.whoami();
const sent = await shards.send("agent b", "hi");
return { me, sent };"#,
            |call| {
                if call.contains(r#""whoami""#) {
                    r#"{"value":"agent a"}"#.to_string()
                } else {
                    r#"{"value":{"id":1}}"#.to_string()
                }
            },
        );
        assert_eq!(
            calls,
            [
                r#"{"name":"whoami","args":{}}"#,
                r#"{"name":"send","args":{"to":"agent b","text":"hi"}}"#
            ]
        );
        assert_eq!(came, Ok(r#"{"me":"agent a","sent":{"id":1}}"#.to_string()));
    }

    /// What the instance refuses is thrown, as an Error the program may catch. A call
    /// past the former picked 1 MiB cap still reaches the instance whole: only the
    /// protocol's u32 length bounds it now (D136).
    #[test]
    fn a_refused_call_is_thrown() {
        let (_, came) = ran(
            r#"try { await shards.send("agent c", "x"); } catch (e) { return e.message; }"#,
            |_| r#"{"error":"agent a may not send agent c requests"}"#.to_string(),
        );
        assert_eq!(came, Ok(r#""agent a may not send agent c requests""#.to_string()));
        let (calls, came) = ran(
            r#"return await shards.send("agent b", "x".repeat(1 << 20));"#,
            |_| r#"{"value":{"id":1}}"#.to_string(),
        );
        let expected = format!(
            r#"{{"name":"send","args":{{"to":"agent b","text":"{}"}}}}"#,
            "x".repeat(1 << 20)
        );
        assert_eq!(calls.len(), 1);
        assert!(calls.first() == Some(&expected), "the large call arrived changed");
        assert_eq!(came, Ok(r#"{"id":1}"#.to_string()));
    }

    /// A broker tool's typed receipt reaches the program: on the Error it throws, as its
    /// cause, and on the value it returns.
    #[test]
    fn a_programs_broker_results_keep_their_receipt() {
        let refused = r#"{"principal":"agent a","peer":"agent c","operation":null,"request":null,"outcome":"refused","effect":{"kind":"not_started"},"phase":"validated","cause":{"kind":"grant"}}"#;
        let (_, came) = ran(
            r#"try { await shards.send("agent c", "x"); } catch (e) { return [e.message, e.cause ?? null]; }"#,
            |_| format!(r#"{{"error":"agent a may not send agent c requests","broker":{refused}}}"#),
        );
        eprintln!("BROKER_RECEIPT thrown={came:?}");
        assert_eq!(
            came,
            Ok(format!(r#"["agent a may not send agent c requests",{refused}]"#))
        );
        let sent = r#"{"principal":"agent a","peer":"agent b","operation":"send","request":1,"outcome":"completed","effect":{"kind":"kernel_accepted","bytes":10},"phase":"terminal","cause":null}"#;
        let (_, came) = ran(
            r#"const v = await shards.send("agent b", "x"); return [v, v.broker ?? null];"#,
            |_| format!(r#"{{"value":{{"id":1}},"broker":{sent}}}"#),
        );
        eprintln!("BROKER_RECEIPT returned={came:?}");
        assert_eq!(came, Ok(format!(r#"[{{"id":1}},{sent}]"#)));
    }

    /// Whether `fd` is readable within `ms` milliseconds.
    fn readable(fd: &OwnedFd, ms: i32) -> bool {
        let mut p = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll(2) of one pollfd of ours.
        unsafe { libc::poll(&raw mut p, 1, ms) == 1 }
    }

    /// A control message naming program `id`: its kind, and the ID.
    fn naming(kind: u8, id: u64) -> Vec<u8> {
        [&[kind][..], &id.to_be_bytes()].concat()
    }

    /// The helper's ENDED messages within five seconds, until one names `id`: each one's ID,
    /// code and status; and whether its control socket closed meanwhile.
    fn ended_until(instance: &OwnedFd, id: u64) -> (Vec<(u64, i32, i32)>, bool) {
        let mut ended = Vec::new();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ended.iter().any(|(e, _, _)| *e == id) && std::time::Instant::now() < until {
            if !readable(instance, 100) {
                continue;
            }
            match recv_control(instance.as_raw_fd()) {
                Ok(Some((msg, _))) if msg.first() == Some(&ENDED) => ended.push((
                    u64::from_be_bytes(msg[1..9].try_into().unwrap()),
                    i32::from_be_bytes(msg[9..13].try_into().unwrap()),
                    i32::from_be_bytes(msg[13..17].try_into().unwrap()),
                )),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => return (ended, true),
            }
        }
        (ended, false)
    }

    /// Whether `socket`'s other end is still open: nothing to read, and no end, for `ms`.
    fn open_for(socket: &UnixStream, ms: u64) -> bool {
        socket
            .set_read_timeout(Some(std::time::Duration::from_millis(ms)))
            .unwrap();
        matches!((&*socket).read(&mut [0u8; 1]), Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
    }

    /// One worker the helper cannot reap ends no other. Run as the helper is, but ignoring
    /// SIGCHLD, so that the kernel reaps each worker as it ends and no waitid finds it, the
    /// helper says the one that ended did and serves on: the worker it still runs runs on,
    /// and ends when stopped.
    #[test]
    fn a_worker_not_reaped_ends_no_other() {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: socketpair(2) into a two-element array.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0);
        // SAFETY: both were just made, and are owned here alone.
        let (instance, theirs) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let mut pipe = [0 as RawFd; 2];
        // SAFETY: pipe2(2) into a two-element array.
        assert_eq!(unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: both were just made, and are owned here alone.
        let (said, report) = unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
        // SAFETY: fork(2) of the test process. The child moves its end of the control socket
        // to CONTROL_FD and the pipe's to the next, closes every other descriptor it inherited,
        // ignores SIGCHLD, serves as the helper (glibc's fork leaves malloc usable in the
        // child), writes why the helper failed if it did, and _exits, returning into no test.
        let child = unsafe { libc::fork() };
        assert!(child >= 0);
        if child == 0 {
            // SAFETY: as above, in the child alone.
            unsafe {
                let control = libc::fcntl(theirs.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64);
                let out = libc::fcntl(report.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64);
                if control < 0
                    || out < 0
                    || libc::dup2(control, CONTROL_FD) != CONTROL_FD
                    || libc::dup2(out, CONTROL_FD + 1) != CONTROL_FD + 1
                {
                    libc::_exit(2);
                }
                libc::syscall(libc::SYS_close_range, CONTROL_FD + 2, u32::MAX, 0);
                libc::signal(libc::SIGCHLD, libc::SIG_IGN);
                // Nor its workers dump a core: a dump holds the dying worker's end, and with
                // it its ENDED, for seconds (systemd-coredump). A debug build's worker 1 ends
                // by SIGSYS, OwnedFd's debug check of its socket being an fcntl(2) its filter
                // refuses; a release build's exits.
                libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
                let code = match serve_helper() {
                    Ok(()) => 0,
                    Err(why) => {
                        let why = why.to_string();
                        libc::write(CONTROL_FD + 1, why.as_ptr().cast(), why.len());
                        3
                    }
                };
                libc::_exit(code);
            }
        }
        drop(theirs);
        drop(report);
        // Program 2's worker waits for the program its socket never brings.
        let (two, two_worker) = UnixStream::pair().unwrap();
        send_control(
            instance.as_raw_fd(),
            &naming(RUN, 2),
            Some(two_worker.as_raw_fd()),
        )
        .unwrap();
        drop(two_worker);
        // Program 1's worker finds its socket's end at once, and exits.
        let (one, one_worker) = UnixStream::pair().unwrap();
        drop(one);
        send_control(
            instance.as_raw_fd(),
            &naming(RUN, 1),
            Some(one_worker.as_raw_fd()),
        )
        .unwrap();
        drop(one_worker);
        let (first, closed) = ended_until(&instance, 1);
        let two_ran_on = open_for(&two, 300);
        let (second, closed_later) = if closed {
            (Vec::new(), true)
        } else {
            let _ = send_control(instance.as_raw_fd(), &naming(STOP, 2), None);
            ended_until(&instance, 2)
        };
        let two_ended = !open_for(&two, 1000);
        drop(instance);
        let mut status = 0;
        // SAFETY: waitpid(2) of the child forked above.
        assert_eq!(unsafe { libc::waitpid(child, &raw mut status, 0) }, child);
        let mut why = String::new();
        std::fs::File::from(said).read_to_string(&mut why).unwrap();
        eprintln!(
            "REAP_UNKNOWN first={first:?} control_closed={closed} worker_two_ran_on={two_ran_on} second={second:?} control_closed_later={closed_later} worker_two_ended={two_ended} helper_exited={} helper_code={} helper_signal={} helper_failure={why:?}",
            libc::WIFEXITED(status),
            libc::WEXITSTATUS(status),
            if libc::WIFSIGNALED(status) {
                libc::WTERMSIG(status)
            } else {
                0
            }
        );
        assert!(!closed, "the helper ended for one worker it could not reap");
        assert!(first.iter().any(|(id, _, _)| *id == 1), "{first:?}");
        assert!(two_ran_on, "the other worker was ended with it");
        assert!(second.iter().any(|(id, _, _)| *id == 2), "{second:?}");
        assert!(two_ended);
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "{status:#x}"
        );
    }

    /// The native call `shards` is made of is out of the program's reach, and `shards`
    /// cannot be changed.
    #[test]
    fn the_call_beneath_the_tools_is_out_of_reach() {
        let (calls, came) = ran(
            r#"let changed;
try { shards.whoami = null; } catch (e) { changed = e.name; }
return [typeof __call, typeof globalThis.__call, Object.isFrozen(shards), changed, typeof shards.whoami];"#,
            |_| String::new(),
        );
        assert!(calls.is_empty(), "{calls:?}");
        assert_eq!(
            came,
            Ok(r#"["undefined","undefined",true,"TypeError","function"]"#.to_string())
        );
    }

    /// A program that returns nothing came to null; one whose value JSON cannot hold, one
    /// that throws, and one that awaits what nothing settles each failed, and say why.
    #[test]
    fn what_a_program_came_to() {
        let none = |_: &str| String::new();
        assert_eq!(ran("return;", none).1, Ok("null".to_string()));
        let big = ran("return 1n;", none).1.unwrap_err();
        assert!(big.starts_with("TypeError: "), "{big}");
        let thrown = ran("throw 7;", none).1;
        assert_eq!(thrown, Err("thrown: 7".to_string()));
        let error = ran("const a = 1;\n  throw new RangeError(\"far\");", none).1;
        assert_eq!(
            error,
            Err(
                "RangeError: far\n    at <anonymous> (program:2:13)\n    at <eval> (program:3:1)".to_string()
            )
        );
        let syntax = ran("return {;", none).1.unwrap_err();
        assert!(
            syntax.starts_with("SyntaxError: ") && syntax.contains("program:1:9"),
            "{syntax}"
        );
        let pending = ran("await new Promise(() => {});", none).1;
        assert_eq!(
            pending,
            Err("the program awaits what nothing will settle".to_string())
        );
    }

    /// A frame is read whole, and refused past its bound.
    #[test]
    fn frames_are_read_whole_and_within_their_bound() {
        let whole = frame(CALL, b"abc").unwrap();
        assert_eq!(parse_frame(&whole), Ok(Some((CALL, &b"abc"[..], 8))));
        assert_eq!(parse_frame(&whole[..7]), Ok(None));
        assert_eq!(parse_frame(&whole[..3]), Ok(None));
        // Its length says up to u32::MAX, which a frame may be.
        let mut most = vec![CALL];
        most.extend_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(parse_frame(&most), Ok(None));
        let (ours, theirs) = UnixStream::pair().unwrap();
        write_all(ours.as_raw_fd(), &whole).unwrap();
        assert!(read_frame(theirs.as_raw_fd(), 2).is_err());
    }

    /// A worker's filter lets it make the calls it needs, and ends it for any other: one
    /// that would open a file, and, on x86_64, an x32 call.
    #[test]
    fn a_worker_is_ended_for_a_call_its_filter_refuses() {
        let filter = allowlist(&worker_calls()).unwrap();
        let ended = |call: fn()| {
            // SAFETY: fork(2); the child loads the filter made above, makes one call and
            // exits, calling nothing that allocates.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                // No core of it: dumped as SIGSYS ends it (seconds, through systemd-coredump),
                // it would hold every descriptor this test process had open, other tests'
                // sockets too, past their ends.
                // SAFETY: prctl(2) with constant arguments.
                unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
                let code = if confine(&filter).is_ok() {
                    call();
                    0
                } else {
                    2
                };
                // SAFETY: _exit(2) of the child.
                unsafe { libc::_exit(code) };
            }
            let mut status = 0;
            // SAFETY: waitpid(2) of the child just forked.
            assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
            status
        };
        let allowed = ended(|| {
            let mut b = [0u8; 8];
            // SAFETY: getrandom(2) into a buffer of ours.
            unsafe { libc::getrandom(b.as_mut_ptr().cast(), b.len(), 0) };
        });
        assert!(
            libc::WIFEXITED(allowed) && libc::WEXITSTATUS(allowed) == 0,
            "{allowed:#x}"
        );
        let opened = ended(|| {
            // SAFETY: openat(2) of a NUL-terminated path, which the filter refuses.
            unsafe { libc::syscall(libc::SYS_openat, libc::AT_FDCWD, c"/".as_ptr(), libc::O_RDONLY) };
        });
        assert!(
            libc::WIFSIGNALED(opened) && libc::WTERMSIG(opened) == libc::SIGSYS,
            "{opened:#x}"
        );
        #[cfg(target_arch = "x86_64")]
        {
            let x32 = ended(|| {
                // SAFETY: getpid(2) as an x32 call, which the filter refuses.
                unsafe { libc::syscall(libc::SYS_getpid | 0x4000_0000) };
            });
            assert!(
                libc::WIFSIGNALED(x32) && libc::WTERMSIG(x32) == libc::SIGSYS,
                "{x32:#x}"
            );
        }
    }
}
