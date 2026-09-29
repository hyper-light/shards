//! Messages between shards' processes over Unix-domain stream sockets, with open file
//! descriptors passed alongside (`SCM_RIGHTS`): how the CLI, the daemon and warm VMM
//! processes hand each other requests, stdio and connections.
//!
//! A message is a kind byte, a big-endian u32 payload length, then the payload. Its
//! descriptors travel with its first byte, so a stream never splits them from their
//! message. Received descriptors are close-on-exec: atomically where the kernel offers
//! `MSG_CMSG_CLOEXEC` (Linux), right after receipt elsewhere.

#![cfg(unix)]

use std::ffi::{CString, OsStr};
use std::io::{self, Read, Write};
use std::mem::{MaybeUninit, size_of};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

/// Message kinds between the daemon, warm VMs and clients.
pub mod kind {
    /// Warm VM → daemon: the guest is connected and waiting for its command.
    pub const READY: u8 = 1;
    /// Daemon → warm VM: [`RUN_INTERACTIVE`](super::RUN_INTERACTIVE) flags, then the
    /// command (`shards_abi::run::Spec`). Descriptors: the client's connection, then its
    /// stdin, stdout and stderr.
    pub const RUN: u8 = 2;
    /// Warm VM → client: the command's exit status, one byte.
    pub const EXIT: u8 = 3;
    /// Client → warm VM: a signal for the command, its Linux number as a big-endian u32.
    pub const SIGNAL: u8 = 4;
}

/// A `kind::RUN` flag: the command reads the client's stdin (`-i`).
pub const RUN_INTERACTIVE: u8 = 1;

/// The largest payload a message may carry.
pub const MAX_PAYLOAD: usize = 1 << 20;
/// The most descriptors a message may carry.
pub const MAX_FDS: usize = 8;
const HEADER: usize = 5;

/// A received message.
#[derive(Debug)]
pub struct Message {
    pub kind: u8,
    pub payload: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}

/// Room for one `cmsghdr` and `MAX_FDS` descriptors, aligned as the header requires.
#[repr(C, align(8))]
struct Control([MaybeUninit<u8>; 128]);

impl Control {
    fn new() -> Control {
        Control([MaybeUninit::uninit(); 128])
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
pub fn recv(sock: &UnixStream) -> io::Result<Option<Message>> {
    let mut header = [0u8; HEADER];
    let mut control = Control::new();
    let mut iov = libc::iovec {
        iov_base: header.as_mut_ptr().cast(),
        iov_len: HEADER,
    };
    // SAFETY: as in `send`.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.0.as_mut_ptr().cast();
    msg.msg_controllen = space(MAX_FDS) as _;
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
    // Take ownership of every descriptor first, so each is closed on any error below.
    let fds = received_fds(&msg)?;
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "more descriptors than a message may carry",
        ));
    }
    if got == 0 {
        return if fds.is_empty() {
            Ok(None)
        } else {
            Err(io::ErrorKind::UnexpectedEof.into())
        };
    }
    let mut reader = sock;
    reader.read_exact(header.get_mut(got..).unwrap_or_default())?;
    let [kind, l0, l1, l2, l3] = header;
    let len = u32::from_be_bytes([l0, l1, l2, l3]) as usize;
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message payload too large",
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    Ok(Some(Message { kind, payload, fds }))
}

/// The descriptors in `msg`'s control data, owned and close-on-exec.
fn received_fds(msg: &libc::msghdr) -> io::Result<Vec<OwnedFd>> {
    let mut fds = Vec::new();
    // SAFETY: walking the control messages the kernel wrote into our buffer; each
    // SCM_RIGHTS descriptor is new to this process and owned by nothing else.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let bytes = ((*cmsg).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                for i in 0..bytes / size_of::<RawFd>() {
                    fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            cmsg = libc::CMSG_NXTHDR(msg, cmsg);
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    for fd in &fds {
        // SAFETY: fcntl(2) on a descriptor we own.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(fds)
}

/// A child started by [`spawn`]. Waiting reaps it; dropping it does not.
#[derive(Debug)]
pub struct Child {
    pid: libc::pid_t,
}

impl Child {
    pub fn id(&self) -> u32 {
        self.pid.unsigned_abs()
    }

    /// Waits for the child to end: its exit status, or 128 plus the signal that ended it.
    pub fn wait(&self) -> io::Result<i32> {
        let mut status = 0;
        loop {
            // SAFETY: waitpid(2) for our own child into a local.
            let r = unsafe { libc::waitpid(self.pid, &mut status, 0) };
            if r == self.pid {
                break;
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        if libc::WIFEXITED(status) {
            Ok(libc::WEXITSTATUS(status))
        } else {
            Ok(128 + libc::WTERMSIG(status))
        }
    }

    pub fn kill(&self, signal: libc::c_int) -> io::Result<()> {
        // SAFETY: kill(2) of our own, not yet reaped, child.
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
pub fn spawn(program: &Path, args: &[&OsStr], fds: &[(BorrowedFd<'_>, RawFd)]) -> io::Result<Child> {
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
            let flags = libc::POSIX_SPAWN_SETSIGMASK
                | libc::POSIX_SPAWN_SETSIGDEF
                | libc::POSIX_SPAWN_CLOEXEC_DEFAULT;
            #[cfg(not(target_vendor = "apple"))]
            let flags = libc::POSIX_SPAWN_SETSIGMASK | libc::POSIX_SPAWN_SETSIGDEF;
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
            Ok(Child { pid })
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
    use std::os::fd::AsFd;

    use super::*;

    fn pipe() -> (File, File) {
        let mut fds = [0; 2];
        // SAFETY: pipe(2) fills both descriptors.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: fresh descriptors nothing else owns.
        unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
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
        let child = spawn(Path::new("/bin/sh"), &["-c".as_ref(), script.as_ref()], &[]).unwrap();
        assert_eq!(
            child.wait().unwrap(),
            0,
            "the child inherited descriptor {}",
            w.as_raw_fd()
        );
    }

    #[test]
    fn statuses_and_signals() {
        let child = spawn(Path::new("/bin/sh"), &["-c".as_ref(), "exit 7".as_ref()], &[]).unwrap();
        assert_eq!(child.wait().unwrap(), 7);
        let child = spawn(Path::new("/bin/sh"), &["-c".as_ref(), "sleep 30".as_ref()], &[]).unwrap();
        child.kill(libc::SIGTERM).unwrap();
        assert_eq!(child.wait().unwrap(), 128 + libc::SIGTERM);
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
