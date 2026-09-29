//! Waiting for host sockets to become readable or writable.
//!
//! Linux uses poll(2): for Unix sockets it reports POLLHUP only when both directions are
//! shut (net/unix/af_unix.c `unix_poll`). macOS uses kqueue(2). Its poll(2) is built on
//! kqueue and turns EV_EOF into POLLHUP, so a peer's half-close reads as a hangup. POLLHUP
//! is always reported, so a connection that only waits to write to such a socket would
//! wake without end. kqueue keeps read EOF and write readiness apart.

use std::io;
use std::os::fd::RawFd;
use std::time::Duration;

/// What to wait for on one descriptor.
#[derive(Debug, Clone, Copy)]
pub struct Interest<T> {
    pub fd: RawFd,
    pub read: bool,
    pub write: bool,
    pub token: T,
}

/// What became ready: `read` covers data, EOF and errors, `write` room and errors, so
/// the next read or write reports what happened.
#[derive(Debug, Clone, Copy)]
pub struct Ready<T> {
    pub token: T,
    pub read: bool,
    pub write: bool,
}

#[cfg(not(target_os = "macos"))]
fn millis(timeout: Option<Duration>) -> libc::c_int {
    timeout.map_or(-1, |t| {
        // Round up, so a deadline is never missed by waking a moment early.
        let ms = t.as_micros().div_ceil(1000);
        libc::c_int::try_from(ms).unwrap_or(libc::c_int::MAX)
    })
}

/// Waits until an interest is ready or `timeout` passes; appends what is ready to `out`.
#[cfg(not(target_os = "macos"))]
pub fn wait<T: Copy>(
    interests: &[Interest<T>],
    timeout: Option<Duration>,
    out: &mut Vec<Ready<T>>,
) -> io::Result<()> {
    let mut fds: Vec<libc::pollfd> = interests
        .iter()
        .map(|i| libc::pollfd {
            fd: i.fd,
            events: (if i.read { libc::POLLIN } else { 0 }) | (if i.write { libc::POLLOUT } else { 0 }),
            revents: 0,
        })
        .collect();
    // SAFETY: `fds` is a valid array of `fds.len()` entries.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis(timeout)) };
    if n < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted {
            Ok(())
        } else {
            Err(e)
        };
    }
    for (f, i) in fds.iter().zip(interests) {
        let failed = f.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
        let read = i.read && (f.revents & libc::POLLIN != 0 || failed);
        let write = i.write && (f.revents & libc::POLLOUT != 0 || failed);
        if read || write {
            out.push(Ready {
                token: i.token,
                read,
                write,
            });
        }
    }
    Ok(())
}

/// Waits until an interest is ready or `timeout` passes; appends what is ready to `out`.
/// Each call uses a fresh kqueue, so descriptors closed or reused between calls leave no
/// stale registrations.
#[cfg(target_os = "macos")]
pub fn wait<T: Copy>(
    interests: &[Interest<T>],
    timeout: Option<Duration>,
    out: &mut Vec<Ready<T>>,
) -> io::Result<()> {
    use std::os::fd::{FromRawFd, OwnedFd};

    // SAFETY: kqueue(2) takes no arguments.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns; closed when this returns.
    let _kq = unsafe { OwnedFd::from_raw_fd(kq) };
    let mut changes = Vec::with_capacity(2 * interests.len());
    for (index, i) in interests.iter().enumerate() {
        for (wanted, filter) in [(i.read, libc::EVFILT_READ), (i.write, libc::EVFILT_WRITE)] {
            if wanted {
                changes.push(libc::kevent {
                    ident: i.fd as libc::uintptr_t,
                    filter,
                    flags: libc::EV_ADD,
                    fflags: 0,
                    data: 0,
                    udata: index as *mut libc::c_void,
                });
            }
        }
    }
    // Room for every change: a registration that fails comes back as an EV_ERROR event
    // instead of ending the call (kevent(2)).
    // SAFETY: an all-zero kevent is a valid value.
    let mut events = vec![unsafe { std::mem::zeroed::<libc::kevent>() }; changes.len().max(1)];
    let ts = timeout.map(|t| libc::timespec {
        tv_sec: libc::time_t::try_from(t.as_secs()).unwrap_or(libc::time_t::MAX),
        tv_nsec: libc::c_long::from(t.subsec_nanos()),
    });
    let ts_ptr = ts
        .as_ref()
        .map_or(std::ptr::null(), |t| t as *const libc::timespec);
    // SAFETY: both arrays are valid for their lengths; `ts_ptr` is null or a timespec.
    let n = unsafe {
        libc::kevent(
            kq,
            changes.as_ptr(),
            changes.len() as libc::c_int,
            events.as_mut_ptr(),
            events.len() as libc::c_int,
            ts_ptr,
        )
    };
    let Ok(n) = usize::try_from(n) else {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted {
            Ok(())
        } else {
            Err(e)
        };
    };
    for e in events.iter().take(n) {
        let Some(i) = interests.get(e.udata as usize) else {
            continue;
        };
        // A failed registration surfaces through the next read or write.
        let failed = e.flags & libc::EV_ERROR != 0;
        let read = i.read && (e.filter == libc::EVFILT_READ || failed);
        let write = i.write && (e.filter == libc::EVFILT_WRITE || failed);
        if read || write {
            out.push(Ready {
                token: i.token,
                read,
                write,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::Write as _;
    use std::net::Shutdown;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    use super::*;

    fn ready(i: Interest<u8>) -> Vec<Ready<u8>> {
        let mut out = Vec::new();
        wait(&[i], Some(Duration::from_millis(50)), &mut out).unwrap();
        out
    }

    #[test]
    fn a_half_closed_peer_is_readable_but_not_a_reason_to_wake_a_writer() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        // Fill our send buffer so the socket is not writable.
        let chunk = [0u8; 4096];
        while (&ours).write(&chunk).is_ok() {}
        theirs.shutdown(Shutdown::Write).unwrap();
        let fd = ours.as_raw_fd();
        let r = ready(Interest {
            fd,
            read: true,
            write: false,
            token: 1,
        });
        assert!(r.first().is_some_and(|r| r.read), "EOF is readable");
        let w = ready(Interest {
            fd,
            read: false,
            write: true,
            token: 2,
        });
        assert!(w.is_empty(), "a writer woke for a half-close: {w:?}");
    }
}
