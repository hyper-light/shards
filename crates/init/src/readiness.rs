//! A run's readiness under the nesting grant (D234), init its one owner: the workload's
//! `READY=1`, sd_notify's word, on a datagram socket at [`NOTIFY_SOCKET`]; its ready-by,
//! the caller's deadline moved onto the guest's CLOCK_BOOTTIME as the run arrives
//! ([`deadline`]); and its state, which they move by D234's table ([`step`]), each move
//! marked on the control page and told the host ([`kind::READY`]).
//!
//! A datagram is taken as systemd's manager takes one: past PIPE_BUF, the most it reads
//! (`NOTIFY_BUFFER_MAX`, src/basic/constants.h), ignored as truncated; from a process
//! outside the workload's cgroup, which the kernel names (SCM_CREDENTIALS) and /proc places,
//! ignored, as `NotifyAccess=all` hears only "all members of the service's control group"
//! (systemd.service(5)); its assignments other than `READY=1` ignored ("Unknown assignments
//! are ignored", sd_notify(3)). A sender gone before /proc places it is taken for one
//! outside, as systemd takes it: the workload's own process that says it is ready lives
//! on.
//!
//! [`kind::READY`]: shards_abi::run::kind::READY

use std::io::{self, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::net::UnixDatagram;

use shards_abi::run::nesting::Ready;

/// Where the workload's processes find its socket, which `NOTIFY_SOCKET` names
/// (sd_notify(3)): beside its microVM's engine's (D181).
pub const NOTIFY_SOCKET: &str = "/run/shards/notify";
/// The standby's setup entry that makes the socket: init's own, never a daemon's.
pub const NOTIFY: &[u8] = b"notify";
/// The tag of its descriptor on the standby's error channel.
pub const LISTENER: u8 = b'N';

/// The workload's environment of the grant: `NOTIFY_SOCKET`, and `SHARDS_READY_BY`, its
/// ready-by on its CLOCK_BOOTTIME in nanoseconds, where the run gives one: an instant
/// before the guest booted, which that clock cannot name, as 0, which is as passed.
pub fn environment(deadline: Option<i64>) -> Vec<Vec<u8>> {
    let mut env = vec![format!("NOTIFY_SOCKET={NOTIFY_SOCKET}").into_bytes()];
    if let Some(at) = deadline {
        env.push(format!("SHARDS_READY_BY={}", at.max(0)).into_bytes());
    }
    env
}

/// The run's ready-by on the guest's CLOCK_BOOTTIME, in nanoseconds (D234): `ready_by`,
/// the caller's, in nanoseconds since the Unix epoch on the host's clock, moved by what is
/// left of it as the host's clock says now (`host_now`, HOST_TIME) onto the guest's clock
/// now (`boot_now`), so that neither a template's restore nor a pool's wait counts against
/// it. Negative where it passed before the guest booted; `None` past what the clock holds.
pub fn deadline(ready_by: u64, host_now: u64, boot_now: i64) -> Option<i64> {
    let left = i128::from(ready_by).checked_sub(i128::from(host_now))?;
    i64::try_from(i128::from(boot_now).checked_add(left)?).ok()
}

/// [`deadline`] of `ready_by` as the run arrives: the host's clock read from the control
/// page, and the guest's CLOCK_BOOTTIME beside it.
pub fn arrived(ready_by: u64) -> Result<i64, String> {
    let host_now = crate::linux::control_read64(shards_abi::control::HOST_TIME)
        .map_err(|e| format!("reading the host's time for its ready-by: {e}"))?;
    if host_now == 0 {
        return Err("the host gave no time to read its ready-by by".into());
    }
    let boot_now = boot_now().map_err(|e| format!("reading the guest's clock for its ready-by: {e}"))?;
    deadline(ready_by, host_now, boot_now).ok_or_else(|| {
        format!("a ready-by of {ready_by} ns since the Unix epoch, past what the guest's clock holds")
    })
}

/// The guest's CLOCK_BOOTTIME, in nanoseconds: the clock the deadline is on, which counts
/// a suspended guest's time as the host's clock does.
fn boot_now() -> io::Result<i64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime(2) into a timespec of ours.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &raw mut ts) } != 0 {
        return Err(io::Error::last_os_error());
    }
    ts.tv_sec
        .checked_mul(1_000_000_000)
        .and_then(|ns| ns.checked_add(ts.tv_nsec))
        .ok_or_else(|| io::Error::other("CLOCK_BOOTTIME past what nanoseconds in an i64 hold"))
}

/// A timer on CLOCK_BOOTTIME that fires at `deadline`: at once where that has passed, the
/// earliest instant the clock names standing for one before the guest booted.
pub fn timer(deadline: i64) -> io::Result<OwnedFd> {
    // SAFETY: timerfd_create(2) returns a new descriptor, or -1.
    let fd = unsafe { libc::timerfd_create(libc::CLOCK_BOOTTIME, libc::TFD_NONBLOCK | libc::TFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let timer = unsafe { OwnedFd::from_raw_fd(fd) };
    // An it_value of zero disarms a timer (timerfd_settime(2)): one nanosecond is as past.
    let at = deadline.max(1);
    let when = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: at / 1_000_000_000,
            tv_nsec: at % 1_000_000_000,
        },
    };
    // SAFETY: timerfd_settime(2) of our descriptor, with a value of ours and no old value.
    if unsafe {
        libc::timerfd_settime(
            timer.as_raw_fd(),
            libc::TFD_TIMER_ABSTIME,
            &when,
            std::ptr::null_mut(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(timer)
}

/// Makes the workload's notify socket at [`NOTIFY_SOCKET`], in its mount namespace (a
/// standby's), and gives init its descriptor on `err`. Any process of the run may send to
/// it, whoever it runs as; SO_PASSCRED has the kernel name each datagram's sender, which
/// init holds to the workload's cgroup. What the image or a run before left at its path is
/// the run's to replace. The errno of what failed.
pub fn listen(err: RawFd) -> Result<(), i32> {
    use std::os::unix::fs::PermissionsExt as _;
    let errno = |e: io::Error| e.raw_os_error().unwrap_or(libc::EIO);
    let path = std::path::Path::new(NOTIFY_SOCKET);
    if let Some(dir) = path.parent() {
        own_dir(dir).map_err(errno)?;
    }
    match std::fs::remove_file(NOTIFY_SOCKET) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(errno(e)),
        _ => {}
    }
    let socket = UnixDatagram::bind(NOTIFY_SOCKET).map_err(errno)?;
    let on: libc::c_int = 1;
    // SAFETY: setsockopt(2) of an int option on our own socket.
    if unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PASSCRED,
            (&raw const on).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    } != 0
    {
        return Err(errno(io::Error::last_os_error()));
    }
    std::fs::set_permissions(NOTIFY_SOCKET, std::fs::Permissions::from_mode(0o666)).map_err(errno)?;
    if !crate::hostbind::send_fd(err, &[LISTENER], socket.as_raw_fd(), 0) {
        return Err(io::Error::last_os_error().raw_os_error().unwrap_or(libc::EPIPE));
    }
    Ok(())
}

/// `dir`, `/run/shards` for the grant's files, made where it is missing, as a run's engine
/// socket makes it (D181); refused where it is anything but a directory, a link among them,
/// which would put what init's standby writes there elsewhere.
pub fn own_dir(dir: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(dir)?;
    if !std::fs::symlink_metadata(dir)?.file_type().is_dir() {
        return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
    }
    Ok(())
}

/// Where a run's readiness stands (D234).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Its command started, with no `READY=1` yet, and its ready-by, where it has one,
    /// not yet passed.
    Starting,
    /// A process of the workload's said `READY=1`.
    Ready,
    /// Its ready-by passed with no `READY=1`.
    ReadyByPassed,
    /// The workload's main process ended: nothing moves it any more.
    Exited,
}

/// What moves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// `READY=1` from a process of the workload's, at this instant on CLOCK_BOOTTIME.
    Ready(i64),
    /// Its ready-by's timer fired, at this instant.
    Due(i64),
    /// The workload's main process ended.
    Exited,
}

/// D234's table: where `event` moves `state` in a run whose ready-by is `deadline`, and
/// what the host is told of it. A `READY=1` by the ready-by is on time, one past it late,
/// whether or not the timer was heard first; a second is no news; the end is terminal.
pub fn step(state: State, event: Event, deadline: Option<i64>) -> (State, Option<Ready>) {
    let past = |now: i64, at: i64| u64::try_from(now.saturating_sub(at)).unwrap_or(0);
    match (state, event) {
        (State::Exited, _) | (_, Event::Exited) => (State::Exited, None),
        (State::Starting, Event::Ready(now)) => match deadline {
            Some(at) if now > at => (State::Ready, Some(Ready::Late(past(now, at)))),
            _ => (State::Ready, Some(Ready::OnTime)),
        },
        (State::ReadyByPassed, Event::Ready(now)) => (
            State::Ready,
            Some(Ready::Late(deadline.map_or(0, |at| past(now, at)))),
        ),
        (State::Starting, Event::Due(now)) => match deadline {
            Some(at) => (State::ReadyByPassed, Some(Ready::Passed(past(now, at)))),
            None => (State::Starting, None),
        },
        (State::Ready, Event::Ready(_) | Event::Due(_)) | (State::ReadyByPassed, Event::Due(_)) => {
            (state, None)
        }
    }
}

/// Whether `text`, a datagram's whole bytes, says `READY=1`: a line of its own among its
/// newline-separated assignments (sd_notify(3)). One trailing NUL is taken, as a C caller
/// may send its string's; a NUL elsewhere has the datagram ignored, as systemd ignores it.
pub fn says_ready(text: &[u8]) -> bool {
    let text = text.strip_suffix(&[0]).unwrap_or(text);
    !text.contains(&0) && text.split(|&b| b == b'\n').any(|line| line == b"READY=1")
}

/// Whether `cgroup`, a process's `/proc/PID/cgroup` as init's cgroup namespace shows it,
/// puts it in `workload`, a cgroup as /proc names it, or beneath it.
pub fn within(cgroup: &str, workload: &str) -> bool {
    cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .and_then(|path| path.strip_prefix(workload))
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// One datagram off `fd` into `buf`: its whole length, past `buf`'s where it was truncated
/// (MSG_TRUNC), and its sender's PID in init's namespace, as the kernel gave it; `None`
/// where none waits. Its control buffer holds the credentials alone, so that descriptors a
/// sender passes are never installed in init (scm_detach_fds finds no room for them).
fn receive(fd: RawFd, buf: &mut [u8]) -> io::Result<Option<(usize, Option<libc::pid_t>)>> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    // Room for one control message of credentials, aligned as cmsghdr is: CMSG_SPACE of a
    // ucred, its 16-byte header and its 12 bytes padded to 16, on every 64-bit Linux.
    let mut control = [0u64; 4];
    // SAFETY: CMSG_SPACE of the credentials' size.
    let space = unsafe { libc::CMSG_SPACE(size_of::<libc::ucred>() as u32) } as usize;
    // SAFETY: zeroed is a valid msghdr.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space.min(size_of_val(&control)) as _;
    // SAFETY: recvmsg(2) into buffers of ours, of the lengths set.
    let n = unsafe {
        libc::recvmsg(
            fd,
            &raw mut msg,
            libc::MSG_DONTWAIT | libc::MSG_TRUNC | libc::MSG_CMSG_CLOEXEC,
        )
    };
    let Ok(n) = usize::try_from(n) else {
        let e = io::Error::last_os_error();
        return match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(None),
            _ => Err(e),
        };
    };
    let mut pid = None;
    // SAFETY: the control buffer recvmsg filled, walked by its own macros.
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&raw const msg) };
    while !cmsg.is_null() {
        // SAFETY: a header recvmsg wrote, within the control buffer.
        let (level, which, len) =
            unsafe { ((*cmsg).cmsg_level, (*cmsg).cmsg_type, (*cmsg).cmsg_len as usize) };
        // SAFETY: CMSG_LEN of the credentials' size, a constant.
        let whole = unsafe { libc::CMSG_LEN(size_of::<libc::ucred>() as u32) } as usize;
        if level == libc::SOL_SOCKET && which == libc::SCM_CREDENTIALS && len >= whole {
            // SAFETY: the message's data, which holds one ucred, read unaligned.
            let cred = unsafe { std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::ucred>()) };
            pid = (cred.pid > 0).then_some(cred.pid);
        }
        // SAFETY: the next header after one within the buffer, or null.
        cmsg = unsafe { libc::CMSG_NXTHDR(&raw const msg, cmsg) };
    }
    Ok(Some((n, pid)))
}

/// A granted run's readiness, init's to keep (D234): the workload's notify socket, its
/// ready-by and the timer that says it has passed, and where it stands.
pub struct Readiness {
    socket: Option<OwnedFd>,
    timer: Option<OwnedFd>,
    deadline: Option<i64>,
    state: State,
    /// The workload's cgroup as /proc names it, whose processes alone may say it is ready.
    workload: String,
    /// One datagram's bytes, the most a sender's is taken whole: PIPE_BUF.
    buf: Vec<u8>,
}

impl Readiness {
    /// Its readiness, starting: `socket` the standby's, `timer` armed at `deadline`, where
    /// the run gives one.
    pub fn new(socket: OwnedFd, timer: Option<OwnedFd>, deadline: Option<i64>, workload: &str) -> Readiness {
        Readiness {
            socket: Some(socket),
            timer,
            deadline,
            state: State::Starting,
            workload: workload.to_string(),
            buf: vec![0; libc::PIPE_BUF],
        }
    }

    /// Its socket, to poll for datagrams, until the workload ends.
    pub fn socket(&self) -> Option<RawFd> {
        self.socket.as_ref().map(|s| s.as_raw_fd())
    }

    /// Its timer, to poll, until it fires or the workload ends.
    pub fn timer(&self) -> Option<RawFd> {
        self.timer.as_ref().map(|t| t.as_raw_fd())
    }

    /// Moves it by `event`: what the host is told of the move.
    fn on(&mut self, event: Event) -> Option<Ready> {
        let (state, told) = step(self.state, event, self.deadline);
        self.state = state;
        if state == State::Exited {
            self.socket = None;
            self.timer = None;
        }
        told
    }

    /// Takes one datagram off its socket, read on while the workload runs so that no
    /// sender waits on a full queue: `READY=1` from a process of the workload's cgroup
    /// moves it, where it is not ready already. A socket that fails but for want of a
    /// datagram is let go of, which its senders then hear.
    pub fn datagram(&mut self) -> Option<Ready> {
        let fd = self.socket()?;
        let (len, pid) = match receive(fd, &mut self.buf) {
            Ok(Some(got)) => got,
            Ok(None) => return None,
            Err(e) => {
                let _ = writeln!(io::stderr(), "shards-init: its notify socket: {e}");
                self.socket = None;
                return None;
            }
        };
        // Past PIPE_BUF it was cut short: ignored whole.
        let text = self.buf.get(..len)?;
        if !matches!(self.state, State::Starting | State::ReadyByPassed) || !says_ready(text) {
            return None;
        }
        let sender = pid.and_then(|p| std::fs::read_to_string(format!("/proc/{p}/cgroup")).ok())?;
        if !within(&sender, &self.workload) {
            return None;
        }
        let now = boot_now().ok()?;
        self.on(Event::Ready(now))
    }

    /// Its timer fired: the ready-by passed, where nothing moved it before.
    pub fn due(&mut self) -> Option<Ready> {
        let fd = self.timer()?;
        let mut expirations = [0u8; 8];
        // SAFETY: read(2) of a timerfd's count into a buffer of its size.
        if unsafe { libc::read(fd, expirations.as_mut_ptr().cast(), expirations.len()) } < 0 {
            return None;
        }
        self.timer = None;
        let now = boot_now().ok()?;
        self.on(Event::Due(now))
    }

    /// The workload's main process ended: its socket and timer let go of.
    pub fn exited(&mut self) {
        self.on(Event::Exited);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;

    /// The caller's ready-by moved onto the guest's clock (D234): what is left of it on the
    /// host's clock, from the guest's now; past, before the guest booted even; none where
    /// the run gives none; refused past what the clock holds.
    #[test]
    fn the_ready_by_is_the_callers_on_the_guests_clock() {
        let host = 1_794_466_320_362_000_000u64;
        // Five seconds left, in a guest two seconds up (a restored template's clock).
        assert_eq!(deadline(host + 5 * S as u64, host, 2 * S), Some(7 * S));
        // Its host's time exactly: now.
        assert_eq!(deadline(host, host, 2 * S), Some(2 * S));
        // Past: three seconds ago, before the guest booted even.
        assert_eq!(deadline(host - 3 * S as u64, host, 2 * S), Some(-S));
        assert_eq!(deadline(0, host, 0), Some(-(host as i64)));
        // Past what i64 nanoseconds hold, either way.
        assert_eq!(deadline(u64::MAX, 0, 0), None);
        assert_eq!(deadline(u64::MAX, 0, i64::MAX), None);
        assert_eq!(deadline(0, u64::MAX, 0), None);
        assert_eq!(deadline(1, 0, i64::MAX), None);
        assert_eq!(deadline(0, 1, i64::MIN), None);
        // None given, none in its environment but its socket.
        assert_eq!(
            environment(None),
            vec![b"NOTIFY_SOCKET=/run/shards/notify".to_vec()]
        );
        assert_eq!(
            environment(Some(7 * S)),
            vec![
                b"NOTIFY_SOCKET=/run/shards/notify".to_vec(),
                b"SHARDS_READY_BY=7000000000".to_vec()
            ]
        );
        // An instant before the guest booted: 0, as passed.
        assert_eq!(environment(Some(-S))[1], b"SHARDS_READY_BY=0".to_vec());
    }

    /// Every transition of D234's table, each state against each event, and what the host
    /// hears: ready on time or late, by how much, its ready-by passed; nothing past the end.
    #[test]
    fn every_readiness_transition_is_d234s() {
        let at = Some(10 * S);
        let cases = [
            // starting, READY=1 by the ready-by, or with none: ready, on time.
            (
                State::Starting,
                Event::Ready(9 * S),
                at,
                State::Ready,
                Some(Ready::OnTime),
            ),
            (
                State::Starting,
                Event::Ready(10 * S),
                at,
                State::Ready,
                Some(Ready::OnTime),
            ),
            (
                State::Starting,
                Event::Ready(i64::MAX),
                None,
                State::Ready,
                Some(Ready::OnTime),
            ),
            // starting, READY=1 past it, the timer not yet heard: ready, late.
            (
                State::Starting,
                Event::Ready(12 * S),
                at,
                State::Ready,
                Some(Ready::Late(2 * S as u64)),
            ),
            // starting, the ready-by passes: passed, by how late it was seen.
            (
                State::Starting,
                Event::Due(10 * S),
                at,
                State::ReadyByPassed,
                Some(Ready::Passed(0)),
            ),
            (
                State::Starting,
                Event::Due(11 * S),
                at,
                State::ReadyByPassed,
                Some(Ready::Passed(S as u64)),
            ),
            // A timer with no ready-by moves nothing.
            (State::Starting, Event::Due(11 * S), None, State::Starting, None),
            // passed, READY=1: ready, late by how much, at the ready-by's own instant even.
            (
                State::ReadyByPassed,
                Event::Ready(13 * S),
                at,
                State::Ready,
                Some(Ready::Late(3 * S as u64)),
            ),
            (
                State::ReadyByPassed,
                Event::Ready(10 * S),
                at,
                State::Ready,
                Some(Ready::Late(0)),
            ),
            (
                State::ReadyByPassed,
                Event::Due(14 * S),
                at,
                State::ReadyByPassed,
                None,
            ),
            // ready, again: no new event.
            (State::Ready, Event::Ready(15 * S), at, State::Ready, None),
            (State::Ready, Event::Due(15 * S), at, State::Ready, None),
            // the workload's end, from each: exited, terminal.
            (State::Starting, Event::Exited, at, State::Exited, None),
            (State::Ready, Event::Exited, at, State::Exited, None),
            (State::ReadyByPassed, Event::Exited, at, State::Exited, None),
            (State::Exited, Event::Ready(1), at, State::Exited, None),
            (State::Exited, Event::Due(1), at, State::Exited, None),
            (State::Exited, Event::Exited, at, State::Exited, None),
        ];
        for (from, event, deadline, to, told) in cases {
            assert_eq!(
                step(from, event, deadline),
                (to, told),
                "{from:?} {event:?} {deadline:?}"
            );
        }
        // Each state met each event above.
        for from in [State::Starting, State::Ready, State::ReadyByPassed, State::Exited] {
            for event in [Event::Ready(0), Event::Due(0), Event::Exited] {
                assert!(
                    cases.iter().any(
                        |c| c.0 == from && std::mem::discriminant(&c.1) == std::mem::discriminant(&event)
                    ),
                    "{from:?} {event:?} untested"
                );
            }
        }
    }

    /// A datagram says READY=1 as sd_notify(3) writes it: a line of its own, among others
    /// ignored; one trailing NUL taken; a NUL elsewhere has it ignored, as systemd does.
    #[test]
    fn notify_datagrams_say_ready_as_sd_notify_writes_it() {
        for ready in [
            &b"READY=1"[..],
            b"READY=1\n",
            b"STATUS=serving\nREADY=1",
            b"READY=1\nX_HEX=node",
            b"READY=1\0",
        ] {
            assert!(says_ready(ready), "{ready:?}");
        }
        for not in [
            &b""[..],
            b"READY=0",
            b"READY=10",
            b"XREADY=1",
            b"READY= 1",
            b" READY=1",
            b"STATUS=READY=1",
            b"WATCHDOG=1\nSTOPPING=1",
            b"READY=1\0\0",
            b"READY=1\0X=1",
            b"\0READY=1",
        ] {
            assert!(!says_ready(not), "{not:?}");
        }
    }

    /// Only a process of the workload's cgroup, or of one beneath it, is heard: never one
    /// of another run's cgroup, a domain's, init's own, or one whose cgroup is unread.
    #[test]
    fn ready_from_outside_the_workloads_cgroup_is_ignored() {
        let w = "/run/workload";
        for inside in [
            "0::/run/workload\n",
            "0::/run/workload/init\n",
            "0::/run/workload/boxes/b1\n",
        ] {
            assert!(within(inside, w), "{inside}");
        }
        for outside in [
            "0::/run/workloadx\n",
            "0::/run/join-1\n",
            "0::/run/domains/agent-a\n",
            "0::/run/engine\n",
            "0::/run\n",
            "0::/\n",
            "1:name=systemd:/run/workload\n",
            "",
        ] {
            assert!(!within(outside, w), "{outside}");
        }
    }

    /// The socket as a granted run's is made, at a path of the test's own.
    fn socket(dir: &std::path::Path) -> (OwnedFd, UnixDatagram) {
        let path = dir.join("notify");
        let receiver = UnixDatagram::bind(&path).unwrap();
        let on: libc::c_int = 1;
        // SAFETY: setsockopt(2) of an int option on our own socket.
        let set = unsafe {
            libc::setsockopt(
                receiver.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                (&raw const on).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(set, 0);
        let sender = UnixDatagram::unbound().unwrap();
        sender.connect(&path).unwrap();
        (OwnedFd::from(receiver), sender)
    }

    /// This process's own cgroup, as /proc names it: the workload's, for the tests.
    fn own_cgroup() -> String {
        let text = std::fs::read_to_string("/proc/self/cgroup").unwrap();
        text.lines()
            .find_map(|l| l.strip_prefix("0::"))
            .unwrap()
            .to_string()
    }

    /// Datagrams as the kernel gives them: whole within PIPE_BUF, cut short past it, each
    /// with its sender named; and a descriptor a sender passes never installed in init.
    #[test]
    fn datagrams_past_pipe_buf_are_cut_and_senders_named() {
        let dir = shards_testdir::TempDir::new("notify-recv").unwrap();
        let (receiver, sender) = socket(dir.path());
        let mut buf = vec![0; libc::PIPE_BUF];
        let me = libc::pid_t::try_from(std::process::id()).unwrap();
        sender.send(b"READY=1").unwrap();
        assert_eq!(
            receive(receiver.as_raw_fd(), &mut buf).unwrap(),
            Some((7, Some(me)))
        );
        assert_eq!(&buf[..7], b"READY=1");
        sender.send(&[b'a'; libc::PIPE_BUF]).unwrap();
        assert_eq!(
            receive(receiver.as_raw_fd(), &mut buf).unwrap(),
            Some((libc::PIPE_BUF, Some(me)))
        );
        sender.send(&[b'a'; libc::PIPE_BUF + 1]).unwrap();
        assert_eq!(
            receive(receiver.as_raw_fd(), &mut buf).unwrap(),
            Some((libc::PIPE_BUF + 1, Some(me)))
        );
        assert_eq!(
            receive(receiver.as_raw_fd(), &mut buf).unwrap(),
            None,
            "none waits"
        );
        // A descriptor passed with READY=1: the datagram read, the descriptor never this
        // process's. Counted by a file of the test's alone, which no other test opens.
        let marker = dir.path().join("passed");
        std::fs::write(&marker, b"").unwrap();
        let passed = std::fs::File::open(&marker).unwrap();
        let held = || {
            std::fs::read_dir("/proc/self/fd")
                .unwrap()
                .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
                .filter(|link| *link == marker)
                .count()
        };
        assert_eq!(held(), 1);
        assert!(crate::hostbind::send_fd(
            sender.as_raw_fd(),
            b"READY=1",
            passed.as_raw_fd(),
            0
        ));
        assert_eq!(
            receive(receiver.as_raw_fd(), &mut buf).unwrap(),
            Some((7, Some(me)))
        );
        assert_eq!(held(), 1, "no descriptor of the sender's installed");
    }

    /// A granted run's readiness from its socket and timer: READY=1 from the workload's
    /// cgroup moves it once and is told; one from outside, or past PIPE_BUF, or with only
    /// other assignments, is not; its ready-by passing is told; the end lets both go.
    #[test]
    fn a_granted_runs_readiness_moves_by_its_socket_and_timer() {
        let dir = shards_testdir::TempDir::new("notify-ready").unwrap();
        // Held to a cgroup this process is not in: nothing it says is heard.
        let (receiver, sender) = socket(dir.path());
        let mut outside = Readiness::new(receiver, None, None, "/run/workload");
        sender.send(b"READY=1").unwrap();
        assert_eq!(outside.datagram(), None);
        assert_eq!(outside.state, State::Starting);

        // Held to its own: past PIPE_BUF, or other assignments alone, nothing; READY=1 once.
        let dir2 = shards_testdir::TempDir::new("notify-ready-own").unwrap();
        let (receiver, sender) = socket(dir2.path());
        let mut own = Readiness::new(receiver, None, None, &own_cgroup());
        let mut long = b"READY=1\n".to_vec();
        long.resize(libc::PIPE_BUF + 1, b'x');
        sender.send(&long).unwrap();
        assert_eq!(own.datagram(), None);
        sender.send(b"STATUS=starting").unwrap();
        assert_eq!(own.datagram(), None);
        assert_eq!(own.state, State::Starting);
        sender.send(b"READY=1").unwrap();
        assert_eq!(own.datagram(), Some(Ready::OnTime));
        assert_eq!(own.state, State::Ready);
        sender.send(b"READY=1").unwrap();
        assert_eq!(own.datagram(), None, "no new event");
        own.exited();
        assert_eq!(own.state, State::Exited);
        assert_eq!((own.socket(), own.timer()), (None, None));

        // A ready-by already passed: its timer fires at once, and READY=1 after it is late.
        let dir3 = shards_testdir::TempDir::new("notify-ready-late").unwrap();
        let (receiver, sender) = socket(dir3.path());
        let at = boot_now().unwrap() - S;
        let mut late = Readiness::new(receiver, Some(timer(at).unwrap()), Some(at), &own_cgroup());
        let mut polled = libc::pollfd {
            fd: late.timer().unwrap(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll(2) on one pollfd of a descriptor this test holds.
        assert_eq!(unsafe { libc::poll(&raw mut polled, 1, 10_000) }, 1);
        let passed = late.due().unwrap();
        assert!(
            matches!(passed, Ready::Passed(by) if by >= S as u64),
            "{passed:?}"
        );
        assert_eq!(late.state, State::ReadyByPassed);
        assert_eq!(late.timer(), None);
        sender.send(b"READY=1").unwrap();
        assert!(matches!(late.datagram(), Some(Ready::Late(by)) if by >= S as u64));
        assert_eq!(late.state, State::Ready);
    }
}
