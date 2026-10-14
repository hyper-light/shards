//! The journald driver's sending (D166): each entry (logging/journald.rs) a datagram to
//! the journal's socket, as go-systemd's journal.Send sends one; an entry too long for a
//! datagram in a sealed memfd passed with an empty one, as systemd's own sd_journal_sendv
//! passes it (sd-journal journal-send.c), which journald takes from any sender, where
//! go-systemd's is a file it makes in /dev/shm. The journal's socket is systemd's, or the
//! one `SHARDS_JOURNAL_SOCKET` names. A datagram on a local socket is in the journal's
//! queue as it is sent: delivered.

use std::io::{self, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;

use super::{Driver, Message, Wait};
use crate::logging::journald::{self, From};

pub(super) struct Journald {
    from: From,
    socket: PathBuf,
    conn: Option<UnixDatagram>,
    /// The entries sent: the next one's ordinal less one (moby's journald.ordinal).
    sent: u64,
}

impl Journald {
    pub(super) fn new(from: From) -> Journald {
        let socket = std::env::var_os("SHARDS_JOURNAL_SOCKET")
            .map_or_else(|| PathBuf::from(journald::SOCKET), PathBuf::from);
        Journald {
            from,
            socket,
            conn: None,
            sent: 0,
        }
    }

    fn write(&mut self, entry: &[u8]) -> io::Result<()> {
        if self.conn.is_none() {
            let s = UnixDatagram::unbound()?;
            s.connect(&self.socket)?;
            self.conn = Some(s);
        }
        let Some(conn) = &self.conn else {
            return Err(io::Error::from(io::ErrorKind::NotConnected));
        };
        let sent = match super::sockets::send_datagram(conn, entry) {
            Err(e) if super::sockets::too_large(&e) => send_sealed(conn, entry),
            sent => sent,
        };
        if sent.is_err() {
            self.conn = None;
        }
        sent
    }
}

impl Driver for Journald {
    fn send(&mut self, m: &Message<'_>, _wait: &Wait<'_>) -> io::Result<()> {
        let ordinal = self.sent.saturating_add(1);
        let entry = journald::entry(&self.from, m.stream, m.at, m.line, m.part, ordinal);
        self.write(&entry)?;
        self.sent = ordinal;
        Ok(())
    }
}

/// Sends `entry` to the journal in a memfd, sealed so that it can change no more, passed
/// with an empty datagram.
fn send_sealed(conn: &UnixDatagram, entry: &[u8]) -> io::Result<()> {
    // SAFETY: memfd_create(2) of a C string's name.
    let fd = unsafe {
        libc::memfd_create(
            c"shards-journal".as_ptr(),
            libc::MFD_ALLOW_SEALING | libc::MFD_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is new, and this function's alone.
    let memfd = unsafe { OwnedFd::from_raw_fd(fd) };
    std::fs::File::from(memfd.try_clone()?).write_all(entry)?;
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
    // SAFETY: fcntl(2) F_ADD_SEALS on a memfd this function holds.
    if unsafe { libc::fcntl(memfd.as_raw_fd(), libc::F_ADD_SEALS, seals) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let bytes = std::mem::size_of::<libc::c_int>();
    // SAFETY: CMSG_SPACE only computes a size.
    let space = unsafe { libc::CMSG_SPACE(bytes as u32) } as usize;
    let mut control = vec![0u64; space.div_ceil(8)];
    // SAFETY: an all-zero msghdr is valid; the control buffer set below outlives the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: the control buffer is aligned (u64s) and CMSG_SPACE(bytes) long, so the
    // first header and its descriptor fit.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("no room for the memfd"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(bytes as u32) as _;
        libc::CMSG_DATA(cmsg)
            .cast::<libc::c_int>()
            .write_unaligned(memfd.as_raw_fd());
    }
    loop {
        // SAFETY: sendmsg(2) of an initialized msghdr over live buffers, on a socket the
        // caller holds.
        if unsafe { libc::sendmsg(conn.as_raw_fd(), &msg, 0) } >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}
