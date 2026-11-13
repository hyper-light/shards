//! A lane to a socket of a run's own command (D251): a byte stream its controller opens on the
//! agent-control endpoint (D238), carried to the microVM over vsock and connected by
//! shards-init to an AF_UNIX stream socket the command listens on, as OpenSSH connects a
//! channel to a socket by its path (`direct-streamlocal@openssh.com`, PROTOCOL §2.4).
//!
//! - **Its grant.** The run names one directory of sockets its controller may reach
//!   ([`DIR`]), as the command sees it, in the command's own root; a lane names one socket in
//!   it by its file name ([`name_ok`]). A run that names none opens no lane (default deny),
//!   and no member's socket is ever one.
//! - **Its ask.** The host asks on the signal connection ([`kind::SOCKET`]): a token, the
//!   lane's id and its name ([`Ask`]). What init refuses before it connects anything (no
//!   grant, a command not running) it says on the workload's connection
//!   ([`kind::SOCKET_REFUSED`]), as [`kind::EXEC_FAILED`] says an exec's.
//! - **Its connection.** Otherwise init dials the exec port, says the token in its HELLO, then
//!   the lane's status: 0, connected, or the errno its connect failed with ([`STATUS`]); once
//!   connected, the bytes each way, and each way's end as that way's half-close.
//!
//! A lane's failures are Linux's errnos, typed as the controller reads them
//! (`Fault::Unreached`): init's own refusals are said as the errno that names each, EACCES
//! for a run that grants no sockets, EINVAL for a name that is no file name in its directory,
//! ENAMETOOLONG for one no socket can be bound at, ESRCH for a command not running, and its
//! connect's own errno otherwise.
//!
//! [`kind::SOCKET`]: super::kind::SOCKET
//! [`kind::SOCKET_REFUSED`]: super::kind::SOCKET_REFUSED
//! [`kind::EXEC_FAILED`]: super::kind::EXEC_FAILED

use alloc::vec::Vec;

use super::TOKEN;
use super::members::{NAME_MOST, PATH_MOST, path_ok as relative_ok};

/// `sockets=<dir>`: the one directory of sockets of the run's own command its controller may
/// reach, as the command sees it, in its root.
pub const DIR: &[u8] = b"sockets=";

/// `sockaddr_un.sun_path`'s bytes on Linux, its NUL among them (unix(7); include/uapi/linux/un.h
/// `UNIX_PATH_MAX`): the longest path a socket can be bound at, and so listened on.
pub const SUN_PATH: usize = 108;

/// Linux's errnos a lane is refused with by init itself (include/uapi/asm-generic/errno-base.h,
/// the numbers every guest architecture shares).
pub mod errno {
    /// The run's own command is not running: not yet started, or ended.
    pub const ESRCH: u32 = 3;
    /// The lane's host could not carry what its guest connected (its descriptor, or a word
    /// it could read).
    pub const EIO: u32 = 5;
    /// The lane's relay could not have its buffers.
    pub const ENOMEM: u32 = 12;
    /// The run grants no sockets.
    pub const EACCES: u32 = 13;
    /// The name is no file name in the run's directory.
    pub const EINVAL: u32 = 22;
    /// The socket's path in the run's directory is past [`SUN_PATH`](super::SUN_PATH): no
    /// socket can be bound there.
    pub const ENAMETOOLONG: u32 = 36;
}

/// Whether `dir` may be a run's socket directory: absolute, each component past `/` nonempty
/// and neither `.` nor `..`, no NUL, within Linux's `PATH_MAX` (include/uapi/linux/limits.h),
/// as a member's working directory is.
pub fn dir_ok(dir: &[u8]) -> bool {
    match dir.strip_prefix(b"/") {
        Some(b"") => true,
        Some(rest) => dir.len() < PATH_MOST && relative_ok(rest),
        None => false,
    }
}

/// Whether `name` names one file in a directory: nonempty, neither `.` nor `..`, no `/` or NUL,
/// within Linux's `NAME_MAX`.
pub fn name_ok(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MOST
        && name != b"."
        && name != b".."
        && !name.contains(&b'/')
        && !name.contains(&0)
}

/// The path of socket `name` in `dir`, where a socket could be bound there: within
/// [`SUN_PATH`] with its NUL. None for a name that is no file name, or a path past it.
pub fn path(dir: &[u8], name: &[u8]) -> Option<Vec<u8>> {
    if !dir_ok(dir) || !name_ok(name) {
        return None;
    }
    let joined = match dir {
        b"/" => [dir, name].concat(),
        _ => [dir, b"/", name].concat(),
    };
    (joined.len() < SUN_PATH).then_some(joined)
}

/// Why a grant's entries are refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A directory that is not absolute and plain within `PATH_MAX` ([`dir_ok`]).
    Dir,
    /// A second directory: a run has one.
    Twice,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Error::Dir => "a socket directory that is not absolute, plain and within PATH_MAX",
            Error::Twice => "a second socket directory",
        })
    }
}

/// The setup entry granting the sockets in `dir`.
pub fn grant(dir: &[u8]) -> Result<Vec<u8>, Error> {
    if !dir_ok(dir) {
        return Err(Error::Dir);
    }
    Ok([DIR, dir].concat())
}

/// The directory `setup` grants, checked; none where it grants none.
pub fn granted(setup: &[Vec<u8>]) -> Result<Option<&[u8]>, Error> {
    let mut dir = None;
    for d in setup.iter().filter_map(|e| e.strip_prefix(DIR)) {
        if !dir_ok(d) {
            return Err(Error::Dir);
        }
        if dir.replace(d).is_some() {
            return Err(Error::Twice);
        }
    }
    Ok(dir)
}

/// The host's ask for a lane, [`kind::SOCKET`](super::kind::SOCKET)'s payload: the token its
/// connection's HELLO will say, the id a refusal names it by, then its socket's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ask<'a> {
    pub token: [u8; TOKEN],
    pub id: u32,
    pub name: &'a [u8],
}

impl<'a> Ask<'a> {
    pub fn encode(&self) -> Vec<u8> {
        [&self.token[..], &self.id.to_be_bytes(), self.name].concat()
    }

    /// The ask `payload` carries; none where it is too short to name a lane. Its name is
    /// init's to check, and refuse by the lane's id.
    pub fn decode(payload: &'a [u8]) -> Option<Ask<'a>> {
        let (token, rest) = payload.split_first_chunk::<TOKEN>()?;
        let (id, name) = rest.split_first_chunk::<4>()?;
        Some(Ask {
            token: *token,
            id: u32::from_be_bytes(*id),
            name,
        })
    }
}

/// The bytes of a lane's status after its HELLO: a big-endian u32, 0 for connected, else the
/// errno it was refused with.
pub const STATUS: usize = 4;

/// [`kind::SOCKET_REFUSED`](super::kind::SOCKET_REFUSED)'s payload: the lane's id, then the
/// errno it was refused with.
pub fn refused(id: u32, errno: u32) -> [u8; 8] {
    let mut bytes = [0u8; 8];
    let (head, tail) = bytes.split_at_mut(4);
    head.copy_from_slice(&id.to_be_bytes());
    tail.copy_from_slice(&errno.to_be_bytes());
    bytes
}

/// The lane and errno `payload` names; none where it is not exactly one, or says no errno.
pub fn read_refused(payload: &[u8]) -> Option<(u32, core::num::NonZeroU32)> {
    let (id, errno) = payload.split_first_chunk::<4>()?;
    let errno = <[u8; 4]>::try_from(errno).ok()?;
    Some((
        u32::from_be_bytes(*id),
        core::num::NonZeroU32::new(u32::from_be_bytes(errno))?,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A run's directory is absolute and plain, as a member's working directory is.
    #[test]
    fn a_runs_socket_directory_is_absolute_and_plain() {
        for ok in [&b"/run/hex/channel"[..], b"/", b"/a"] {
            assert!(dir_ok(ok), "{ok:?}");
            assert_eq!(grant(ok).unwrap(), [DIR, ok].concat());
        }
        let past = [b"/".as_slice(), &[b'a'; PATH_MOST]].concat();
        for bad in [
            &b""[..],
            b"run/hex",
            b"//x",
            b"/a//b",
            b"/a/",
            b"/./a",
            b"/a/..",
            b"/run/hex/../etc",
            b"/a\0b",
            &past,
        ] {
            assert!(!dir_ok(bad), "{bad:?}");
            assert_eq!(grant(bad), Err(Error::Dir));
        }
    }

    /// A lane's name is one file name, and its path one a socket could be bound at.
    #[test]
    fn a_lanes_name_is_one_file_name_within_sun_path() {
        assert_eq!(
            path(b"/run/hex/channel", b"control.sock").unwrap(),
            b"/run/hex/channel/control.sock"
        );
        assert_eq!(path(b"/", b"s").unwrap(), b"/s");
        let long = [b'x'; 256];
        for bad in [
            &b""[..],
            b".",
            b"..",
            b"a/b",
            b"../control.sock",
            b"/x",
            b"nul\0x",
            &long,
        ] {
            assert!(!name_ok(bad), "{bad:?}");
            assert_eq!(path(b"/run", bad), None, "{bad:?}");
        }
        assert!(name_ok(&long[..NAME_MOST]));
        // Its NUL counted: 107 bytes of path fit, 108 do not.
        let dir = [b"/".as_slice(), &[b'd'; 100]].concat();
        assert!(path(&dir, b"abcde").is_some_and(|p| p.len() == SUN_PATH - 1));
        assert_eq!(path(&dir, b"abcdef"), None);
        assert_eq!(path(b"run", b"s"), None);
    }

    /// A run's grant, read from its entries as the host writes them: one directory at most;
    /// anything else left to the rest of the run.
    #[test]
    fn a_runs_grant_is_read_as_written() {
        let setup = [
            b"readonly".to_vec(),
            grant(b"/run/hex/channel").unwrap(),
            b"nesting".to_vec(),
        ];
        assert_eq!(granted(&setup), Ok(Some(&b"/run/hex/channel"[..])));
        assert_eq!(granted(&[b"readonly".to_vec()]), Ok(None));
        assert_eq!(granted(&[b"sockets=x".to_vec()]), Err(Error::Dir));
        assert_eq!(
            granted(&[grant(b"/a").unwrap(), grant(b"/b").unwrap()]),
            Err(Error::Twice)
        );
    }

    /// The ask reads as written; one too short to name a lane is none.
    #[test]
    fn an_ask_reads_as_written() {
        let ask = Ask {
            token: [7; TOKEN],
            id: 0x0102_0304,
            name: b"files.sock",
        };
        let bytes = ask.encode();
        assert_eq!(&bytes[..TOKEN], &[7; TOKEN]);
        assert_eq!(&bytes[TOKEN..TOKEN + 4], &[1, 2, 3, 4]);
        assert_eq!(Ask::decode(&bytes), Some(ask));
        assert_eq!(Ask::decode(&bytes[..TOKEN + 3]), None);
        assert_eq!(Ask::decode(&bytes[..TOKEN + 4]).unwrap().name, b"");
    }

    /// A refusal reads as written; one of no errno, or not exactly one, is none.
    #[test]
    fn a_refusal_reads_as_written() {
        let r = refused(9, errno::EACCES);
        assert_eq!(
            read_refused(&r),
            Some((9, core::num::NonZeroU32::new(errno::EACCES).unwrap()))
        );
        assert_eq!(read_refused(&refused(9, 0)), None);
        assert_eq!(read_refused(&r[..7]), None);
        assert_eq!(read_refused(&[&r[..], &[0]].concat()), None);
    }
}
