//! The domain owner and its native, separately sealed namespace init (D59).

use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

const ACTOR: libc::pid_t = 2;
const READY: u8 = 1;
const SPLIT: u8 = 2;

#[repr(i32)]
enum Fd {
    Owner = 3,
    Gate,
    Split,
    Ready,
    Binary,
    Past,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Step {
    Helper = 1,
    Setup,
    Exec,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Failure {
    pub step: Step,
    pub errno: i32,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: errno {}", self.step, self.errno)
    }
}

impl std::error::Error for Failure {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryFault {
    PopulationControl { errno: i32 },
    LeaderControl { errno: i32 },
    StatusConflict,
}

impl std::fmt::Display for BoundaryFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PopulationControl { errno } => write!(f, "tenant population control lost: errno {errno}"),
            Self::LeaderControl { errno } => write!(f, "tenant leader control lost: errno {errno}"),
            Self::StatusConflict => f.write_str("conflicting owned termination status"),
        }
    }
}

impl std::error::Error for BoundaryFault {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuarantineCause {
    PopulationObservation { errno: i32 },
    Capture { errno: i32 },
}

impl QuarantineCause {
    pub fn errno(self) -> i32 {
        match self {
            Self::PopulationObservation { errno } | Self::Capture { errno } => errno,
        }
    }
}

impl std::fmt::Display for QuarantineCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PopulationObservation { errno } => write!(f, "population observation errno {errno}"),
            Self::Capture { errno } => write!(f, "capture errno {errno}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Update {
    Observed,
    Quarantined { cause: QuarantineCause },
}

impl Failure {
    fn protocol(step: Step) -> Self {
        Self {
            step,
            errno: libc::EPROTO,
        }
    }

    fn encode(self) -> [u8; 1 + size_of::<i32>()] {
        let [a, b, c, d] = self.errno.to_be_bytes();
        [self.step as u8, a, b, c, d]
    }

    fn decode(b: [u8; 1 + size_of::<i32>()]) -> Option<Self> {
        let [step, a, b, c, d] = b;
        let errno = i32::from_be_bytes([a, b, c, d]);
        if errno <= 0 {
            return None;
        }
        Some(Self {
            step: match step {
                1 => Step::Helper,
                2 => Step::Setup,
                3 => Step::Exec,
                _ => return None,
            },
            errno,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Starting,
    Running,
    Stopping,
    Draining {
        status: u32,
    },
    Ended {
        status: u32,
    },
    Quarantined {
        status: Option<u32>,
        cause: QuarantineCause,
    },
    Faulted {
        status: Option<u32>,
        fault: BoundaryFault,
    },
}

impl State {
    fn checkpoint_block(self) -> Option<(shards_abi::run::quiesce::Reason, i32)> {
        use shards_abi::run::quiesce::Reason;
        match self {
            Self::Running | Self::Ended { .. } => None,
            Self::Starting => Some((Reason::Starting, 0)),
            Self::Stopping => Some((Reason::Stopping, 0)),
            Self::Draining { .. } => Some((Reason::Draining, 0)),
            Self::Quarantined { cause, .. } => Some((Reason::Quarantined, cause.errno())),
            Self::Faulted { fault, .. } => Some((
                Reason::Faulted,
                match fault {
                    BoundaryFault::PopulationControl { errno } | BoundaryFault::LeaderControl { errno } => {
                        errno
                    }
                    BoundaryFault::StatusConflict => libc::EPROTO,
                },
            )),
        }
    }

    fn stopping(self) -> Self {
        match self {
            Self::Starting | Self::Running => Self::Stopping,
            other => other,
        }
    }

    fn reaped(self, status: u32) -> Result<Self, BoundaryFault> {
        match self {
            Self::Starting | Self::Running | Self::Stopping => Ok(Self::Draining { status }),
            Self::Draining { status: old } | Self::Ended { status: old } if old == status => Ok(self),
            Self::Quarantined { status: None, cause } => Ok(Self::Quarantined {
                status: Some(status),
                cause,
            }),
            Self::Quarantined {
                status: Some(old), ..
            } if old == status => Ok(self),
            Self::Faulted { status: None, fault } => Ok(Self::Faulted {
                status: Some(status),
                fault,
            }),
            Self::Faulted {
                status: Some(old), ..
            } if old == status => Ok(self),
            _ => Err(BoundaryFault::StatusConflict),
        }
    }

    fn empty(self) -> Self {
        match self {
            Self::Draining { status } => Self::Ended { status },
            other => other,
        }
    }

    fn status(self) -> Option<u32> {
        match self {
            Self::Draining { status } | Self::Ended { status } => Some(status),
            Self::Quarantined { status, .. } | Self::Faulted { status, .. } => status,
            _ => None,
        }
    }

    fn faulted(self, fault: BoundaryFault) -> Self {
        Self::Faulted {
            status: self.status(),
            fault,
        }
    }
}

#[derive(Debug)]
pub struct Owned {
    pid: libc::pid_t,
    pidfd: OwnedFd,
    _group: std::fs::File,
    kill: std::fs::File,
    events: std::fs::File,
    owner: Option<OwnedFd>,
    ready: Option<OwnedFd>,
    setup: Option<OwnedFd>,
    state: State,
}

/// All inherited descriptors exist before clone3; neither child allocates before exec.
pub struct Launch {
    binary: OwnedFd,
    events: std::fs::File,
    kill: std::fs::File,
    owner: (OwnedFd, OwnedFd),
    gate: (OwnedFd, OwnedFd),
    split: (OwnedFd, OwnedFd),
    ready: (OwnedFd, OwnedFd),
    setup: (OwnedFd, OwnedFd),
    pub pidfd: RawFd,
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: pipe2(2) writes both descriptors into this array, owned below.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let [read, write] = fds;
    // SAFETY: pipe2 just created both descriptors, with no other owner.
    Ok(unsafe { (OwnedFd::from_raw_fd(read), OwnedFd::from_raw_fd(write)) })
}

impl Launch {
    pub fn new(last_cap: u32, group: &std::fs::File) -> io::Result<Self> {
        // SAFETY: open(2) of init's own executable, before entering an actor root.
        let binary = unsafe { libc::open(c"/proc/self/exe".as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if binary < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor just opened, owned here alone.
        let binary = unsafe { OwnedFd::from_raw_fd(binary) };
        let owner = pipe()?;
        std::fs::File::from(owner.1.try_clone()?).write_all(&last_cap.to_be_bytes())?;
        Ok(Self {
            binary,
            events: open_at(group, c"cgroup.events", libc::O_RDONLY)?,
            kill: open_at(group, c"cgroup.kill", libc::O_WRONLY)?,
            owner,
            gate: pipe()?,
            split: pipe()?,
            ready: pipe()?,
            setup: pipe()?,
            pidfd: -1,
        })
    }

    /// Only the actor branch returns; the namespace init immediately execs fresh code.
    pub fn child(&self) -> RawFd {
        // SAFETY: raw clone as fork, from the namespace's single thread. No Rust runtime
        // or allocator is entered in either branch before its exec.
        let actor = unsafe { libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0) };
        if actor < 0 {
            raw_fail(self.ready.1.as_raw_fd(), Step::Helper);
        }
        if actor == 0 {
            // SAFETY: each descriptor belongs to this fork's table. The actor's mount
            // namespace separates before the helper changes its own root.
            unsafe {
                for fd in [
                    self.owner.0.as_raw_fd(),
                    self.owner.1.as_raw_fd(),
                    self.gate.1.as_raw_fd(),
                    self.split.0.as_raw_fd(),
                    self.ready.0.as_raw_fd(),
                    self.ready.1.as_raw_fd(),
                    self.setup.0.as_raw_fd(),
                    self.binary.as_raw_fd(),
                ] {
                    libc::close(fd);
                }
                if libc::unshare(libc::CLONE_NEWNS) != 0 || !raw_write(self.split.1.as_raw_fd(), &[SPLIT]) {
                    raw_fail(self.setup.1.as_raw_fd(), Step::Setup);
                }
                libc::close(self.split.1.as_raw_fd());
                if !gate_ready(self.gate.0.as_raw_fd()) {
                    raw_fail(self.setup.1.as_raw_fd(), Step::Helper);
                }
                libc::close(self.gate.0.as_raw_fd());
            }
            return self.setup.1.as_raw_fd();
        }
        if actor != libc::c_long::from(ACTOR) {
            raw_fail(self.ready.1.as_raw_fd(), Step::Helper);
        }
        self.exec_helper()
    }

    fn exec_helper(&self) -> ! {
        // SAFETY: all sources are first duplicated above the complete destination set.
        // Only the four protocol endpoints and CLOEXEC executable survive close_range.
        unsafe {
            let sources = [
                self.owner.0.as_raw_fd(),
                self.gate.1.as_raw_fd(),
                self.split.0.as_raw_fd(),
                self.ready.1.as_raw_fd(),
                self.binary.as_raw_fd(),
            ];
            let mut high = [-1; 5];
            for (source, target) in sources.into_iter().zip(high.iter_mut()) {
                *target = libc::fcntl(source, libc::F_DUPFD_CLOEXEC, Fd::Past as i32);
                if *target < 0 {
                    raw_fail(self.ready.1.as_raw_fd(), Step::Helper);
                }
            }
            for (source, target) in high.into_iter().zip(Fd::Owner as i32..Fd::Past as i32) {
                if libc::dup3(
                    source,
                    target,
                    if target == Fd::Binary as i32 {
                        libc::O_CLOEXEC
                    } else {
                        0
                    },
                ) < 0
                {
                    raw_fail(self.ready.1.as_raw_fd(), Step::Helper);
                }
            }
            for fd in libc::STDIN_FILENO..=libc::STDERR_FILENO {
                libc::close(fd);
            }
            if libc::syscall(libc::SYS_close_range, Fd::Past as u32, u32::MAX, 0u32) != 0 {
                raw_fail(Fd::Ready as i32, Step::Helper);
            }
            let args = [
                c"shards-init".as_ptr(),
                c"managed-reaper".as_ptr(),
                std::ptr::null(),
            ];
            let env = [std::ptr::null::<libc::c_char>()];
            libc::syscall(
                libc::SYS_execveat,
                Fd::Binary as i32,
                c"".as_ptr(),
                args.as_ptr(),
                env.as_ptr(),
                libc::AT_EMPTY_PATH,
            );
            raw_fail(Fd::Ready as i32, Step::Helper)
        }
    }

    pub fn parent(self, pid: libc::pid_t, group: std::fs::File) -> io::Result<Owned> {
        if self.pidfd < 0 {
            let _ = kill_population(&self.kill);
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        // SAFETY: CLONE_PIDFD stored a new descriptor into this slot in the parent.
        let pidfd = unsafe { OwnedFd::from_raw_fd(self.pidfd) };
        let owned = Owned {
            pid,
            pidfd,
            _group: group,
            kill: self.kill,
            events: self.events,
            owner: Some(self.owner.1),
            ready: Some(self.ready.0),
            setup: Some(self.setup.0),
            state: State::Starting,
        };
        Ok(owned)
    }
}

impl Owned {
    pub fn group(&self) -> &std::fs::File {
        &self._group
    }

    pub fn launched(&mut self) -> Result<(), Failure> {
        if self.state != State::Starting {
            return Err(Failure::protocol(Step::Setup));
        }
        let fd = self.ready.take().ok_or(Failure::protocol(Step::Helper))?;
        helper_ready(fd)?;
        let fd = self.setup.take().ok_or(Failure::protocol(Step::Setup))?;
        actor_exec(fd)?;
        self.state = State::Running;
        Ok(())
    }

    pub fn awaiting_status(&self, pid: libc::pid_t) -> bool {
        self.pid == pid
            && matches!(
                self.state,
                State::Starting
                    | State::Running
                    | State::Stopping
                    | State::Quarantined { status: None, .. }
                    | State::Faulted { status: None, .. }
            )
    }
    pub fn events(&self) -> Option<RawFd> {
        match self.state {
            State::Ended { .. } | State::Quarantined { .. } | State::Faulted { .. } => None,
            _ => Some(self.events.as_raw_fd()),
        }
    }
    pub fn ended(&self) -> bool {
        matches!(self.state, State::Ended { .. })
    }

    /// Its status once its whole population has ended, which its owner may be told (D231);
    /// none before, nor where it is quarantined or faulted, its emptiness unknown.
    pub fn ended_status(&self) -> Option<u32> {
        match self.state {
            State::Ended { status } => Some(status),
            _ => None,
        }
    }

    pub fn checkpoint_block(&self) -> Option<(shards_abi::run::quiesce::Reason, i32)> {
        self.state.checkpoint_block()
    }

    pub fn quarantined(&self) -> bool {
        matches!(self.state, State::Quarantined { .. })
    }

    /// A quarantine can finish VM disposal; it does not prove emptiness or release its charge.
    pub fn settled(&self) -> bool {
        self.ended() || self.quarantined()
    }

    pub fn stop(&mut self) -> Result<(), BoundaryFault> {
        if self.settled() {
            return Ok(());
        }
        self.state = self.state.stopping();
        self.owner = None;
        self.kill_population().map_err(|e| {
            let fault = BoundaryFault::PopulationControl {
                errno: e.raw_os_error().unwrap_or(libc::EIO),
            };
            self.state = self.state.faulted(fault);
            fault
        })
    }

    pub fn reaped(&mut self, status: u32) -> Result<Update, BoundaryFault> {
        self.state = self
            .state
            .reaped(status)
            .inspect_err(|&fault| self.state = self.state.faulted(fault))?;
        if self.quarantined() {
            return Ok(Update::Observed);
        }
        self.owner = None;
        self.stop()?;
        self.changed()
    }

    pub fn changed(&mut self) -> Result<Update, BoundaryFault> {
        if self.settled() {
            return Ok(Update::Observed);
        }
        match self.population_empty() {
            Ok(true) => self.state = self.state.empty(),
            Ok(false) => {}
            Err(e) => {
                return self.quarantine(QuarantineCause::PopulationObservation {
                    errno: e.raw_os_error().unwrap_or(libc::EIO),
                });
            }
        }
        Ok(Update::Observed)
    }

    pub fn quarantine(&mut self, cause: QuarantineCause) -> Result<Update, BoundaryFault> {
        match self.state {
            State::Ended { .. } | State::Quarantined { .. } => return Ok(Update::Observed),
            State::Faulted { fault, .. } => return Err(fault),
            _ => {}
        }
        self.stop()?;
        if let Err(e) = signal_stop(&self.pidfd) {
            let fault = BoundaryFault::LeaderControl {
                errno: e.raw_os_error().unwrap_or(libc::EIO),
            };
            self.state = self.state.faulted(fault);
            return Err(fault);
        }
        self.state = State::Quarantined {
            status: self.state.status(),
            cause,
        };
        Ok(Update::Quarantined { cause })
    }

    fn population_empty(&self) -> io::Result<bool> {
        // cgroup.events contains boolean populated/frozen fields (cgroup-v2.rst).
        let mut bytes = [0; b"populated 1\nfrozen 1\n".len()];
        let n = loop {
            // SAFETY: pread(2) reads the held cgroup's event file at its start.
            let n =
                unsafe { libc::pread(self.events.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len(), 0) };
            if n < 0 && errno() == libc::EINTR {
                continue;
            }
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            break n;
        };
        let got = usize::try_from(n)
            .ok()
            .and_then(|n| bytes.get(..n))
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))?;
        let mut value = None;
        for line in got.split(|&b| b == b'\n') {
            if line == b"populated 0" {
                value = Some(true);
            }
            if line == b"populated 1" {
                value = Some(false);
            }
        }
        value.ok_or_else(|| io::Error::from_raw_os_error(libc::EPROTO))
    }

    fn kill_population(&self) -> io::Result<()> {
        kill_population(&self.kill)
    }
}

fn signal_stop(pidfd: &OwnedFd) -> io::Result<()> {
    loop {
        // SAFETY: the held pidfd names this tenant's namespace init; it cannot select a reused PID.
        let stopped = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if stopped == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => return Ok(()),
            Some(libc::EINTR) => {}
            _ => return Err(error),
        }
    }
}

fn kill_population(file: &std::fs::File) -> io::Result<()> {
    use std::os::unix::fs::FileExt as _;
    loop {
        // kernfs preserves FMODE_PWRITE for writable controls (Linux6.18 fs/kernfs/file.c).
        match file.write_at(b"1", 0) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Ok(1) => return Ok(()),
            Ok(_) => return Err(io::Error::from_raw_os_error(libc::EIO)),
            Err(e) => return Err(e),
        }
    }
}

fn helper_ready(fd: OwnedFd) -> Result<(), Failure> {
    let mut ready = std::fs::File::from(fd);
    let mut word = [0; 1 + size_of::<i32>()];
    ready
        .read_exact(&mut word)
        .map_err(|_| Failure::protocol(Step::Helper))?;
    if word != [READY, 0, 0, 0, 0] {
        return Err(Failure::decode(word).unwrap_or(Failure::protocol(Step::Helper)));
    }
    Ok(())
}

fn actor_exec(fd: OwnedFd) -> Result<(), Failure> {
    let mut setup = std::fs::File::from(fd);
    let mut first = [0];
    let n = loop {
        match setup.read(&mut first) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(Failure::protocol(Step::Setup)),
            Ok(n) => break n,
        }
    };
    if n != 0 {
        let mut rest = [0; size_of::<i32>()];
        setup
            .read_exact(&mut rest)
            .map_err(|_| Failure::protocol(Step::Setup))?;
        let [a, b, c, d] = rest;
        let [first] = first;
        return Err(Failure::decode([first, a, b, c, d]).unwrap_or(Failure::protocol(Step::Setup)));
    }
    Ok(())
}

fn gate_ready(fd: RawFd) -> bool {
    let mut word = [0];
    raw_read(fd, &mut word) && word == [READY]
}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.ended() {
            self.state = self.state.stopping();
            self.owner = None;
            let _ = self.kill_population();
            // SAFETY: this held pidfd selects only this namespace init, never a reused PID.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
        }
    }
}

fn open_at(group: &std::fs::File, name: &std::ffi::CStr, flags: i32) -> io::Result<std::fs::File> {
    // SAFETY: openat(2) resolves a fixed control name relative to the held cgroup.
    let fd = unsafe { libc::openat(group.as_raw_fd(), name.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor openat just made, owned here alone.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

fn errno() -> i32 {
    match io::Error::last_os_error().raw_os_error() {
        Some(n) if n != 0 => n,
        _ => libc::EIO,
    }
}

pub fn raw_fail(fd: RawFd, step: Step) -> ! {
    report_failure(fd, step, errno());
    // SAFETY: _exit(2) never invokes inherited Rust teardown.
    unsafe { libc::_exit(125) }
}

pub fn report_failure(fd: RawFd, step: Step, errno: i32) {
    let word = Failure {
        step,
        errno: if errno == 0 { libc::EIO } else { errno },
    }
    .encode();
    let _ = raw_write(fd, &word);
}

fn raw_read(fd: RawFd, mut bytes: &mut [u8]) -> bool {
    while !bytes.is_empty() {
        // SAFETY: read(2) writes only the remaining local slice.
        let n = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        if n < 0 && errno() == libc::EINTR {
            continue;
        }
        let Ok(n) = usize::try_from(n) else { return false };
        if n == 0 {
            return false;
        }
        let Some(rest) = bytes.get_mut(n..) else {
            return false;
        };
        bytes = rest;
    }
    true
}

fn raw_write(fd: RawFd, mut bytes: &[u8]) -> bool {
    while !bytes.is_empty() {
        // SAFETY: write(2) reads only the remaining borrowed slice.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 && errno() == libc::EINTR {
            continue;
        }
        let Ok(n) = usize::try_from(n) else { return false };
        if n == 0 {
            return false;
        }
        let Some(rest) = bytes.get(n..) else { return false };
        bytes = rest;
    }
    true
}

pub fn reaper() -> ! {
    if std::process::id() != 1 {
        raw_fail(Fd::Ready as i32, Step::Helper);
    }
    let mut cap = [0; size_of::<u32>()];
    let mut split = [0];
    if !raw_read(Fd::Owner as i32, &mut cap)
        || !raw_read(Fd::Split as i32, &mut split)
        || split != [SPLIT]
        || !seal(u32::from_be_bytes(cap))
    {
        raw_fail(Fd::Ready as i32, Step::Helper);
    }
    // SAFETY: close(2) of the completed split endpoint.
    unsafe { libc::close(Fd::Split as i32) };
    if !raw_write(Fd::Ready as i32, &[READY, 0, 0, 0, 0]) || !raw_write(Fd::Gate as i32, &[READY]) {
        raw_fail(Fd::Ready as i32, Step::Helper);
    }
    // SAFETY: close(2) of completed one-way readiness endpoints.
    unsafe {
        libc::close(Fd::Ready as i32);
        libc::close(Fd::Gate as i32);
    }
    reaping()
}

fn seal(last_cap: u32) -> bool {
    // Linux securebits.h: NOROOT and NO_SETUID_FIXUP, each irreversibly locked.
    const SECURE: libc::c_ulong = (1 << 0) | (1 << 1) | (1 << 2) | (1 << 3);
    // SAFETY: all calls use fixed paths/local structures and drop authority before gate release.
    unsafe {
        let shut = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
        libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        ) == 0
            && libc::mount(
                c"tmpfs".as_ptr(),
                c"/proc".as_ptr(),
                c"tmpfs".as_ptr(),
                shut,
                std::ptr::null(),
            ) == 0
            && libc::chroot(c"/proc".as_ptr()) == 0
            && libc::chdir(c"/".as_ptr()) == 0
            && libc::setgroups(0, std::ptr::null()) == 0
            && libc::prctl(libc::PR_SET_SECUREBITS, SECURE, 0, 0, 0) == 0
            && crate::defaults::bound(last_cap, |_| false)
            && libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) == 0
            && crate::defaults::set(last_cap, |_| false)
            && libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) == 0
            && libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0
            && signals()
            && filter()
    }
}

fn filter() -> bool {
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc000_00b7;
    // linux/filter.h, seccomp.h and audit.h: native arch, then only this loop's calls.
    let op = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    let mut instructions = [op(0, 0, 0, 0); 4 + 2 * ALLOWED.len() + 1];
    let head = [
        op(0x20, 0, 0, 4),
        op(0x15, 1, 0, ARCH),
        op(0x06, 0, 0, 0x8000_0000),
        op(0x20, 0, 0, 0),
    ];
    for (to, from) in instructions.iter_mut().zip(head) {
        *to = from;
    }
    for (chunk, syscall) in instructions
        .get_mut(head.len()..)
        .into_iter()
        .flat_map(|tail| tail.chunks_mut(2))
        .zip(ALLOWED)
    {
        if let Some(at) = chunk.get_mut(0) {
            *at = op(0x15, 0, 1, syscall as u32);
        }
        if let Some(at) = chunk.get_mut(1) {
            *at = op(0x06, 0, 0, 0x7fff_0000);
        }
    }
    if let Some(last) = instructions.last_mut() {
        *last = op(0x06, 0, 0, 0x8000_0000);
    }
    let Ok(len) = u16::try_from(instructions.len()) else {
        return false;
    };
    let program = libc::sock_fprog {
        len,
        filter: instructions.as_mut_ptr(),
    };
    // SAFETY: seccomp(2) copies the complete stack-resident BPF program.
    unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0,
            &raw const program,
        ) == 0
    }
}

const ALLOWED: [libc::c_long; 8] = [
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_close,
    libc::SYS_wait4,
    libc::SYS_ppoll,
    libc::SYS_rt_sigreturn,
    libc::SYS_exit,
    libc::SYS_exit_group,
];

extern "C" fn child_changed(_: libc::c_int) {}

fn signals() -> bool {
    // SAFETY: the fresh helper is single-threaded; the handler is async-signal-safe.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = child_changed as extern "C" fn(libc::c_int) as libc::sighandler_t;
        libc::sigemptyset(&raw mut action.sa_mask);
        let mut signals: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut signals);
        libc::sigaddset(&raw mut signals, libc::SIGCHLD);
        libc::sigaction(libc::SIGCHLD, &raw const action, std::ptr::null_mut()) == 0
            && libc::sigprocmask(libc::SIG_SETMASK, &raw const signals, std::ptr::null_mut()) == 0
    }
}

fn reaping() -> ! {
    // SAFETY: only this fresh thread owns the namespace's wait statuses. SIGCHLD is
    // blocked except during ppoll's atomic wait, preventing a lost wakeup.
    unsafe {
        loop {
            let mut status = 0;
            loop {
                let pid = libc::syscall(
                    libc::SYS_wait4,
                    -1,
                    &raw mut status,
                    libc::WNOHANG,
                    std::ptr::null_mut::<libc::rusage>(),
                );
                if pid == libc::c_long::from(ACTOR) {
                    libc::_exit(if libc::WIFSIGNALED(status) {
                        128 + libc::WTERMSIG(status)
                    } else {
                        libc::WEXITSTATUS(status)
                    });
                }
                if pid > 0 {
                    continue;
                }
                if pid < 0 && errno() == libc::EINTR {
                    continue;
                }
                if pid < 0 {
                    libc::_exit(125);
                }
                break;
            }
            let mut owner = libc::pollfd {
                fd: Fd::Owner as i32,
                events: libc::POLLIN,
                revents: 0,
            };
            let mut empty: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&raw mut empty);
            // Kernel rt_sigset_t is one u64 on both supported Linux guest architectures.
            let n = libc::syscall(
                libc::SYS_ppoll,
                &raw mut owner,
                1,
                std::ptr::null::<libc::timespec>(),
                &raw const empty,
                size_of::<u64>(),
            );
            if n < 0 && errno() == libc::EINTR {
                continue;
            }
            libc::_exit(125);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    fn written(bytes: &[u8]) -> OwnedFd {
        let (read, write) = pipe().unwrap();
        let mut write = std::fs::File::from(write);
        write.write_all(bytes).unwrap();
        drop(write);
        read
    }

    #[test]
    fn only_running_and_proven_ended_populations_are_checkpoint_ready() {
        use shards_abi::run::quiesce::Reason;
        assert_eq!(State::Running.checkpoint_block(), None);
        assert_eq!(State::Ended { status: 17 }.checkpoint_block(), None);
        assert_eq!(State::Starting.checkpoint_block(), Some((Reason::Starting, 0)));
        assert_eq!(State::Stopping.checkpoint_block(), Some((Reason::Stopping, 0)));
        assert_eq!(
            State::Draining { status: 17 }.checkpoint_block(),
            Some((Reason::Draining, 0))
        );
        for status in [None, Some(17)] {
            assert_eq!(
                State::Quarantined {
                    status,
                    cause: QuarantineCause::PopulationObservation { errno: libc::EISDIR }
                }
                .checkpoint_block(),
                Some((Reason::Quarantined, libc::EISDIR))
            );
            assert_eq!(
                State::Faulted {
                    status,
                    fault: BoundaryFault::StatusConflict
                }
                .checkpoint_block(),
                Some((Reason::Faulted, libc::EPROTO))
            );
        }
    }

    #[test]
    fn only_a_complete_fresh_helper_ready_record_releases_startup() {
        let ready = [READY, 0, 0, 0, 0];
        for cut in 0..ready.len() {
            assert!(helper_ready(written(&ready[..cut])).is_err(), "cut {cut}");
        }
        assert_eq!(helper_ready(written(&ready)), Ok(()));
        assert!(helper_ready(written(&[SPLIT, 0, 0, 0, 0])).is_err());
        let error = Failure {
            step: Step::Helper,
            errno: libc::EPERM,
        };
        assert_eq!(helper_ready(written(&error.encode())), Err(error));
    }

    #[test]
    fn eof_or_an_unexpected_gate_word_never_releases_the_actor() {
        for word in [b"".as_slice(), &[0], &[SPLIT], &[u8::MAX]] {
            assert!(!gate_ready(written(word).as_raw_fd()));
        }
        assert!(gate_ready(written(&[READY]).as_raw_fd()));
    }

    #[test]
    fn exec_closes_its_status_but_setup_and_exec_failures_stay_typed() {
        assert_eq!(actor_exec(written(&[])), Ok(()));
        for step in [Step::Setup, Step::Exec] {
            let error = Failure {
                step,
                errno: libc::ENOENT,
            };
            assert_eq!(actor_exec(written(&error.encode())), Err(error));
            for cut in 1..error.encode().len() {
                assert_eq!(
                    actor_exec(written(&error.encode()[..cut])),
                    Err(Failure::protocol(Step::Setup))
                );
            }
        }
    }

    #[test]
    fn an_empty_population_cannot_supply_an_unobserved_exit_status() {
        for state in [State::Starting, State::Running, State::Stopping] {
            assert_eq!(state.empty(), state);
        }
        let draining = State::Running.reaped(17).unwrap();
        assert_eq!(draining, State::Draining { status: 17 });
        assert_eq!(draining.empty(), State::Ended { status: 17 });
        assert_eq!(draining.stopping(), draining);
    }

    #[test]
    fn a_second_completion_cannot_replace_the_owned_status() {
        for state in [State::Draining { status: 17 }, State::Ended { status: 17 }] {
            assert_eq!(state.reaped(17).unwrap(), state);
            assert_eq!(state.reaped(18).unwrap_err(), BoundaryFault::StatusConflict);
            assert_eq!(state.stopping(), state);
        }
    }

    #[test]
    fn malformed_status_never_becomes_a_successful_launch() {
        for word in [
            [0, 0, 0, 0, 1],
            [u8::MAX, 0, 0, 0, 1],
            [1, 0, 0, 0, 0],
            [1, 255, 255, 255, 255],
        ] {
            assert!(Failure::decode(word).is_none());
            assert!(actor_exec(written(&word)).is_err());
        }
    }

    struct Child {
        pid: Option<libc::pid_t>,
        pidfd: OwnedFd,
    }

    impl Child {
        fn start() -> Self {
            let mut pidfd: libc::c_int = -1;
            // SAFETY: a private fixture child; its branch calls only raw setup/pause/exit.
            let parent = unsafe { libc::getpid() };
            // clone(2) as fork(2), as init's own forks are (`Launch::child`), not clone3,
            // which Docker's default seccomp profile, CI's musl container's, answers ENOSYS
            // (crates/seccomp/data/default.json); clone(2) carries this request whole.
            // SAFETY: CLONE_PIDFD writes the pidfd through the third argument, parent_tid
            // (kernel/fork.c), in the parent only. No stack, TLS or child TID: the last two
            // arguments are 0 in either order an architecture takes them.
            let pid = unsafe {
                libc::syscall(
                    libc::SYS_clone,
                    (libc::CLONE_PIDFD | libc::SIGCHLD) as libc::c_long,
                    0 as libc::c_long,
                    &raw mut pidfd,
                    0 as libc::c_long,
                    0 as libc::c_long,
                )
            };
            if pid == 0 {
                // SAFETY: no inherited allocator/runtime state is entered by the child.
                unsafe {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0
                        || libc::getppid() != parent
                    {
                        libc::_exit(1);
                    }
                    loop {
                        libc::pause();
                    }
                }
            }
            assert!(pid > 0, "clone: {}", io::Error::last_os_error());
            assert!(pidfd >= 0);
            Self {
                pid: Some(libc::pid_t::try_from(pid).unwrap()),
                // SAFETY: CLONE_PIDFD created the sole descriptor stored here.
                pidfd: unsafe { OwnedFd::from_raw_fd(pidfd) },
            }
        }

        fn reap(&mut self) -> u32 {
            let pid = self.pid.take().unwrap();
            let mut status = 0;
            loop {
                // SAFETY: this fixture alone waits for its own child.
                let got = unsafe { libc::waitpid(pid, &raw mut status, 0) };
                if got == pid {
                    break;
                }
                assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::Interrupted);
            }
            assert!(libc::WIFSIGNALED(status));
            assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
            128 + libc::WTERMSIG(status) as u32
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            if self.pid.is_some() {
                let _ = signal_stop(&self.pidfd);
                self.reap();
            }
        }
    }

    fn wrong_event_owner(dir: &std::path::Path, pidfd: OwnedFd, pid: libc::pid_t) -> Owned {
        let group = std::fs::File::open(dir).unwrap();
        let kill = std::fs::File::create(dir.join("kill")).unwrap();
        let (_, owner) = pipe().unwrap();
        Owned {
            pid,
            pidfd,
            _group: group.try_clone().unwrap(),
            kill,
            // Reading a private directory gives EISDIR, never a real kernel cgroup fault.
            events: group,
            owner: Some(owner),
            ready: None,
            setup: None,
            state: State::Running,
        }
    }

    #[test]
    fn a_capture_failure_uses_the_same_owned_containment_and_keeps_its_cause() {
        let mut child = Child::start();
        let dir = shards_testdir::TempDir::new("lifecycle-capture-error").unwrap();
        let mut domain = wrong_event_owner(&dir, child.pidfd.try_clone().unwrap(), child.pid.unwrap());
        let cause = QuarantineCause::Capture { errno: libc::ENOMEM };
        assert_eq!(domain.quarantine(cause), Ok(Update::Quarantined { cause }));
        assert!(domain.owner.is_none());
        assert!(domain.events().is_none());
        assert_eq!(std::fs::read(dir.join("kill")).unwrap(), b"1");
        assert_eq!(
            domain.quarantine(QuarantineCause::PopulationObservation { errno: libc::EISDIR }),
            Ok(Update::Observed)
        );
        assert_eq!(domain.state, State::Quarantined { status: None, cause });
        let status = child.reap();
        assert_eq!(domain.reaped(status), Ok(Update::Observed));
        assert_eq!(
            domain.state,
            State::Quarantined {
                status: Some(status),
                cause
            }
        );
        assert_eq!(
            domain.checkpoint_block(),
            Some((shards_abi::run::quiesce::Reason::Quarantined, libc::ENOMEM))
        );
    }

    #[test]
    fn quarantine_preserves_ended_proof_and_a_prior_boundary_failure() {
        let dir = shards_testdir::TempDir::new("lifecycle-terminal-quarantine").unwrap();
        let (wrong_pidfd, other) = pipe().unwrap();
        drop(other);
        let mut domain = wrong_event_owner(&dir, wrong_pidfd, 0);
        // Neither terminal call may issue the invalid stop controls.
        domain.kill = std::fs::File::open(dir.join("kill")).unwrap();
        let cause = QuarantineCause::Capture { errno: libc::ENOMEM };
        domain.state = State::Ended { status: 17 };
        assert_eq!(domain.quarantine(cause), Ok(Update::Observed));
        assert_eq!(domain.state, State::Ended { status: 17 });
        let fault = BoundaryFault::PopulationControl { errno: libc::EBADF };
        domain.state = State::Faulted {
            status: Some(17),
            fault,
        };
        assert_eq!(domain.quarantine(cause), Err(fault));
        assert_eq!(
            domain.state,
            State::Faulted {
                status: Some(17),
                fault
            }
        );
    }

    #[test]
    fn a_real_observation_error_quarantines_only_the_owned_tenant() {
        let mut child = Child::start();
        let sibling = Child::start();
        let dir = shards_testdir::TempDir::new("lifecycle-events-error").unwrap();
        let mut domain = wrong_event_owner(&dir, child.pidfd.try_clone().unwrap(), child.pid.unwrap());
        assert_eq!(
            domain.changed(),
            Ok(Update::Quarantined {
                cause: QuarantineCause::PopulationObservation { errno: libc::EISDIR }
            })
        );
        assert_eq!(
            domain.state,
            State::Quarantined {
                status: None,
                cause: QuarantineCause::PopulationObservation { errno: libc::EISDIR }
            }
        );
        assert!(domain.owner.is_none());
        assert!(domain.events().is_none());
        assert!(domain.settled());
        assert!(!domain.ended());
        assert_eq!(std::fs::read(dir.join("kill")).unwrap(), b"1");
        let status = child.reap();
        assert_eq!(domain.reaped(status), Ok(Update::Observed));
        assert_eq!(
            domain.state,
            State::Quarantined {
                status: Some(status),
                cause: QuarantineCause::PopulationObservation { errno: libc::EISDIR }
            }
        );
        assert_eq!(domain.changed(), Ok(Update::Observed));
        assert!(!domain.ended());
        let mut poll = libc::pollfd {
            fd: sibling.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: a nonblocking look at the independent fixture child's pidfd.
        assert_eq!(unsafe { libc::poll(&raw mut poll, 1, 0) }, 0);
    }

    #[test]
    fn a_lost_population_control_is_a_shared_boundary_failure() {
        let dir = shards_testdir::TempDir::new("lifecycle-kill-error").unwrap();
        let (wrong_pidfd, other) = pipe().unwrap();
        drop(other);
        let mut domain = wrong_event_owner(&dir, wrong_pidfd, 0);
        domain.kill = std::fs::File::open(dir.join("kill")).unwrap();
        let fault = BoundaryFault::PopulationControl { errno: libc::EBADF };
        assert_eq!(domain.changed(), Err(fault));
        assert_eq!(domain.state, State::Faulted { status: None, fault });
        assert!(!domain.settled());
        assert!(domain.owner.is_none());
        assert!(domain.events().is_none());
    }

    #[test]
    fn a_lost_leader_identity_is_a_shared_boundary_failure() {
        let dir = shards_testdir::TempDir::new("lifecycle-identity-error").unwrap();
        let (wrong_pidfd, other) = pipe().unwrap();
        drop(other);
        let mut domain = wrong_event_owner(&dir, wrong_pidfd, 0);
        let fault = BoundaryFault::LeaderControl { errno: libc::EBADF };
        assert_eq!(domain.changed(), Err(fault));
        assert_eq!(domain.state, State::Faulted { status: None, fault });
        assert!(!domain.settled());
        assert!(domain.events().is_none());
    }
}
