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

impl Identity {
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
        let mut rest = Vec::with_capacity(total - sent);
        rest.extend(header.iter().chain(payload).skip(sent));
        (&*sock).write_all(&rest)?;
    }
    Ok(())
}

/// Receives one message. `None` if the peer closed the stream between messages.
///
/// Every read is a `recvmsg` with room for all the descriptors a kernel passes at once, so
/// descriptors arriving with any part of a message become this process's to close.
pub fn recv(sock: &UnixStream) -> io::Result<Option<Message>> {
    let mut fds = Vec::new();
    let mut header = [0u8; HEADER];
    let got = recv_part(sock, &mut header, &mut fds)?;
    if got == 0 {
        return if fds.is_empty() {
            Ok(None)
        } else {
            Err(io::ErrorKind::UnexpectedEof.into())
        };
    }
    fill(sock, header.get_mut(got..).unwrap_or_default(), &mut fds)?;
    let [kind, l0, l1, l2, l3] = header;
    let len = u32::from_be_bytes([l0, l1, l2, l3]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message payload too large",
        ));
    }
    let mut payload = vec![0u8; len];
    fill(sock, &mut payload, &mut fds)?;
    if fds.len() > MAX_FDS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "more descriptors than a message may carry",
        ));
    }
    Ok(Some(Message { kind, payload, fds }))
}

/// Reads exactly `buf.len()` more bytes of a message.
fn fill(sock: &UnixStream, mut buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> io::Result<()> {
    while !buf.is_empty() {
        let n = recv_part(sock, buf, fds)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf = buf.get_mut(n..).unwrap_or_default();
    }
    Ok(())
}

/// One `recvmsg` into `buf`. Returns the bytes read, and adds the descriptors that came
/// with them to `fds`, owned and close-on-exec.
fn recv_part(sock: &UnixStream, buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> io::Result<usize> {
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

/// Where a daemon the client starts writes its messages.
pub fn log(home: &Path) -> PathBuf {
    home.join("daemon.log")
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
    let cstring = |s: &OsStr| {
        CString::new(s.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in an argument"))
    };
    let path = cstring(program.as_os_str())?;
    let mut argv = vec![path.clone()];
    for a in args {
        argv.push(cstring(a)?);
    }
    let env: Vec<CString> = std::env::vars_os()
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
            for (fd, target) in fds {
                check(libc::posix_spawn_file_actions_adddup2(
                    &mut actions,
                    fd.as_raw_fd(),
                    *target,
                ))?;
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

    #[test]
    fn an_end_inside_a_message_is_an_error() {
        let (a, b) = UnixStream::pair().unwrap();
        (&a).write_all(&[3, 0, 0, 0, 10, b'x']).unwrap();
        drop(a);
        assert_eq!(recv(&b).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
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
