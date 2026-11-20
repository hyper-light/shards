//! A served share's resync channel (D288): how a share another server serves outlives that
//! server's death, as slates' daemon offers it (slates §4.6, A-316 and A-339, its
//! `slates_bridge_fuse::shards_resync`, whose parse is this one's reference). A share's stream
//! is read whole by length, so one its server died partway through is never asked on again;
//! a successor instead hands the device a fresh stream over a second channel made beside the
//! stream, and the device asks its outstanding request again on it.
//!
//! The channel is a pair of Unix sockets that carry descriptors (sequenced packets on Linux,
//! a stream on macOS), one way, the server's to the device's, and every message on it is
//! [`MESSAGE_BYTES`] long, little-endian: kind, version, two zero bytes, the attachment
//! (`u64`), its token (16 bytes). First comes `Attached`, with no descriptor, which tells the
//! device the token; each `Resync` after it carries one descriptor, the device's end of a
//! fresh stream. A device takes a `Resync` only where it parses, is the version it knows,
//! carries exactly one descriptor and names the attachment and token `Attached` named, the
//! token compared in time independent of where it differs; anything else, or the channel's
//! end, ends the share as a share with no channel ends, typed ([`Ended`]). Windows', which
//! names a handle rather than carrying one, is not taken here.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

/// Format: the bytes of each message, slates' `RESYNC_MESSAGE_BYTES`: kind (1), version (1),
/// reserved (2), attachment (8), token (16).
pub const MESSAGE_BYTES: usize = 28;
/// Format: the version of the messages this device reads, slates' `RESYNC_VERSION`.
const VERSION: u8 = 1;
/// Format: slates' kind bytes, `Attached` and `Resync`.
const ATTACHED: u8 = 1;
const RESYNC: u8 = 2;
/// Format: the bytes of an attachment's token, slates' `TOKEN_BYTES`.
const TOKEN_BYTES: usize = 16;

/// Why a share's channel gives it no fresh stream: its share ends, as one with no channel.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// The server's end closed: no resync will ever come.
    Closed,
    /// The channel failed, in the OS's words.
    Transport(io::ErrorKind),
    /// A message that is not one of this version's.
    Malformed,
    /// A message of a kind not expected where it came: anything but `Attached` first, or
    /// anything but `Resync` after.
    Unexpected { kind: u8 },
    /// A `Resync` with no descriptor, or more than one.
    Descriptors { given: usize },
    /// A `Resync` for another attachment, or with another token.
    Foreign,
}

impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ended::Closed => f.write_str("its server's resync channel closed"),
            Ended::Transport(kind) => write!(f, "its resync channel: {kind}"),
            Ended::Malformed => f.write_str("a malformed resync message"),
            Ended::Unexpected { kind } => write!(f, "a resync message of kind {kind} out of turn"),
            Ended::Descriptors { given } => write!(f, "a resync carrying {given} descriptors"),
            Ended::Foreign => f.write_str("a resync for another attachment"),
        }
    }
}

/// One message: its kind, attachment and token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Message {
    kind: u8,
    attachment: u64,
    token: [u8; TOKEN_BYTES],
}

/// `bytes` as a message, checked as slates' parse checks one: version, reserved bytes, then
/// a kind it defines.
fn parse(bytes: &[u8; MESSAGE_BYTES]) -> Result<Message, Ended> {
    let [kind, version, r0, r1, rest @ ..] = *bytes;
    if version != VERSION || r0 != 0 || r1 != 0 || !matches!(kind, ATTACHED | RESYNC) {
        return Err(Ended::Malformed);
    }
    let (attachment, token) = rest.split_at(8);
    let attachment = u64::from_le_bytes(attachment.try_into().map_err(|_| Ended::Malformed)?);
    let token = token.try_into().map_err(|_| Ended::Malformed)?;
    Ok(Message {
        kind,
        attachment,
        token,
    })
}

/// Whether `a` and `b` are the same token, in time independent of where they differ.
fn same_token(a: &[u8; TOKEN_BYTES], b: &[u8; TOKEN_BYTES]) -> bool {
    a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// A share's resync channel, its `Attached` taken.
#[derive(Debug)]
pub struct Resync {
    channel: UnixStream,
    attachment: u64,
    token: [u8; TOKEN_BYTES],
}

impl Resync {
    /// The channel `channel`, its first message read: the `Attached` its server queued
    /// before handing the channel on.
    pub fn attached(channel: UnixStream) -> Result<Resync, Ended> {
        let (m, fds) = next(&channel)?;
        if m.kind != ATTACHED {
            return Err(Ended::Unexpected { kind: m.kind });
        }
        if !fds.is_empty() {
            return Err(Ended::Descriptors { given: fds.len() });
        }
        Ok(Resync {
            channel,
            attachment: m.attachment,
            token: m.token,
        })
    }

    /// The fresh stream the next `Resync` hands over, waiting for it: what a share whose
    /// stream failed asks, its server's successor sending it once it has kept the
    /// attachment.
    pub fn fresh(&self) -> Result<OwnedFd, Ended> {
        let (m, fds) = next(&self.channel)?;
        if m.kind != RESYNC {
            return Err(Ended::Unexpected { kind: m.kind });
        }
        if m.attachment != self.attachment || !same_token(&m.token, &self.token) {
            return Err(Ended::Foreign);
        }
        let given = fds.len();
        match <[OwnedFd; 1]>::try_from(fds) {
            Ok([fd]) => Ok(fd),
            Err(_) => Err(Ended::Descriptors { given }),
        }
    }
}

/// The channel's next message and the descriptors it carried.
fn next(channel: &UnixStream) -> Result<(Message, Vec<OwnedFd>), Ended> {
    let mut bytes = [0u8; MESSAGE_BYTES];
    match shards_ipc::recv_exact(channel, &mut bytes) {
        Ok(Some(fds)) => Ok((parse(&bytes)?, fds)),
        Ok(None) => Err(Ended::Closed),
        Err(e) => Err(Ended::Transport(e.kind())),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A message's bytes, as slates' `ResyncMessage::to_bytes` makes them.
    pub(crate) fn message(kind: u8, attachment: u64, token: [u8; TOKEN_BYTES]) -> [u8; MESSAGE_BYTES] {
        let mut b = [0u8; MESSAGE_BYTES];
        b[0] = kind;
        b[1] = VERSION;
        b[4..12].copy_from_slice(&attachment.to_le_bytes());
        b[12..].copy_from_slice(&token);
        b
    }

    pub(crate) fn attached(attachment: u64, token: [u8; TOKEN_BYTES]) -> [u8; MESSAGE_BYTES] {
        message(ATTACHED, attachment, token)
    }

    pub(crate) fn resync(attachment: u64, token: [u8; TOKEN_BYTES]) -> [u8; MESSAGE_BYTES] {
        message(RESYNC, attachment, token)
    }

    /// The server's end and the device's, as slates makes them: sequenced packets on Linux,
    /// a stream on macOS.
    pub(crate) fn channel() -> (UnixStream, UnixStream) {
        #[cfg(target_os = "linux")]
        {
            let mut fds = [0; 2];
            // SAFETY: socketpair(2) into a two-int array; each end is owned once, below.
            let r = unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            };
            assert_eq!(r, 0, "socketpair");
            use std::os::fd::FromRawFd as _;
            // SAFETY: the two fresh descriptors socketpair(2) made, each owned once.
            unsafe { (UnixStream::from_raw_fd(fds[0]), UnixStream::from_raw_fd(fds[1])) }
        }
        #[cfg(not(target_os = "linux"))]
        UnixStream::pair().unwrap()
    }

    /// Sends `bytes` on the server's end, with `fds`.
    pub(crate) fn send(server: &UnixStream, bytes: &[u8], fds: &[std::os::fd::BorrowedFd<'_>]) {
        use std::os::fd::AsRawFd as _;
        let raw: Vec<i32> = fds.iter().map(|f| f.as_raw_fd()).collect();
        let space = if raw.is_empty() {
            0
        } else {
            // SAFETY: CMSG_SPACE computes a size alone.
            (unsafe { libc::CMSG_SPACE((raw.len() * 4) as u32) }) as usize
        };
        let mut control = vec![0u8; space];
        let mut iov = libc::iovec {
            iov_base: bytes.as_ptr() as *mut _,
            iov_len: bytes.len(),
        };
        // SAFETY: a zeroed msghdr, then filled with the live buffers above.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if !raw.is_empty() {
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = space as _;
            // SAFETY: the control buffer holds one header of `raw`'s descriptors.
            unsafe {
                let c = libc::CMSG_FIRSTHDR(&msg);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN((raw.len() * 4) as u32) as _;
                std::ptr::copy_nonoverlapping(raw.as_ptr(), libc::CMSG_DATA(c).cast::<i32>(), raw.len());
            }
        }
        // SAFETY: sendmsg(2) of the message built above.
        let n = unsafe { libc::sendmsg(server.as_raw_fd(), &msg, 0) };
        assert_eq!(n, bytes.len() as isize, "sendmsg");
    }

    const TOKEN: [u8; TOKEN_BYTES] = [7; TOKEN_BYTES];

    #[test]
    fn a_resync_naming_the_attachment_and_its_token_hands_over_its_stream() {
        let (server, device) = channel();
        send(&server, &attached(9, TOKEN), &[]);
        let r = Resync::attached(device).unwrap();
        let (fresh, _peer) = UnixStream::pair().unwrap();
        use std::os::fd::AsFd as _;
        send(&server, &resync(9, TOKEN), &[fresh.as_fd()]);
        let got = r.fresh().unwrap();
        assert!(same_inode(&got, &fresh), "the stream the successor handed over");
    }

    #[test]
    fn anything_but_a_resync_of_its_own_ends_the_share() {
        use std::os::fd::AsFd as _;
        let (a, _pa) = UnixStream::pair().unwrap();
        let (b, _pb) = UnixStream::pair().unwrap();
        let mut other_token = TOKEN;
        other_token[15] ^= 1;
        let mut bad_version = resync(9, TOKEN);
        bad_version[1] = 2;
        let cases: Vec<(&str, [u8; MESSAGE_BYTES], Vec<std::os::fd::BorrowedFd<'_>>, Ended)> = vec![
            (
                "another attachment",
                resync(8, TOKEN),
                vec![a.as_fd()],
                Ended::Foreign,
            ),
            (
                "another token",
                resync(9, other_token),
                vec![a.as_fd()],
                Ended::Foreign,
            ),
            (
                "no descriptor",
                resync(9, TOKEN),
                vec![],
                Ended::Descriptors { given: 0 },
            ),
            (
                "two descriptors",
                resync(9, TOKEN),
                vec![a.as_fd(), b.as_fd()],
                Ended::Descriptors { given: 2 },
            ),
            ("another version", bad_version, vec![a.as_fd()], Ended::Malformed),
            (
                "an Attached again",
                attached(9, TOKEN),
                vec![],
                Ended::Unexpected { kind: ATTACHED },
            ),
        ];
        for (what, bytes, fds, want) in cases {
            let (server, device) = channel();
            send(&server, &attached(9, TOKEN), &[]);
            let r = Resync::attached(device).unwrap();
            send(&server, &bytes, &fds);
            assert_eq!(r.fresh().unwrap_err(), want, "{what}");
        }
        let (server, device) = channel();
        send(&server, &attached(9, TOKEN), &[]);
        let r = Resync::attached(device).unwrap();
        drop(server);
        assert_eq!(r.fresh().unwrap_err(), Ended::Closed, "its server's end closed");
    }

    #[test]
    fn a_channel_must_say_attached_first() {
        let (server, device) = channel();
        send(&server, &resync(9, TOKEN), &[]);
        assert_eq!(
            Resync::attached(device).unwrap_err(),
            Ended::Unexpected { kind: RESYNC }
        );
    }

    fn same_inode(a: &OwnedFd, b: &impl std::os::fd::AsFd) -> bool {
        use std::os::fd::AsRawFd as _;
        // SAFETY: fstat(2) of two live descriptors into zeroed stat buffers.
        unsafe {
            let (mut x, mut y): (libc::stat, libc::stat) = (std::mem::zeroed(), std::mem::zeroed());
            libc::fstat(a.as_raw_fd(), &mut x) == 0
                && libc::fstat(b.as_fd().as_raw_fd(), &mut y) == 0
                && x.st_ino == y.st_ino
                && x.st_dev == y.st_dev
        }
    }
}
