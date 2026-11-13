//! Lanes between the host and sockets inside the microVM (D251), carried over vsock through
//! the exec port: each one's connection says its token in its HELLO, then its status, then
//! carries the bytes each way, each way's end that way's half-close (`native_relay::pump`,
//! `EndPolicy::Independent`).
//!
//! A lane to a socket of the run's own command is asked by the host ([`open`]): its name must
//! be a file name in the one directory the run grants (`sockets=`, default deny), and its path
//! is resolved inside the command's own root (openat2 `RESOLVE_IN_ROOT`, no magic links,
//! [`crate::inroot`]) as the command sees it, so that nothing the command planted leads
//! elsewhere; it is connected through its descriptor (`/proc/self/fd/N`), so that it is not
//! resolved again. Its status is 0, connected, or the errno it was refused with.
//!
//! Each lane runs on a thread of its own, as init's member ports do: a connect that waits on
//! a full backlog holds that lane alone, never init's loop.

use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

use shards_abi::run::socket::{self, Ask, errno};
use shards_abi::run::{self, kind};

use crate::engine::native_relay;

/// The one directory of sockets of the run's own command its controller may reach
/// (`sockets=`); none, and no lane opens.
#[derive(Debug, Default)]
pub struct Granted(Option<Vec<u8>>);

impl Granted {
    /// The grant `setup` carries, refused where it is malformed or twice: a run whose own
    /// command `runs` may be granted sockets, and an orchestrator's run of its image's members
    /// alone (`ATTEND`), whose command is init's, none.
    pub fn read(setup: &[Vec<u8>], runs: bool) -> Result<Granted, String> {
        let dir = socket::granted(setup).map_err(|e| e.to_string())?;
        if !runs && dir.is_some() {
            return Err("sockets granted in a run without a command of its own".into());
        }
        Ok(Granted(dir.map(<[u8]>::to_vec)))
    }
}

/// Opens the lane `payload` asks for ([`kind::SOCKET`]) to its socket in the root of
/// `command`, the run's own command, on a thread of its own; `command` none where it is not
/// running. What it refuses at once: the lane's id and the errno, for the host to hear on the
/// workload's connection ([`refused`]); none where the frame names no lane.
pub fn open(
    payload: &[u8],
    granted: &Granted,
    command: Option<libc::pid_t>,
) -> Result<(), Option<(u32, u32)>> {
    let ask = Ask::decode(payload).ok_or(None)?;
    let refuse = |errno| Err(Some((ask.id, errno)));
    let Some(dir) = &granted.0 else {
        return refuse(errno::EACCES);
    };
    if !socket::name_ok(ask.name) {
        return refuse(errno::EINVAL);
    }
    let Some(path) = socket::path(dir, ask.name) else {
        return refuse(errno::ENAMETOOLONG);
    };
    let Some(pid) = command else {
        return refuse(errno::ESRCH);
    };
    let token = ask.token;
    std::thread::Builder::new()
        .name("lane".into())
        .spawn(move || {
            let local = connect(Path::new(&format!("/proc/{pid}/root")), &path);
            carry(local, &token);
        })
        .map(drop)
        .map_err(|e| Some((ask.id, errno_of(&e))))
}

/// The run's [`kind::SOCKET_REFUSED`] frame for lane `id`, refused with `errno`.
pub fn refused(id: u32, errno: u32) -> Vec<u8> {
    let payload = socket::refused(id, errno);
    [
        &run::header(kind::SOCKET_REFUSED, payload.len() as u32)[..],
        &payload,
    ]
    .concat()
}

/// Linux's errno for `e`: its own, or EIO for an error that names none.
fn errno_of(e: &io::Error) -> u32 {
    e.raw_os_error()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|&n| n != 0)
        .unwrap_or(libc::EIO as u32)
}

/// The socket at `path` in the root at `root`, connected: resolved inside it, and connected
/// through its descriptor. A root that is no longer there is a command that has ended (ESRCH).
fn connect(root: &Path, path: &[u8]) -> Result<UnixStream, u32> {
    let root = crate::inroot::Root::open(root).map_err(|_| errno::ESRCH)?;
    let at = root.open_at(path, libc::O_PATH).map_err(|e| errno_of(&e))?;
    UnixStream::connect(crate::inroot::path(&at)).map_err(|e| errno_of(&e))
}

/// Carries `local` to the host: dials the exec port, says `token` in its HELLO and the lane's
/// status after it, then, connected, the bytes each way until both ways have ended. A lane
/// whose relay's buffers cannot be had says ENOMEM. Only a fault is said, on init's stderr; a
/// host that cannot be dialed is the run's end, whose host waits for no lane then.
fn carry(local: Result<UnixStream, u32>, token: &[u8; run::TOKEN]) {
    let slots = native_relay::Slots::vsock_packet();
    let status = match (&local, &slots) {
        (Err(errno), _) => *errno,
        (Ok(_), Err(_)) => errno::ENOMEM,
        (Ok(_), Ok(_)) => 0,
    };
    let host = match crate::run::dial(run::EXEC_PORT, true) {
        Ok(host) => host,
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards-init: a lane's connection to its host: {e}");
            return;
        }
    };
    let said = [
        &run::header(kind::HELLO, run::TOKEN as u32)[..],
        token,
        &status.to_be_bytes(),
    ]
    .concat();
    if (&host).write_all(&said).is_err() {
        return;
    }
    let (Ok(local), Ok(slots)) = (local, slots) else {
        return;
    };
    let mut report = native_relay::Report::new();
    if let Err(fault) = native_relay::pump(
        [local.as_fd(), host.as_fd()],
        slots,
        [native_relay::ByteLimit::PeerEnd; 2],
        native_relay::Control::ProcessOwned,
        native_relay::EndPolicy::Independent,
        &mut report,
    ) {
        let _ = writeln!(io::stderr(), "shards-init: a lane: {fault:?}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;

    /// The grant is the run's own command's alone, one directory.
    #[test]
    fn a_grant_is_a_running_commands_alone() {
        let setup = vec![socket::grant(b"/run/hex/channel").unwrap()];
        assert_eq!(
            Granted::read(&setup, true).unwrap().0.as_deref(),
            Some(&b"/run/hex/channel"[..])
        );
        assert!(Granted::read(&setup, false).is_err());
        assert!(Granted::read(&[], false).unwrap().0.is_none());
        assert!(Granted::read(&[b"sockets=rel".to_vec()], true).is_err());
    }

    /// What init refuses before connecting anything, by the lane's id and the errno that names
    /// it: a frame that names no lane is dropped.
    #[test]
    fn a_lane_is_refused_at_once_where_it_may_not_be() {
        let granted = Granted::read(&[socket::grant(b"/run/hex/channel").unwrap()], true).unwrap();
        let ask = |name: &[u8]| {
            Ask {
                token: [1; run::TOKEN],
                id: 7,
                name,
            }
            .encode()
        };
        assert_eq!(open(b"short", &granted, Some(1)), Err(None));
        assert_eq!(
            open(&ask(b"s.sock"), &Granted::default(), Some(1)),
            Err(Some((7, errno::EACCES)))
        );
        for bad in [&b""[..], b".", b"..", b"a/b", b"nul\0x"] {
            assert_eq!(
                open(&ask(bad), &granted, Some(1)),
                Err(Some((7, errno::EINVAL))),
                "{bad:?}"
            );
        }
        assert_eq!(
            open(&ask(&[b'x'; 100]), &granted, Some(1)),
            Err(Some((7, errno::ENAMETOOLONG)))
        );
        assert_eq!(
            open(&ask(b"s.sock"), &granted, None),
            Err(Some((7, errno::ESRCH)))
        );
        let frame = refused(7, errno::EACCES);
        let (which, len) = run::parse_header(frame[..run::HEADER].try_into().unwrap()).unwrap();
        assert_eq!(
            (which, len as usize),
            (kind::SOCKET_REFUSED, frame.len() - run::HEADER)
        );
        assert_eq!(
            socket::read_refused(&frame[run::HEADER..]).map(|(id, e)| (id, e.get())),
            Some((7, errno::EACCES))
        );
    }

    /// A socket in the root connects through its descriptor, however long the root's own path;
    /// a link out of the root, `..` past it, a missing socket, a file that is no socket, one
    /// nothing listens on, and a root that is gone are each refused by their errno.
    #[test]
    fn a_path_connects_inside_its_root_alone() {
        let dir = shards_testdir::TempDir::new("lanes").unwrap();
        // A root whose own path is past sun_path's 108 bytes, as /proc/PID/root's view is not.
        let root = dir.path().join("r".repeat(60)).join("o".repeat(60));
        std::fs::create_dir_all(root.join("run/hex/channel")).unwrap();
        let short = dir.path().join("l.sock");
        let listener = UnixListener::bind(&short).unwrap();
        std::fs::rename(&short, root.join("run/hex/channel/control.sock")).unwrap();
        assert!(root.join("run/hex/channel/control.sock").as_os_str().len() > socket::SUN_PATH);
        let mut conn = connect(&root, b"/run/hex/channel/control.sock").unwrap();
        let (mut accepted, _) = listener.accept().unwrap();
        conn.write_all(b"ping").unwrap();
        let mut got = [0u8; 4];
        accepted.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ping");

        // A link that leaves the root is resolved inside it: it finds nothing there.
        let outside = dir.path().join("o.sock");
        let _outside = UnixListener::bind(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("run/escape.sock")).unwrap();
        let enoent = libc::ENOENT as u32;
        assert_eq!(connect(&root, b"/run/escape.sock").map(drop), Err(enoent));
        // `..` past the root stays in it.
        assert_eq!(connect(&root, b"/run/../../../o.sock").map(drop), Err(enoent));
        assert_eq!(connect(&root, b"/run/none.sock").map(drop), Err(enoent));
        // A file that is no socket: connect(2)'s own answer (af_unix.c `unix_find_bsd`).
        std::fs::write(root.join("run/file"), b"").unwrap();
        assert_eq!(
            connect(&root, b"/run/file").map(drop),
            Err(libc::ECONNREFUSED as u32)
        );
        // A socket nothing listens on any more.
        let gone = dir.path().join("g.sock");
        drop(UnixListener::bind(&gone).unwrap());
        std::fs::rename(&gone, root.join("run/gone.sock")).unwrap();
        assert_eq!(
            connect(&root, b"/run/gone.sock").map(drop),
            Err(libc::ECONNREFUSED as u32)
        );
        assert_eq!(
            connect(&dir.path().join("no-root"), b"/run/s.sock").map(drop),
            Err(errno::ESRCH)
        );
    }
}
