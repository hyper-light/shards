//! A member's starter (confine-10, D243): the process of the member's own that starts each
//! command run in it, so that the command is born in the member's own Landlock domain.
//! landlock(7) gives a domain to the process that enters it and to its children, and has no
//! way to join one: a command that entered the member's ruleset itself was a sibling domain,
//! which the member's scopes (`LANDLOCK_SCOPE_SIGNAL`, `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`,
//! Linux 6.12) hold apart from the member's processes, so `kill 1`, `pkill`, a supervisor's
//! CLI or an abstract socket's client reached none of them.
//!
//! The member's first process forks it once it is confined, in its working directory, just
//! before its program runs ([`fork`]), so it is everything that process is: its IDs, no
//! capability, `no_new_privs`, its Landlock domain, its seccomp filter, its namespaces and
//! its cgroup (whose `pids.max` it counts in). It is that process's child, as nothing else
//! can be: that process is PID 1 of the member's PID namespace, to which the kernel refuses
//! `CLONE_PARENT` (kernel/fork.c `copy_process`: no namespace's init makes siblings). Its
//! exit signal is none, so the member's program, waiting for every child it has, sees none
//! of it (kernel/exit.c `eligible_child`: a child that reports by no SIGCHLD is waited for
//! only with `__WCLONE` or `__WALL`), and it ends with the member as every process of the
//! member's PID namespace does. It holds nothing of the
//! member's: its stdio, launch status and ruleset closed, it keeps one descriptor, its end
//! of a channel to init ([`STARTER_FD`]), which the member's program never has (close-on-
//! exec). It is not dumpable (`PR_SET_DUMPABLE` 0), so no process of the member's, which has
//! no capability, may trace it, read its memory or take its descriptors (ptrace(2),
//! `PTRACE_MODE_ATTACH` of a non-dumpable process needs `CAP_SYS_PTRACE`; pidfd_getfd(2)).
//! A process of the member's may signal it, as it may any of its own: killed, its commands
//! go with it (`PR_SET_PDEATHSIG`), and init refuses the member more ([`NotBorn::Gone`]);
//! stopped, it holds up nothing of init's, which waits on it only in its poll loop.
//!
//! Init asks it for a command with one message ([`Starter::start`]): the command's spec, a
//! memfd of init's that it maps ([`spec`]), its stdio and its error channel, as descriptors.
//! It forks the command (`CLONE_PIDFD`), answers with the command's pidfd, and leaves it to
//! exit unreaped: init reads its end from the zombie's `/proc/PID/stat`, then asks it to reap
//! it ([`Starter::reap`]), so that no PID is taken again before init has its end. It runs a
//! fork of init's address space, so it calls the kernel alone, allocating nothing.

use std::ffi::{CStr, CString};
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// Where the member's first process, and its starter, hold the starter's end of its
/// channel to init: next above the member's Landlock ruleset's.
pub(crate) const STARTER_FD: RawFd = crate::domains::RULESET_FD + 1;

/// Init's ask: a command to start; its exec's id, its terminal or not, and five
/// descriptors ([`FDS`]).
const START: u8 = 1;
/// Init's ask: a command whose end it has read is to be reaped; its PID in the member's
/// namespace.
const REAP: u8 = 2;
/// The starter's answer: born, its PID in the member's namespace, its pidfd.
const BORN: u8 = 1;
/// The starter's answer: not born, the errno of its fork.
const NOT_BORN: u8 = 2;
/// The descriptors a start carries: its spec, its stdin, stdout and stderr, its error
/// channel.
const FDS: usize = 5;
/// A message's bytes: its kind, an exec's id, and one more word.
const MESSAGE: usize = 9;
/// The words of a spec's header ([`spec`]).
const HEADER: usize = 8;
const WORD: usize = size_of::<u64>();

/// Init's end of a member's starter.
#[derive(Debug)]
pub struct Starter {
    channel: OwnedFd,
}

/// A command its starter started: its PID in init's namespace and in the member's, and a
/// pidfd of it, readable as it ends.
#[derive(Debug)]
pub struct Born {
    pub id: u32,
    pub pid: libc::pid_t,
    pub ns_pid: libc::pid_t,
    pub pidfd: OwnedFd,
}

/// What a starter answered.
#[derive(Debug)]
pub enum Answer {
    Born(Born),
    /// Its fork failed, with the errno (`EAGAIN` at the member's `pids.max`).
    NotBorn {
        id: u32,
        errno: i32,
    },
}

/// Why a starter took no ask.
#[derive(Debug)]
pub enum NotBorn {
    /// It has gone: it never started, or a process of the member's ended it.
    Gone,
    /// Its spec could not be made: a memfd of init's.
    Spec(io::Error),
}

/// A command for a starter to start, as init prepares it.
pub struct Command<'a> {
    pub id: u32,
    pub candidates: &'a [CString],
    pub explicit: bool,
    pub argv: &'a [CString],
    pub envp: &'a [CString],
    pub cwd: &'a CStr,
    pub stdio: [RawFd; 3],
    pub err: RawFd,
    pub tty: bool,
}

/// A starter's channel: init's end, and the one its member's first process holds at
/// [`STARTER_FD`] for the starter it forks. Both close on exec.
pub fn channel() -> io::Result<(Starter, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: socketpair(2) writes two descriptors into this array, owned below.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let [ours, theirs] = fds;
    // SAFETY: socketpair just made both, which nothing else owns.
    let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(ours), OwnedFd::from_raw_fd(theirs)) };
    Ok((Starter { channel: ours }, theirs))
}

impl Starter {
    /// Its channel, polled for its answers and its end.
    pub fn fd(&self) -> RawFd {
        self.channel.as_raw_fd()
    }

    /// Asks it to start `c`, without waiting: its answer comes on [`fd`](Self::fd).
    pub fn start(&self, c: &Command<'_>) -> Result<(), NotBorn> {
        let spec = spec(c).map_err(NotBorn::Spec)?;
        let [i, o, e] = c.stdio;
        let mut message = [0u8; MESSAGE];
        let id = c.id.to_ne_bytes();
        let fields = [START, id[0], id[1], id[2], id[3], u8::from(c.tty), 0, 0, 0];
        message.copy_from_slice(&fields);
        send(self.fd(), &message, &[spec.as_raw_fd(), i, o, e, c.err]).map_err(|_| NotBorn::Gone)
    }

    /// Asks it to reap its command `ns_pid`, whose end init has read; nothing where it has
    /// gone, as its commands went with it.
    pub fn reap(&self, ns_pid: libc::pid_t) {
        let pid = ns_pid.to_ne_bytes();
        let message = [REAP, 0, 0, 0, 0, pid[0], pid[1], pid[2], pid[3]];
        let _ = send(self.fd(), &message, &[]);
    }

    /// Its next answer, without waiting: none where none waits; `Err(Gone)` where it has
    /// gone.
    pub fn answer(&self) -> Result<Option<Answer>, NotBorn> {
        let mut buf = [0u8; MESSAGE];
        let (n, pidfd) = match crate::hostbind::recv_fd(self.fd(), &mut buf, libc::MSG_DONTWAIT) {
            Ok(got) => got,
            Err(e) if matches!(e.raw_os_error(), Some(libc::EAGAIN | libc::EINTR)) => return Ok(None),
            Err(_) => return Err(NotBorn::Gone),
        };
        let word = |at: usize| {
            buf.get(at..at + 4)
                .and_then(|b| b.try_into().ok())
                .map(i32::from_ne_bytes)
                .unwrap_or(0)
        };
        let id = word(1).cast_unsigned();
        match (n, buf.first(), pidfd) {
            (0, _, _) => Err(NotBorn::Gone),
            (MESSAGE, Some(&BORN), Some(pidfd)) => {
                let pid = pid_of(&pidfd).ok_or(NotBorn::Gone)?;
                Ok(Some(Answer::Born(Born {
                    id,
                    pid,
                    ns_pid: word(5),
                    pidfd,
                })))
            }
            (MESSAGE, Some(&NOT_BORN), _) => Ok(Some(Answer::NotBorn { id, errno: word(5) })),
            // Not an answer a starter gives: it is not one.
            _ => Err(NotBorn::Gone),
        }
    }
}

/// The PID in init's namespace of the process `pidfd` is of (proc(5), fdinfo's `Pid:`).
fn pid_of(pidfd: &OwnedFd) -> Option<libc::pid_t> {
    std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("Pid:"))?
        .trim()
        .parse()
        .ok()
}

/// How the command `pid`, a zombie its starter has not reaped, ended: the exit code
/// proc(5)'s `/proc/PID/stat` gives as its 52nd field, in `wait4(2)`'s form. None where it
/// is gone, reaped by the member's namespace init as its starter went.
pub fn ended(pid: libc::pid_t) -> Option<libc::c_int> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    // Its name, the second field, is in parentheses and may hold anything but its end.
    let after = stat.iter().rposition(|&b| b == b')')?;
    let rest = std::str::from_utf8(stat.get(after + 1..)?).ok()?;
    // The third field is the first past the name; the 52nd is the 50th past it.
    rest.split_ascii_whitespace()
        .nth(52 - 3)
        .and_then(|f| f.parse().ok())
}

/// A memfd holding `c` as its starter reads it: a header of [`HEADER`] words (the offsets of
/// its working directory, its candidates, their number, whether it named a path, its
/// arguments, their number, its environment, its number), then the three pointer arrays,
/// each its strings' offsets and a 0 after them, then the strings, each with its NUL. Its
/// last byte is a NUL, so a string at any offset within it ends within it.
fn spec(c: &Command<'_>) -> io::Result<OwnedFd> {
    let too_big = || io::Error::from_raw_os_error(libc::E2BIG);
    let lists: [&[CString]; 3] = [c.candidates, c.argv, c.envp];
    let slots = lists
        .iter()
        .try_fold(0usize, |n, l| n.checked_add(l.len())?.checked_add(1))
        .ok_or_else(too_big)?;
    let arrays = HEADER
        .checked_add(slots)
        .and_then(|w| w.checked_mul(WORD))
        .ok_or_else(too_big)?;
    let mut strings: Vec<u8> = Vec::new();
    let mut at = |s: &CStr| -> io::Result<u64> {
        let offset = arrays.checked_add(strings.len()).ok_or_else(too_big)?;
        strings.extend_from_slice(s.to_bytes_with_nul());
        u64::try_from(offset).map_err(|_| too_big())
    };
    let cwd = at(c.cwd)?;
    let mut words: Vec<u64> = Vec::with_capacity(arrays / WORD);
    words.resize(HEADER, 0);
    let mut heads = [0u64; 3];
    for (k, list) in lists.iter().enumerate() {
        if let Some(h) = heads.get_mut(k) {
            *h = u64::try_from(words.len().saturating_mul(WORD)).map_err(|_| too_big())?;
        }
        for s in list.iter() {
            words.push(at(s)?);
        }
        words.push(0);
    }
    let count = |l: &[CString]| u64::try_from(l.len()).unwrap_or(u64::MAX);
    let header = [
        cwd,
        heads[0],
        count(c.candidates),
        u64::from(c.explicit),
        heads[1],
        count(c.argv),
        heads[2],
        count(c.envp),
    ];
    for (w, h) in words.iter_mut().zip(header) {
        *w = h;
    }
    let mut bytes: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
    bytes.extend_from_slice(&strings);
    // SAFETY: memfd_create(2) of a NUL-terminated name.
    let fd = unsafe { libc::memfd_create(c"shards-exec".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor just made, owned here alone.
    let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    file.write_all(&bytes)?;
    Ok(file.into())
}

/// `message` and `fds` sent on `channel`, never waiting.
fn send(channel: RawFd, message: &[u8], fds: &[RawFd]) -> io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: message.as_ptr().cast_mut().cast(),
        iov_len: message.len(),
    };
    // Room for [`FDS`] descriptors' control message, aligned as cmsghdr is.
    let mut control = [0u64; 8];
    // SAFETY: zeroed is a valid msghdr.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        let bytes =
            u32::try_from(size_of_val(fds)).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        // SAFETY: CMSG_SPACE of the descriptors' size, a computation.
        let space = unsafe { libc::CMSG_SPACE(bytes) } as usize;
        if space > size_of_val(&control) {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: the first header of a control buffer of ours, of the length set.
        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&raw const msg) };
        if cmsg.is_null() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        // SAFETY: a header within the control buffer, and its data, of the length set.
        unsafe {
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(bytes) as _;
            for (i, fd) in fds.iter().enumerate() {
                std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>().add(i), *fd);
            }
        }
    }
    // SAFETY: sendmsg(2) of a message of ours, which it only reads.
    let sent = unsafe { libc::sendmsg(channel, &raw const msg, libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL) };
    match usize::try_from(sent) {
        Ok(n) if n == message.len() => Ok(()),
        Ok(_) => Err(io::Error::from_raw_os_error(libc::EMSGSIZE)),
        Err(_) => Err(io::Error::last_os_error()),
    }
}

/// Forks the member's starter, from its first process, confined and in its working
/// directory, just before its program runs; returns in that process. Where no starter can
/// be had (its fork refused: its `pids.max`, or its seccomp filter), the member runs without
/// one, and init, finding its channel closed as the member's program execs, refuses every
/// command run in it by type.
///
/// # Safety
/// Called in a process of one thread that holds its starter's channel at [`STARTER_FD`],
/// close-on-exec, and that execs or exits next.
pub(crate) unsafe fn fork() {
    // SAFETY: clone(2) as fork(2), of no exit signal (flags 0: the module's doc); the child
    // calls the kernel alone and never returns.
    unsafe {
        let pid = libc::syscall(libc::SYS_clone, 0 as libc::c_ulong, 0, 0, 0, 0);
        if pid == 0 {
            serve()
        }
    }
}

/// The starter: answers init's asks on [`STARTER_FD`] until init's end closes.
unsafe fn serve() -> ! {
    // SAFETY: system calls on this stack's buffers, in a process of one thread.
    unsafe {
        // Its commands go with it (`fork`'s doc); none of the member's may trace it.
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        libc::prctl(libc::PR_SET_NAME, c"starter".as_ptr(), 0, 0, 0);
        // Its stdio, the member's launch status and ruleset: none of them its.
        for fd in 0..STARTER_FD {
            libc::close(fd);
        }
        loop {
            let mut message = [0u8; MESSAGE];
            let mut fds = [-1 as RawFd; FDS];
            let n = receive(&mut message, &mut fds);
            if n < 0 && *libc::__errno_location() == libc::EINTR {
                continue;
            }
            if n <= 0 {
                libc::_exit(0);
            }
            let word = |at: usize| {
                message
                    .get(at..at + 4)
                    .and_then(|b| b.try_into().ok())
                    .map(i32::from_ne_bytes)
                    .unwrap_or(0)
            };
            match message.first() {
                Some(&START) if n as usize == MESSAGE && fds.iter().all(|&fd| fd >= 0) => {
                    start(word(1), message.get(5) == Some(&1), fds);
                }
                Some(&REAP) if n as usize == MESSAGE => {
                    // Its own child, a zombie init has read the end of; never waited for.
                    libc::wait4(word(5), std::ptr::null_mut(), libc::WNOHANG, std::ptr::null_mut());
                }
                _ => {}
            }
            for fd in fds {
                if fd >= 0 {
                    libc::close(fd);
                }
            }
        }
    }
}

/// One message from init into `message`, its descriptors into `fds` (close-on-exec, as
/// many as fit, the rest closed): its length, 0 at init's end, or -1.
unsafe fn receive(message: &mut [u8; MESSAGE], fds: &mut [RawFd; FDS]) -> isize {
    // SAFETY: recvmsg(2) into this stack's buffers, of the lengths set; the control
    // buffer walked by its own macros, within what recvmsg wrote.
    unsafe {
        let mut iov = libc::iovec {
            iov_base: message.as_mut_ptr().cast(),
            iov_len: message.len(),
        };
        let mut control = [0u64; 8];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = size_of_val(&control) as _;
        let n = libc::recvmsg(STARTER_FD, &raw mut msg, libc::MSG_CMSG_CLOEXEC);
        let mut kept = 0;
        let mut cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
        while n > 0 && !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let count = ((*cmsg).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize)
                    / size_of::<RawFd>();
                for i in 0..count {
                    let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>().add(i));
                    match fds.get_mut(kept) {
                        Some(slot) => *slot = fd,
                        None => {
                            libc::close(fd);
                        }
                    }
                    kept += 1;
                }
            }
            cmsg = libc::CMSG_NXTHDR(&raw const msg, cmsg);
        }
        n
    }
}

/// A spec mapped: its working directory, its candidates, whether it named a path, its
/// arguments and environment, each a NULL-terminated array, pointing into the mapping.
struct Loaded {
    cwd: *const libc::c_char,
    candidates: *const *const libc::c_char,
    explicit: bool,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
}

/// The spec `fd` holds ([`spec`]) mapped privately at `*base` (`*len` bytes) and made
/// pointers of; none where it is not one.
unsafe fn load(fd: RawFd, base: &mut *mut libc::c_void, len: &mut usize) -> Option<Loaded> {
    // SAFETY: fstat(2) and mmap(2) of a descriptor init sent; the mapping's words read and
    // written within its length, each offset checked against it.
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 {
            return None;
        }
        let size = usize::try_from(st.st_size).ok()?;
        if size < HEADER * WORD {
            return None;
        }
        let at = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE,
            fd,
            0,
        );
        if at == libc::MAP_FAILED {
            return None;
        }
        (*base, *len) = (at, size);
        let bytes = at.cast::<u8>();
        if *bytes.add(size - 1) != 0 {
            return None;
        }
        let words = at.cast::<u64>();
        let header = |i: usize| usize::try_from(*words.add(i)).ok();
        let string =
            |offset: usize| (offset < size).then(|| bytes.add(offset).cast::<libc::c_char>().cast_const());
        // The `n` offsets at `offset`, and the 0 after them, made pointers and a NULL.
        let array = |offset: usize, n: usize| -> Option<*const *const libc::c_char> {
            let end = n.checked_add(1)?.checked_mul(WORD)?.checked_add(offset)?;
            if !offset.is_multiple_of(WORD) || end > size {
                return None;
            }
            let slots = words.add(offset / WORD);
            for i in 0..n {
                let s = string(usize::try_from(*slots.add(i)).ok()?)?;
                *slots.add(i) = s as u64;
            }
            *slots.add(n) = 0;
            Some(slots.cast::<*const libc::c_char>().cast_const())
        };
        Some(Loaded {
            cwd: string(header(0)?)?,
            candidates: array(header(1)?, header(2)?)?,
            explicit: header(3)? != 0,
            argv: array(header(4)?, header(5)?)?,
            envp: array(header(6)?, header(7)?)?,
        })
    }
}

/// Starts exec `id` from the descriptors init sent, `[spec, stdin, stdout, stderr, error
/// channel]`, and answers init.
unsafe fn start(id: i32, tty: bool, fds: [RawFd; FDS]) {
    let [spec, stdin, stdout, stderr, err] = fds;
    // SAFETY: system calls on this stack's buffers and the spec's private mapping, which
    // the child keeps a copy of as it is forked and this process lets go of after.
    unsafe {
        let mut base = std::ptr::null_mut();
        let mut len = 0;
        let loaded = load(spec, &mut base, &mut len);
        let mut pidfd: libc::c_int = -1;
        let pid = match &loaded {
            Some(_) => libc::syscall(
                libc::SYS_clone,
                (libc::CLONE_PIDFD | libc::SIGCHLD) as libc::c_ulong,
                0,
                &raw mut pidfd,
                0,
                0,
            ),
            None => {
                *libc::__errno_location() = libc::EINVAL;
                -1
            }
        };
        if pid == 0
            && let Some(l) = &loaded
        {
            child(l, tty, [stdin, stdout, stderr], err)
        }
        let errno = *libc::__errno_location();
        if !base.is_null() {
            libc::munmap(base, len);
        }
        let id = id.to_ne_bytes();
        let (kind, word) = match i32::try_from(pid) {
            Ok(pid) if pid > 0 => (BORN, pid),
            _ => (NOT_BORN, errno),
        };
        let w = word.to_ne_bytes();
        let message = [kind, id[0], id[1], id[2], id[3], w[0], w[1], w[2], w[3]];
        let fds: &[RawFd] = if pidfd >= 0 { &[pidfd] } else { &[] };
        let _ = send(STARTER_FD, &message, fds);
        if pidfd >= 0 {
            libc::close(pidfd);
        }
    }
}

/// The command: its stdio given, its session its terminal's, in its working directory, its
/// program found there and run; why not, said on its error channel as every exec's is
/// (`run::report`).
unsafe fn child(l: &Loaded, tty: bool, stdio: [RawFd; 3], err: RawFd) -> ! {
    // SAFETY: system calls on the descriptors init sent and the spec's mapping, in a process
    // of one thread that execs or exits.
    unsafe {
        // A member that ends its starter ends what it started (`fork`'s doc).
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
        // Each above every descriptor this process has, so that none is overwritten as its
        // stdio is laid at 0, 1 and 2.
        let above = |fd: RawFd| libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, STARTER_FD + 1);
        let err = above(err);
        if err < 0 {
            libc::_exit(127);
        }
        let [i, o, e] = stdio.map(above);
        if i < 0 || o < 0 || e < 0 || libc::dup2(i, 0) < 0 || libc::dup2(o, 1) < 0 || libc::dup2(e, 2) < 0 {
            crate::run::report(err, crate::run::step::EXEC, 0);
        }
        // No descriptor but its stdio and its error channel, which closes as it execs.
        let at = u32::try_from(err).unwrap_or(u32::MAX);
        libc::syscall(libc::SYS_close_range, 3u32, at.saturating_sub(1), 0u32);
        libc::syscall(libc::SYS_close_range, at.saturating_add(1), u32::MAX, 0u32);
        if tty && (libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) != 0) {
            crate::run::report(err, crate::run::step::TTY, 0);
        }
        if libc::chdir(l.cwd) != 0 {
            crate::run::report(err, crate::run::step::CHDIR, 0);
        }
        let (which, at) = crate::run::exec_found(each(l.candidates), l.explicit, l.argv, l.envp);
        crate::run::report(err, which, at)
    }
}

/// The strings of NULL-terminated array `list`.
unsafe fn each<'a>(list: *const *const libc::c_char) -> impl Iterator<Item = &'a CStr> {
    (0..)
        // SAFETY: the caller's array, read up to its NULL.
        .map(move |i| unsafe { *list.add(i) })
        .take_while(|p| !p.is_null())
        // SAFETY: a NUL-terminated string within the spec's mapping ([`load`]).
        .map(|p| unsafe { CStr::from_ptr(p) })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    /// A command's spec is read by its starter as init wrote it: its working directory,
    /// whether it named a path, and each list whole and NULL-terminated, pointers into the
    /// starter's mapping; one that is not a spec is none.
    #[test]
    fn a_spec_is_read_as_it_was_written() {
        let cs = |l: &[&str]| l.iter().map(|s| CString::new(*s).unwrap()).collect::<Vec<_>>();
        let (candidates, argv, envp) = (cs(&["./bin/x", "/usr/bin/x"]), cs(&["x", "-y", ""]), cs(&[]));
        let c = Command {
            id: 1,
            candidates: &candidates,
            explicit: false,
            argv: &argv,
            envp: &envp,
            cwd: c"/work dir",
            stdio: [0, 1, 2],
            err: 2,
            tty: false,
        };
        let fd = spec(&c).unwrap();
        let (mut base, mut len) = (std::ptr::null_mut(), 0);
        // SAFETY: a spec of ours, mapped privately here and let go of below.
        let loaded = unsafe { load(fd.as_raw_fd(), &mut base, &mut len) }.unwrap();
        let strings = |list| -> Vec<String> {
            // SAFETY: an array `load` made, within the mapping.
            unsafe { each(list) }
                .map(|s| s.to_str().unwrap().to_owned())
                .collect()
        };
        // SAFETY: a string within the mapping.
        assert_eq!(unsafe { CStr::from_ptr(loaded.cwd) }, c"/work dir");
        assert!(!loaded.explicit);
        assert_eq!(strings(loaded.candidates), ["./bin/x", "/usr/bin/x"]);
        assert_eq!(strings(loaded.argv), ["x", "-y", ""]);
        assert!(strings(loaded.envp).is_empty());
        // SAFETY: the mapping `load` made, unmapped once.
        unsafe { libc::munmap(base, len) };
        // A memfd that holds less than a header, or whose last byte is no NUL, is none.
        for bytes in [&[0u8; 8][..], &[1u8; 200][..]] {
            // SAFETY: memfd_create(2) of a NUL-terminated name.
            let fd = unsafe { OwnedFd::from_raw_fd(libc::memfd_create(c"t".as_ptr(), libc::MFD_CLOEXEC)) };
            std::fs::File::from(fd.try_clone().unwrap())
                .write_all(bytes)
                .unwrap();
            let (mut base, mut len) = (std::ptr::null_mut(), 0);
            // SAFETY: as above.
            assert!(unsafe { load(fd.as_raw_fd(), &mut base, &mut len) }.is_none());
            if !base.is_null() {
                // SAFETY: as above.
                unsafe { libc::munmap(base, len) };
            }
        }
    }

    /// A command's end is read from its zombie as `wait4(2)` would say it, and is none once
    /// it is reaped.
    #[test]
    fn an_end_is_read_from_a_zombie() {
        for (code, status) in [(7, 7 << 8), (0, 0)] {
            // SAFETY: fork(2); the child only _exit(2)s.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0);
            if pid == 0 {
                // SAFETY: ends the child at once.
                unsafe { libc::_exit(code) };
            }
            // SAFETY: pidfd_open(2) of our child, and poll(2) of it until it has exited.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as RawFd;
            let mut p = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll(2) of one pollfd of ours.
            assert_eq!(unsafe { libc::poll(&raw mut p, 1, -1) }, 1);
            assert_eq!(ended(pid), Some(status));
            // SAFETY: waitpid(2) of our own child, then close(2) of its pidfd.
            unsafe {
                libc::waitpid(pid, std::ptr::null_mut(), 0);
                libc::close(fd);
            }
            assert_eq!(ended(pid), None);
        }
    }
}
