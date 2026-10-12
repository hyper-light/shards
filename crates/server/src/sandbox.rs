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

/// The most a frame of the worker's holds, a call or its value, as a request's body holds
/// (`server::BODY_MAX`): a worker may make its instance hold no more for it than a request
/// may. The instance's frames, the program (a request's body) and what its calls return,
/// are bounded by what the instance holds, and the worker reads them whole.
pub const FRAME_MAX: usize = 1 << 20;

/// Where the helper finds its control socket.
pub const CONTROL_FD: RawFd = 3;

/// What the program runs in: `shards`, its tools, each an async function that asks the
/// instance and throws what it refuses; the native call it is made of taken out of reach.
const PRELUDE: &str = r#"const shards = (() => {
  const call = globalThis.__call;
  delete globalThis.__call;
  const tool = (name, args) => {
    const r = JSON.parse(call(name, JSON.stringify(args)));
    if (r.error !== undefined) throw new Error(r.error);
    return r.value;
  };
  return Object.freeze({
    whoami: async () => tool("whoami", {}),
    mcp: async () => tool("mcp", {}),
    peers: async () => tool("peers", {}),
    send: async (to, text) => tool("send", { to, text }),
    receive: async () => tool("receive", {}),
    answer: async (to, id, text) => tool("answer", { to, id, text }),
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

/// A frame: its kind, its length and its bytes; an error for more bytes than its length
/// can say.
pub fn frame(kind: u8, bytes: &[u8]) -> Result<Vec<u8>, String> {
    let len = u32::try_from(bytes.len()).map_err(|_| format!("a frame of {} bytes", bytes.len()))?;
    let mut out = Vec::with_capacity(bytes.len() + 5);
    out.push(kind);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(out)
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
    let mut bytes = vec![0u8; len];
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
/// descriptors, polling, forking (`clone` on arm64, `fork` on x86_64, as musl's `_Fork`
/// makes one), each worker's pidfd, to kill it and reap it, and what a worker calls to
/// confine itself further before it reads its program (`prctl`, `seccomp`), which its
/// own filter then refuses.
fn helper_calls() -> Vec<libc::c_long> {
    let mut calls = worker_calls();
    calls.extend([
        libc::SYS_recvmsg,
        libc::SYS_sendmsg,
        libc::SYS_ppoll,
        libc::SYS_clone,
        libc::SYS_pidfd_open,
        libc::SYS_pidfd_send_signal,
        libc::SYS_waitid,
        libc::SYS_fcntl,
        libc::SYS_prctl,
        libc::SYS_seccomp,
    ]);
    #[cfg(target_arch = "x86_64")]
    calls.extend([libc::SYS_poll, libc::SYS_fork]);
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

/// A worker the helper forked: its program's ID, and its pidfd.
struct Worker {
    id: u64,
    pidfd: OwnedFd,
}

fn serve_helper() -> Result<(), String> {
    // Named as what it is, apart from the instance it serves (`shards top`, D123).
    // SAFETY: prctl(2) naming this thread, with a NUL-terminated name of 15 bytes or fewer.
    unsafe { libc::prctl(libc::PR_SET_NAME, c"shards-sandbox".as_ptr()) };
    confine(&allowlist(&helper_calls())?)?;
    let control = CONTROL_FD;
    let mut workers: Vec<Worker> = Vec::new();
    loop {
        let mut set: Vec<libc::pollfd> = std::iter::once(control)
            .chain(workers.iter().map(|w| w.pidfd.as_raw_fd()))
            .map(|fd| libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        // SAFETY: poll(2) of pollfds of ours.
        if unsafe { libc::poll(set.as_mut_ptr(), set.len() as libc::nfds_t, -1) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(os_err("polling"));
        }
        // Workers that ended: reaped, and said.
        let ended: Vec<usize> = set
            .iter()
            .skip(1)
            .enumerate()
            .filter(|(_, p)| p.revents != 0)
            .map(|(i, _)| i)
            .collect();
        for i in ended.into_iter().rev() {
            let w = workers.swap_remove(i);
            // SAFETY: an all-zero siginfo_t is valid for waitid to fill.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: waitid(2) of the worker's pidfd, into a siginfo_t of ours.
            let reaped = unsafe {
                libc::waitid(
                    libc::P_PIDFD,
                    w.pidfd.as_raw_fd() as libc::id_t,
                    &raw mut info,
                    libc::WEXITED,
                )
            };
            let status = if reaped == 0 {
                // SAFETY: si_status of a siginfo_t waitid filled for a child that ended.
                unsafe { info.si_status() }
            } else {
                -1
            };
            let code = if reaped == 0 { info.si_code } else { -1 };
            let mut said = vec![ENDED];
            said.extend_from_slice(&w.id.to_be_bytes());
            said.extend_from_slice(&code.to_be_bytes());
            said.extend_from_slice(&status.to_be_bytes());
            send_control(control, &said, None).map_err(|e| format!("saying a worker ended: {e}"))?;
        }
        if set.first().is_some_and(|p| p.revents == 0) {
            continue;
        }
        let Some((msg, fd)) = recv_control(control).map_err(|e| format!("its control socket: {e}"))? else {
            // The instance is gone: so are its programs.
            for w in &workers {
                // SAFETY: pidfd_send_signal(2) of SIGKILL to a worker's pidfd.
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        w.pidfd.as_raw_fd(),
                        libc::SIGKILL,
                        0,
                        0,
                    )
                };
            }
            return Ok(());
        };
        let id = msg
            .get(1..9)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_be_bytes)
            .unwrap_or(0);
        match (msg.first(), fd) {
            (Some(&RUN), Some(program)) => {
                // SAFETY: fork(2) of this single-threaded process; the child runs the
                // program and exits, touching nothing of the parent's but what it holds.
                let pid = unsafe { libc::fork() };
                if pid < 0 {
                    let mut said = vec![ENDED];
                    said.extend_from_slice(&id.to_be_bytes());
                    said.extend_from_slice(&(-1i32).to_be_bytes());
                    said.extend_from_slice(&(-1i32).to_be_bytes());
                    send_control(control, &said, None).map_err(|e| format!("saying a worker ended: {e}"))?;
                    continue;
                }
                if pid == 0 {
                    // The worker: the program's socket alone, and named as what it runs.
                    // SAFETY: closing descriptors this child holds and does not use again,
                    // and prctl(2) naming this thread, with a NUL-terminated name of 15
                    // bytes or fewer.
                    unsafe {
                        libc::close(control);
                        libc::prctl(libc::PR_SET_NAME, c"shards-program".as_ptr());
                    }
                    for w in &workers {
                        // SAFETY: as above.
                        unsafe { libc::close(w.pidfd.as_raw_fd()) };
                    }
                    let code = worker(program);
                    // SAFETY: _exit(2) of the worker, which flushes nothing of the parent's.
                    unsafe { libc::_exit(code) };
                }
                drop(program);
                // SAFETY: pidfd_open(2) of the child just forked.
                let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
                let Ok(pidfd) = RawFd::try_from(pidfd) else {
                    return Err(os_err("a worker's pidfd"));
                };
                if pidfd < 0 {
                    return Err(os_err("a worker's pidfd"));
                }
                // SAFETY: a descriptor pidfd_open just gave this process.
                let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
                workers.push(Worker { id, pidfd });
            }
            (Some(&STOP), _) => {
                if let Some(w) = workers.iter().find(|w| w.id == id) {
                    // SAFETY: pidfd_send_signal(2) of SIGKILL to a worker's pidfd.
                    unsafe {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            w.pidfd.as_raw_fd(),
                            libc::SIGKILL,
                            0,
                            0,
                        )
                    };
                }
            }
            _ => return Err("a control message it does not know".into()),
        }
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

    /// What the instance refuses is thrown, as an Error the program may catch; so is a call
    /// past what a frame holds, which is never sent.
    #[test]
    fn a_refused_call_is_thrown() {
        let (_, came) = ran(
            r#"try { await shards.send("agent c", "x"); } catch (e) { return e.message; }"#,
            |_| r#"{"error":"agent a may not send agent c requests"}"#.to_string(),
        );
        assert_eq!(came, Ok(r#""agent a may not send agent c requests""#.to_string()));
        let (calls, came) = ran(
            r#"try { await shards.send("agent b", "x".repeat(1 << 20)); } catch (e) { return e.message; }"#,
            |_| r#"{"value":{"id":1}}"#.to_string(),
        );
        assert!(calls.is_empty(), "{calls:?}");
        let said = came.unwrap();
        assert!(
            said.starts_with(r#""a call of "#)
                && said.ends_with(r#" bytes, more than the 1048576 a call holds""#),
            "{said}"
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
        let mut past = vec![CALL];
        past.extend_from_slice(&u32::try_from(FRAME_MAX + 1).unwrap().to_be_bytes());
        assert!(parse_frame(&past).is_err());
        let (ours, theirs) = UnixStream::pair().unwrap();
        write_all(ours.as_raw_fd(), &past).unwrap();
        assert!(read_frame(theirs.as_raw_fd(), FRAME_MAX).is_err());
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
