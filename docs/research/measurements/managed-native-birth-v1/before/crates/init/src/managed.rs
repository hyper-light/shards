//! The domain owner and its native, separately sealed namespace init (D59).

#[cfg(test)]
use std::io::Read as _;
use std::io::{self, Write as _};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

mod channel;
mod control;
mod native;
pub(crate) use native::Failure as NativeFailure;
mod reaper;
mod receipts;
mod setup;

pub use control::{Error as ControlFailure, LaunchFailure, Protocol as ControlProtocol};
pub(crate) use setup::ChildSetup;
pub use shards_abi::run::lifecycle::{Failure, Step};

const ACTOR: libc::pid_t = 2;
#[cfg(test)]
const READY: u8 = 1;
const SPLIT: u8 = 2;

#[repr(i32)]
enum Fd {
    Owner = 3,
    Gate,
    Split,
    Ready,
    Binary,
}

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
    HelperExit { raw_status: i32 },
    UnobservedWait { cause: ControlFailure },
}

impl QuarantineCause {
    pub fn errno(self) -> i32 {
        match self {
            Self::PopulationObservation { errno } | Self::Capture { errno } => errno,
            Self::HelperExit { .. } => libc::EPIPE,
            Self::UnobservedWait { cause } => cause.errno(),
        }
    }
}

impl std::fmt::Display for QuarantineCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PopulationObservation { errno } => write!(f, "population observation errno {errno}"),
            Self::Capture { errno } => write!(f, "capture errno {errno}"),
            Self::HelperExit { raw_status } => write!(f, "owned helper raw wait status {raw_status}"),
            Self::UnobservedWait { cause } => write!(f, "matching child waits unobserved: {cause}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Update {
    Observed,
    Quarantined { cause: QuarantineCause },
}

#[cfg(test)]
fn protocol(step: Step) -> Failure {
    Failure::native(step, libc::EPROTO).unwrap()
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
    receipts: receipts::Receipts,
    setup: Option<OwnedFd>,
    state: State,
    native: Option<native::Owner>,
}

/// The exact kill/events objects opened by checked cgroup admission.
#[derive(Debug)]
pub(crate) struct Controls {
    kill: std::fs::File,
    events: std::fs::File,
}

impl Controls {
    /// Held by the checked cgroup admission producer before any child release.
    /// These objects confer no unused memory, task or future queue reservation.
    pub(crate) fn held(kill: std::fs::File, events: std::fs::File) -> Self {
        Self { kill, events }
    }
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
    files: shards_ipc::child_fds::PreparedFdMap,
    helper_required: bool,
    native: Option<native::Owner>,
    pub pidfd: RawFd,
}

#[derive(Debug)]
pub(crate) enum PreparationFailure {
    Native(io::Error),
    Map(shards_ipc::child_fds::Failure),
}

impl From<io::Error> for PreparationFailure {
    fn from(cause: io::Error) -> Self {
        Self::Native(cause)
    }
}

impl std::fmt::Display for PreparationFailure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Native(cause) => write!(
                out,
                "owner preparation: {:?}, OS {:?}",
                cause.kind(),
                cause.raw_os_error()
            ),
            Self::Map(cause) => write!(out, "owner descriptor preparation: {cause:?}"),
        }
    }
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

fn status_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let pair = pipe()?;
    // SAFETY: this read endpoint is held here alone before clone; the write
    // endpoint is a different open file description and keeps its existing mode.
    let flags = unsafe { libc::fcntl(pair.0.as_raw_fd(), libc::F_GETFL) };
    // SAFETY: the same held read endpoint and its existing flags, before clone.
    if flags < 0 || unsafe { libc::fcntl(pair.0.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pair)
}

impl Launch {
    pub(crate) fn new(
        last_cap: u32,
        controls: Controls,
        helper: Option<BorrowedFd<'_>>,
    ) -> Result<Self, PreparationFailure> {
        // SAFETY: open(2) of init's own executable, before entering an actor root.
        let binary = unsafe { libc::open(c"/proc/self/exe".as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if binary < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: the descriptor just opened, owned here alone.
        let binary = unsafe { OwnedFd::from_raw_fd(binary) };
        let owner = pipe()?;
        std::fs::File::from(owner.1.try_clone()?).write_all(&last_cap.to_be_bytes())?;
        let gate = crate::hostbind::seqpacket()?;
        let split = pipe()?;
        let ready = crate::hostbind::seqpacket()?;
        let setup = status_pipe()?;
        use shards_ipc::child_fds::{Binding, Inheritance, PreparedFdMap};
        let bindings = [
            (owner.0.as_fd(), Fd::Owner, Inheritance::RetainAcrossExec),
            (gate.1.as_fd(), Fd::Gate, Inheritance::RetainAcrossExec),
            (split.0.as_fd(), Fd::Split, Inheritance::RetainAcrossExec),
            (ready.1.as_fd(), Fd::Ready, Inheritance::RetainAcrossExec),
            (binary.as_fd(), Fd::Binary, Inheritance::SetupOnly),
        ]
        .map(|(source, destination, inheritance)| Binding {
            source,
            destination: destination as RawFd,
            inheritance,
        });
        let files = PreparedFdMap::prepare(&bindings).map_err(PreparationFailure::Map)?;
        Ok(Self {
            binary,
            events: controls.events,
            kill: controls.kill,
            owner,
            gate,
            split,
            ready,
            setup,
            files,
            helper_required: helper.is_some(),
            native: None,
            pidfd: -1,
        })
    }

    /// The actual normalized Domain/Prepared supplies the only projection.
    /// Queue bootstrap before clone while both channel endpoints are held.
    pub(crate) fn prepare_native(
        &mut self,
        projection: &crate::domains::native_grants::Projection<'_>,
        scope: shards_abi::run::native_grants::Scope,
        context: &shards_abi::run::management::Context,
        policy: &crate::domains::Filter,
    ) -> Result<OwnedFd, NativeFailure> {
        if self.native.is_some() {
            return Err(NativeFailure::State);
        }
        let (prepared, helper) = native::Preparation::acquire(projection, scope, context, policy)?;
        self.native = Some(prepared.into_owner());
        Ok(helper)
    }

    /// Only the actor branch returns; the namespace init immediately execs fresh code.
    pub fn child(&self) -> ChildSetup {
        let mut actor_pidfd = -1;
        let mut args = crate::domains::CloneArgs {
            flags: libc::CLONE_PIDFD as u64,
            pidfd: (&raw mut actor_pidfd) as u64,
            exit_signal: libc::SIGCHLD as u64,
            ..Default::default()
        };
        // SAFETY: the single-threaded namespace init uses only raw setup before
        // exec. CLONE_PIDFD writes a stable actor handle into this parent slot.
        let actor = unsafe {
            libc::syscall(
                libc::SYS_clone3,
                &raw mut args,
                size_of::<crate::domains::CloneArgs>(),
            )
        };
        if actor < 0 {
            bootstrap_fail(self.ready.1.as_raw_fd(), Step::Helper, errno());
        }
        if actor == 0 {
            let staging = [Fd::Owner, Fd::Gate, Fd::Split, Fd::Ready, Fd::Binary].map(|destination| {
                self.files
                    .staged_source(destination as RawFd)
                    .map(|source| source.as_raw_fd())
            });
            let [Some(owner), Some(gate), Some(split), Some(ready), Some(binary)] = staging else {
                raw_control_fail(
                    self.setup.1.as_raw_fd(),
                    ControlFailure::Protocol(ControlProtocol::State).into(),
                );
            };
            // SAFETY: each descriptor belongs to this fork's table. The actor's mount
            // namespace separates before the helper changes its own root. All raw
            // staging numbers were captured before closure invalidates borrowed owners.
            unsafe {
                for source in [owner, gate, split, ready, binary] {
                    if libc::close(source) != 0 {
                        raw_fail(self.setup.1.as_raw_fd(), Step::Helper);
                    }
                }
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
                if libc::unshare(libc::CLONE_NEWNS) != 0 {
                    raw_fail(self.setup.1.as_raw_fd(), Step::Setup);
                }
                if let Err(cause) = raw_write(self.split.1.as_raw_fd(), &[SPLIT]) {
                    raw_control_fail(self.setup.1.as_raw_fd(), cause.into());
                }
                libc::close(self.split.1.as_raw_fd());
                if let Err(error) = setup::base_ready(self.gate.0.as_raw_fd()) {
                    raw_control_fail(self.setup.1.as_raw_fd(), error);
                }
            }
            return ChildSetup::new(
                self.setup.1.as_raw_fd(),
                self.gate.0.as_raw_fd(),
                self.helper_required,
            );
        }
        if actor != libc::c_long::from(ACTOR) || actor_pidfd < 0 {
            bootstrap_control_fail(
                self.ready.1.as_raw_fd(),
                ControlFailure::Protocol(ControlProtocol::Conflict).into(),
            );
        }
        if let Err(cause) = raw_write(self.owner.1.as_raw_fd(), &[u8::from(self.helper_required)]) {
            bootstrap_control_fail(self.ready.1.as_raw_fd(), cause.into());
        }
        // SAFETY: these inherited endpoints belong to the actor/outer owner.
        // In particular, retaining a liveness writer here would hide owner loss.
        unsafe {
            for fd in [
                self.owner.1.as_raw_fd(),
                self.ready.0.as_raw_fd(),
                self.gate.0.as_raw_fd(),
                self.split.1.as_raw_fd(),
                self.setup.0.as_raw_fd(),
                self.setup.1.as_raw_fd(),
            ] {
                libc::close(fd);
            }
        }
        if let Err(error) = associate_actor(self.ready.1.as_raw_fd(), self.owner.0.as_raw_fd(), actor_pidfd) {
            bootstrap_control_fail(self.ready.1.as_raw_fd(), error.into());
        }
        control::close(actor_pidfd);
        self.exec_helper()
    }

    /// Source for native's SAME pre-clone FD map; no actor-selected descriptor.
    pub(crate) fn helper_gate(&self) -> BorrowedFd<'_> {
        self.gate.0.as_fd()
    }

    fn exec_helper(&self) -> ! {
        let Some(staged_ready) = self.files.staged_source(Fd::Ready as RawFd) else {
            bootstrap_control_fail(
                self.ready.1.as_raw_fd(),
                ControlFailure::Protocol(ControlProtocol::State).into(),
            );
        };
        let staged_ready = staged_ready.as_raw_fd();
        // SAFETY: only this raw reaper child installs its pre-clone owned map;
        // it execs or _exits on all outcomes without inherited Rust teardown.
        if let Err(cause) = unsafe { self.files.install_in_child() } {
            use shards_ipc::child_fds::{Failure, Operation};
            // Close follows complete placement. Earlier Install failures leave
            // the high staged Ready object intact, even if original numbers moved.
            let report = if matches!(
                cause,
                Failure::Native {
                    operation: Operation::Close { .. },
                    ..
                }
            ) {
                Fd::Ready as RawFd
            } else {
                staged_ready
            };
            match cause {
                Failure::Native {
                    errno: Some(code), ..
                } => bootstrap_fail(report, Step::Helper, code),
                _ => bootstrap_control_fail(report, ControlFailure::Protocol(ControlProtocol::State).into()),
            }
        }
        let args = [
            c"shards-init".as_ptr(),
            c"managed-reaper".as_ptr(),
            std::ptr::null(),
        ];
        let env = [std::ptr::null::<libc::c_char>()];
        // SAFETY: exact installed CLOEXEC executable and literal prebuilt argv/env.
        unsafe {
            libc::syscall(
                libc::SYS_execveat,
                Fd::Binary as i32,
                c"".as_ptr(),
                args.as_ptr(),
                env.as_ptr(),
                libc::AT_EMPTY_PATH,
            );
        }
        bootstrap_fail(Fd::Ready as i32, Step::Helper, errno())
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
            receipts: receipts::Receipts::new(self.ready.0, self.helper_required),
            setup: Some(self.setup.0),
            state: State::Starting,
            native: self.native,
        };
        Ok(owned)
    }
}

impl Owned {
    pub fn group(&self) -> &std::fs::File {
        &self._group
    }

    pub(crate) fn bind_actor(&mut self, startup: &mut Startup<'_>) -> Result<(), StartupFailure> {
        loop {
            startup.progress()?;
            self.progress_start()?;
            if self.receipts.actor_bound() {
                return Ok(());
            }
            startup.wait(self, false)?;
        }
    }

    pub(crate) fn launched(&mut self, startup: &mut Startup<'_>) -> Result<(), StartupFailure> {
        if self.state != State::Starting {
            return Err(ControlFailure::Protocol(ControlProtocol::State).into());
        }
        let fd = self.setup.as_ref().ok_or(ControlFailure::Closed)?.as_raw_fd();
        let mut status = Status::new();
        loop {
            startup.progress()?;
            let closed = status.progress(fd)?;
            if closed {
                if !self.receipts.released() {
                    return Err(ControlFailure::Closed.into());
                }
                // Release and status closure are recorded, not inferred task
                // liveness or a matching wait. Those handles remain held below.
                self.receipts.running()?;
                self.setup = None;
                self.state = State::Running;
                return Ok(());
            }
            self.progress_start()?;
            startup.wait(self, true)?;
        }
    }

    fn progress_start(&mut self) -> Result<(), StartupFailure> {
        loop {
            self.progress_native()?;
            let installed = self.native.as_ref().is_none_or(native::Owner::installed);
            match self.receipts.progress(installed)? {
                receipts::Change::Pending => {
                    return self
                        .receipts
                        .launch_refusal()
                        .map_or(Ok(()), |cause| Err(cause.into()));
                }
                receipts::Change::Advanced => {}
                receipts::Change::ActorEnded => {
                    if let Some(native) = &mut self.native {
                        native.close(native::Close::ActorReaped);
                    }
                    if let Some(helper) = self.receipts.helper() {
                        signal_stop(helper).map_err(|error| {
                            ControlFailure::Native(error.raw_os_error().unwrap_or(libc::EIO))
                        })?;
                    }
                }
                receipts::Change::HelperEnded { raw } if !self.receipts.actor_ended() => {
                    return Err(ControlFailure::HelperExited { status: raw }.into());
                }
                receipts::Change::HelperEnded { .. } => {}
                receipts::Change::Refused { failure } => return Err(LaunchFailure::Reported(failure).into()),
            }
        }
    }

    pub fn native_control(&self) -> Option<RawFd> {
        self.native.as_ref()?.control().map(|fd| fd.as_raw_fd())
    }

    pub fn native_events(&self) -> libc::c_short {
        self.native.as_ref().map_or(0, native::Owner::events)
    }

    pub fn native_population_events(&self) -> impl Iterator<Item = (std::num::NonZeroU64, RawFd)> + '_ {
        self.native
            .iter()
            .flat_map(|owner| owner.population_events().map(|(id, fd)| (id, fd.as_raw_fd())))
    }

    pub fn native_population_changed(&mut self, id: std::num::NonZeroU64) -> Result<(), NativeFailure> {
        self.native
            .as_mut()
            .ok_or(NativeFailure::State)?
            .population_changed(id)
    }

    pub fn native_retention(&self) -> Result<Option<impl std::fmt::Display>, NativeFailure> {
        self.native.as_ref().map(native::Owner::retention).transpose()
    }

    /// Current authority remains a borrow of this SAME live lifecycle. Installed,
    /// decoded Context and copied Scope are bootstrap/data facts only.
    pub fn progress_native_runtime(&mut self) -> Result<(), NativeFailure> {
        match self.progress_native()? {
            native::Update::Observed => {}
            native::Update::Refused(refused) => {
                let _ = writeln!(std::io::stderr(), "{refused}");
            }
        }
        Ok(())
    }

    fn progress_native(&mut self) -> Result<native::Update, NativeFailure> {
        let Self {
            native,
            state,
            owner,
            receipts,
            _group,
            ..
        } = self;
        let Some(native) = native.as_mut() else {
            return Ok(native::Update::Observed);
        };
        native.progress(receipts, _group, |scope, context| {
            use shards_abi::run::native_grants::operation::Refusal;
            if context.epoch() != scope.epoch {
                return Err(Refusal::StaleScope);
            }
            if *state != State::Running
                || owner.is_none()
                || receipts.actor_ended()
                || receipts.launch_refusal().is_some()
            {
                return Err(Refusal::Closed);
            }
            Ok(())
        })
    }

    pub fn control(&self) -> Option<RawFd> {
        match self.state {
            State::Ended { .. } | State::Quarantined { .. } | State::Faulted { .. } => None,
            _ if self.receipts.listening() => Some(self.receipts.fd()),
            _ => None,
        }
    }

    pub fn control_events(&self) -> libc::c_short {
        self.receipts
            .events_with_native(self.native.as_ref().is_none_or(native::Owner::installed))
    }

    pub fn progress_control(&mut self) -> Result<Update, BoundaryFault> {
        if self.state == State::Starting {
            return Err(BoundaryFault::StatusConflict);
        }
        loop {
            let installed = self.native.as_ref().is_none_or(native::Owner::installed);
            let progress = match self.receipts.progress(installed) {
                Ok(progress) => progress,
                Err(cause) => return self.quarantine(QuarantineCause::UnobservedWait { cause }),
            };
            match progress {
                receipts::Change::Pending => return Ok(Update::Observed),
                receipts::Change::Advanced => {}
                receipts::Change::ActorEnded => {
                    if let Some(native) = &mut self.native {
                        native.close(native::Close::ActorReaped);
                    }
                    if let Some(helper) = self.receipts.helper()
                        && let Err(error) = signal_stop(helper)
                    {
                        return self.quarantine(QuarantineCause::UnobservedWait {
                            cause: ControlFailure::Native(error.raw_os_error().unwrap_or(libc::EIO)),
                        });
                    }
                }
                receipts::Change::HelperEnded { raw } if !self.receipts.actor_ended() => {
                    return self.quarantine(QuarantineCause::HelperExit { raw_status: raw });
                }
                receipts::Change::HelperEnded { .. } => {}
                // Refused launches never publish an Owned runtime object. A
                // caller entering runtime progress before launch is invalid.
                receipts::Change::Refused { .. } => return Err(BoundaryFault::StatusConflict),
            }
        }
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
        if let Some(native) = &mut self.native {
            native.close(native::Close::Lifecycle);
        }
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
        if let Some(native) = &mut self.native {
            native.close(native::Close::Lifecycle);
        }
        if !matches!(self.state, State::Quarantined { .. } | State::Faulted { .. }) {
            let actual = match self.receipts.reaper_status() {
                Ok(actual) => actual,
                Err(cause) => {
                    let update = self.quarantine(QuarantineCause::UnobservedWait { cause })?;
                    self.state = self.state.reaped(status)?;
                    return Ok(update);
                }
            };
            if status != actual {
                if self.state == State::Stopping {
                    // An owner-requested population kill can race an already
                    // received inner wait. Keep the distinct outer status too.
                    let update = self.quarantine(QuarantineCause::UnobservedWait {
                        cause: ControlFailure::Protocol(ControlProtocol::Conflict),
                    })?;
                    self.state = self.state.reaped(status)?;
                    return Ok(update);
                }
                let fault = BoundaryFault::StatusConflict;
                self.state = self.state.faulted(fault);
                return Err(fault);
            }
        }
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
        if let Some(native) = &mut self.native {
            native.close(native::Close::Lifecycle);
        }
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
        population_empty(&self.events)
    }

    fn kill_population(&self) -> io::Result<()> {
        kill_population(&self.kill)
    }
}

fn population_empty(events: &std::fs::File) -> io::Result<bool> {
    // cgroup.events contains boolean populated/frozen fields (cgroup-v2.rst).
    let mut bytes = [0; b"populated 1\nfrozen 1\n".len()];
    let n = loop {
        // SAFETY: pread(2) reads the held cgroup's event file at its start.
        let n = unsafe { libc::pread(events.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len(), 0) };
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

/// Borrow the actual protected run connection; this adds no owner or watcher.
#[derive(Debug)]
pub(crate) enum StartupFailure {
    Launch(LaunchFailure),
    Native(NativeFailure),
    Control(crate::run::SignalFailure),
}

impl From<NativeFailure> for StartupFailure {
    fn from(cause: NativeFailure) -> Self {
        Self::Native(cause)
    }
}

impl From<LaunchFailure> for StartupFailure {
    fn from(cause: LaunchFailure) -> Self {
        Self::Launch(cause)
    }
}

impl From<ControlFailure> for StartupFailure {
    fn from(cause: ControlFailure) -> Self {
        Self::Launch(cause.into())
    }
}

pub(crate) struct Startup<'a> {
    owner: &'a std::fs::File,
    control: crate::run::StartupSignal<'a>,
    context: Option<&'a shards_abi::run::management::Context>,
}

impl<'a> Startup<'a> {
    pub(crate) fn owner(
        owner: &'a std::fs::File,
        control: crate::run::StartupSignal<'a>,
        context: Option<&'a shards_abi::run::management::Context>,
    ) -> Self {
        Self {
            owner,
            control,
            context,
        }
    }

    pub(crate) fn context(&self) -> Option<&'a shards_abi::run::management::Context> {
        self.context
    }

    fn progress(&mut self) -> Result<(), StartupFailure> {
        self.control.progress().map_err(StartupFailure::Control)
    }

    fn wait(&mut self, scope: &Owned, status: bool) -> Result<(), StartupFailure> {
        let mut polls = [
            libc::pollfd {
                fd: scope.receipts.fd(),
                events: scope.control_events(),
                revents: 0,
            },
            libc::pollfd {
                fd: scope.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.owner.as_raw_fd(),
                events: libc::POLLRDHUP,
                revents: 0,
            },
            libc::pollfd {
                fd: if status {
                    scope.setup.as_ref().map_or(-1, AsRawFd::as_raw_fd)
                } else {
                    -1
                },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.control.signal().unwrap_or(-1),
                events: self.control.events(),
                revents: 0,
            },
            libc::pollfd {
                fd: self.control.main(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: scope.native_control().unwrap_or(-1),
                events: scope.native_events(),
                revents: 0,
            },
        ];
        control::wait(&mut polls, None)?;
        let [_, child, owner, _, _, _, _] = polls;
        if owner.revents & (libc::POLLHUP | libc::POLLRDHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(ControlFailure::OwnerGone.into());
        }
        if child.revents != 0 && !(scope.receipts.released() && scope.receipts.reaper_status().is_ok()) {
            return Err(ControlFailure::ReaperExited.into());
        }
        self.progress()?;
        Ok(())
    }
}

struct Status {
    bytes: [u8; shards_abi::run::lifecycle::FAILURE_BYTES],
    filled: usize,
}

impl Status {
    fn new() -> Self {
        Self {
            bytes: [0; shards_abi::run::lifecycle::FAILURE_BYTES],
            filled: 0,
        }
    }

    fn progress(&mut self, fd: RawFd) -> Result<bool, LaunchFailure> {
        loop {
            if self.filled == self.bytes.len() {
                return Err(Failure::decode(self.bytes).map_or(
                    ControlFailure::Protocol(ControlProtocol::State).into(),
                    LaunchFailure::Reported,
                ));
            }
            let rest = self
                .bytes
                .get_mut(self.filled..)
                .ok_or(ControlFailure::Protocol(ControlProtocol::State))?;
            // SAFETY: read writes only this remaining fixed status buffer; this
            // endpoint was made nonblocking by acquisition before clone.
            let n = unsafe { libc::read(fd, rest.as_mut_ptr().cast(), rest.len()) };
            if n == 0 {
                return if self.filled == 0 {
                    Ok(true)
                } else {
                    Err(ControlFailure::Protocol(ControlProtocol::Length).into())
                };
            }
            if n > 0 {
                self.filled = self
                    .filled
                    .checked_add(
                        usize::try_from(n).map_err(|_| ControlFailure::Protocol(ControlProtocol::Length))?,
                    )
                    .ok_or(ControlFailure::Protocol(ControlProtocol::Length))?;
                continue;
            }
            match errno() {
                libc::EINTR => {}
                libc::EAGAIN => return Ok(false),
                errno => return Err(ControlFailure::Native(errno).into()),
            }
        }
    }
}

fn associate_actor(ready: RawFd, owner: RawFd, actor: RawFd) -> Result<(), ControlFailure> {
    use control::{Kind, Packet, Received, Sent};
    let born = Packet::checked(Kind::ActorBorn, ACTOR, 0).map_err(ControlFailure::Codec)?;
    let mut sent = false;
    loop {
        if !sent {
            sent = control::send(ready, born, Some(actor))? == Sent::Complete;
        }
        if sent {
            match control::receive(ready)? {
                Received::Pending => {}
                Received::Message {
                    packet,
                    descriptor: None,
                } if born.matches_ack(packet) => return Ok(()),
                Received::Message { descriptor, .. } => {
                    if let Some(fd) = descriptor {
                        control::close(fd);
                    }
                    return Err(ControlFailure::Protocol(ControlProtocol::Conflict));
                }
            }
        }
        let mut polls = [
            libc::pollfd {
                fd: ready,
                events: if sent { libc::POLLIN } else { libc::POLLOUT },
                revents: 0,
            },
            // The startup word is deliberately unread until fresh exec; only
            // hangup/error on this existing pipe is relevant at this phase.
            libc::pollfd {
                fd: owner,
                events: 0,
                revents: 0,
            },
        ];
        control::wait(&mut polls, None)?;
        let [_, owner] = polls;
        if owner.revents != 0 {
            return Err(ControlFailure::OwnerGone);
        }
    }
}

fn checked_control_failure(error: LaunchFailure) -> Option<Failure> {
    error.canonical().ok()
}

pub(crate) fn raw_control_fail(fd: RawFd, error: LaunchFailure) -> ! {
    if let Some(failure) = checked_control_failure(error) {
        report_failure(fd, failure);
    }
    // SAFETY: post-clone failure must not run inherited Rust teardown.
    unsafe { libc::_exit(125) }
}

fn bootstrap_fail(fd: RawFd, step: Step, errno: i32) -> ! {
    let failure = Failure::native(step, errno)
        .map(LaunchFailure::Reported)
        .unwrap_or_else(|cause| ControlFailure::Codec(cause).into());
    bootstrap_control_fail(fd, failure)
}

fn bootstrap_control_fail(fd: RawFd, cause: LaunchFailure) -> ! {
    if let Some(failure) = checked_control_failure(cause)
        && let Ok(packet) = control::Packet::failed(ACTOR, failure)
    {
        // Nonblocking: an unavailable error queue retains Unknown at the owner.
        let _ = control::send(fd, packet, None);
    }
    // SAFETY: this trusted bootstrap cannot continue or run inherited teardown.
    unsafe { libc::_exit(125) }
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

#[cfg(test)]
fn helper_ready(fd: OwnedFd) -> Result<(), Failure> {
    let mut ready = std::fs::File::from(fd);
    let mut word = [0; 1 + size_of::<i32>()];
    ready.read_exact(&mut word).map_err(|_| protocol(Step::Helper))?;
    if word != [READY, 0, 0, 0, 0] {
        return Err(Failure::decode(word).unwrap_or(protocol(Step::Helper)));
    }
    Ok(())
}

#[cfg(test)]
fn actor_exec(fd: OwnedFd) -> Result<(), Failure> {
    let mut setup = std::fs::File::from(fd);
    let mut first = [0];
    let n = loop {
        match setup.read(&mut first) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(protocol(Step::Setup)),
            Ok(n) => break n,
        }
    };
    if n != 0 {
        let mut rest = [0; size_of::<i32>()];
        setup.read_exact(&mut rest).map_err(|_| protocol(Step::Setup))?;
        let [a, b, c, d] = rest;
        let [first] = first;
        return Err(Failure::decode([first, a, b, c, d]).unwrap_or(protocol(Step::Setup)));
    }
    Ok(())
}

#[cfg(test)]
fn gate_ready(fd: RawFd) -> bool {
    let mut word = [0];
    raw_read(fd, &mut word).is_ok() && word == [READY]
}

impl Drop for Owned {
    fn drop(&mut self) {
        if let Some(native) = &mut self.native {
            native.close(native::Close::Lifecycle);
        }
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

fn errno() -> i32 {
    // SAFETY: copy this Linux guest thread's actual errno. Checked canonical
    // construction refuses a missing/nonpositive code instead of inventing one.
    unsafe { *libc::__errno_location() }
}

pub fn raw_fail(fd: RawFd, step: Step) -> ! {
    let cause = Failure::native(step, errno())
        .map(LaunchFailure::Reported)
        .unwrap_or_else(|cause| ControlFailure::Codec(cause).into());
    raw_control_fail(fd, cause)
}

pub fn report_failure(fd: RawFd, failure: Failure) {
    let _ = raw_write(fd, &failure.encode());
}

fn raw_read(fd: RawFd, mut bytes: &mut [u8]) -> Result<(), ControlFailure> {
    while !bytes.is_empty() {
        // SAFETY: read(2) writes only the remaining local slice.
        let n = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
        if n < 0 {
            let code = errno();
            if code == libc::EINTR {
                continue;
            }
            return Err(ControlFailure::Native(code));
        }
        let n = usize::try_from(n).map_err(|_| ControlFailure::Protocol(ControlProtocol::Length))?;
        if n == 0 {
            return Err(ControlFailure::Closed);
        }
        bytes = bytes
            .get_mut(n..)
            .ok_or(ControlFailure::Protocol(ControlProtocol::Length))?;
    }
    Ok(())
}

fn raw_write(fd: RawFd, mut bytes: &[u8]) -> Result<(), ControlFailure> {
    while !bytes.is_empty() {
        // SAFETY: write(2) reads only the remaining borrowed slice.
        let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if n < 0 {
            let code = errno();
            if code == libc::EINTR {
                continue;
            }
            return Err(ControlFailure::Native(code));
        }
        let n = usize::try_from(n).map_err(|_| ControlFailure::Protocol(ControlProtocol::Length))?;
        if n == 0 {
            return Err(ControlFailure::Closed);
        }
        bytes = bytes
            .get(n..)
            .ok_or(ControlFailure::Protocol(ControlProtocol::Length))?;
    }
    Ok(())
}

pub fn reaper() -> ! {
    if std::process::id() != 1 {
        bootstrap_control_fail(
            Fd::Ready as i32,
            ControlFailure::Protocol(ControlProtocol::State).into(),
        );
    }
    let mut cap = [0; size_of::<u32>()];
    let mut required = [0];
    let mut split = [0];
    if let Err(cause) = raw_read(Fd::Owner as i32, &mut cap)
        .and_then(|()| raw_read(Fd::Owner as i32, &mut required))
        .and_then(|()| raw_read(Fd::Split as i32, &mut split))
    {
        bootstrap_control_fail(Fd::Ready as i32, cause.into());
    }
    if !matches!(required, [0] | [1]) || split != [SPLIT] {
        bootstrap_control_fail(
            Fd::Ready as i32,
            ControlFailure::Protocol(ControlProtocol::Conflict).into(),
        );
    }
    // SAFETY: the fresh single-threaded reaper owns this pipe read endpoint.
    let flags = unsafe { libc::fcntl(Fd::Owner as i32, libc::F_GETFL) };
    // SAFETY: preserve its native flags, adding only nonblocking liveness reads.
    if flags < 0 || unsafe { libc::fcntl(Fd::Owner as i32, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        bootstrap_fail(Fd::Ready as i32, Step::Helper, errno());
    }
    if !seal(u32::from_be_bytes(cap)) {
        bootstrap_fail(Fd::Ready as i32, Step::Helper, errno());
    }
    control::close(Fd::Split as i32);
    let result = reaper::serve(
        Fd::Owner as i32,
        Fd::Gate as i32,
        Fd::Ready as i32,
        required == [1],
    );
    match result {
        // SAFETY: normal exit is possible only after matching actual actor/helper
        // wait4 facts and their outer-owner ACKs; no Rust teardown is required.
        Ok(status) => unsafe { libc::_exit(status) },
        Err(error) => bootstrap_control_fail(Fd::Ready as i32, error.into()),
    }
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

const ALLOWED: [libc::c_long; 10] = [
    libc::SYS_read,
    libc::SYS_write,
    libc::SYS_sendmsg,
    libc::SYS_recvmsg,
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn admission_controls_move_to_launch_without_reopening_names() {
        let dir = shards_testdir::TempDir::new("controls-transfer").unwrap();
        let kill_path = dir.join("kill");
        let events_path = dir.join("events");
        let kill = std::fs::File::create(&kill_path).unwrap();
        std::fs::write(&events_path, b"populated 0\n").unwrap();
        let events = std::fs::File::open(&events_path).unwrap();
        let kill_fd = kill.as_raw_fd();
        let events_fd = events.as_raw_fd();
        let controls = Controls::held(kill, events);
        std::fs::remove_file(kill_path).unwrap();
        std::fs::remove_file(events_path).unwrap();
        let launch = Launch::new(0, controls, None).unwrap();
        assert_eq!(launch.kill.as_raw_fd(), kill_fd);
        assert_eq!(launch.events.as_raw_fd(), events_fd);
        let mut text = String::new();
        (&launch.events).read_to_string(&mut text).unwrap();
        assert_eq!(text, "populated 0\n");
        let originals = [
            (Fd::Owner, launch.owner.0.as_raw_fd()),
            (Fd::Gate, launch.gate.1.as_raw_fd()),
            (Fd::Split, launch.split.0.as_raw_fd()),
            (Fd::Ready, launch.ready.1.as_raw_fd()),
            (Fd::Binary, launch.binary.as_raw_fd()),
        ]
        .map(|(destination, original)| (destination as RawFd, original));
        assert_eq!(launch.files.descriptors(), originals.len());
        let staged: Vec<_> = originals
            .into_iter()
            .map(|(destination, original)| {
                let source = launch.files.staged_source(destination).unwrap().as_raw_fd();
                assert!(originals.iter().all(|(destination, _)| source > *destination));
                // SAFETY: compare the two live fixture-owned kernel objects only.
                let mut before: libc::stat = unsafe { std::mem::zeroed() };
                // SAFETY: initialized native output record for the live staged copy.
                let mut after: libc::stat = unsafe { std::mem::zeroed() };
                // SAFETY: fstat only inspects the two held descriptors.
                assert_eq!(unsafe { libc::fstat(original, &raw mut before) }, 0);
                // SAFETY: same native observation of this prepared staged descriptor.
                assert_eq!(unsafe { libc::fstat(source, &raw mut after) }, 0);
                assert_eq!((before.st_dev, before.st_ino), (after.st_dev, after.st_ino));
                source
            })
            .collect();
        drop(launch);
        for source in staged {
            // SAFETY: no new acquisition intervenes after the staging owner drops.
            assert_eq!(unsafe { libc::fcntl(source, libc::F_GETFD) }, -1);
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        }
        // SAFETY: F_GETFD only inspects this process's descriptor table. No new
        // descriptor has been acquired between drop and these observations.
        assert_eq!(unsafe { libc::fcntl(kill_fd, libc::F_GETFD) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        // SAFETY: the same descriptor-table observation for the events object.
        assert_eq!(unsafe { libc::fcntl(events_fd, libc::F_GETFD) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
    }

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
        let error = Failure::native(Step::Helper, libc::EPERM).unwrap();
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
            let error = Failure::native(step, libc::ENOENT).unwrap();
            assert_eq!(actor_exec(written(&error.encode())), Err(error));
            for cut in 1..error.encode().len() {
                assert_eq!(
                    actor_exec(written(&error.encode()[..cut])),
                    Err(protocol(Step::Setup))
                );
            }
        }
    }

    #[test]
    fn selected_mismatch_keeps_its_union_through_the_status_pipe() {
        let mismatch = Failure::SelectedObjectMismatch;
        assert_eq!(actor_exec(written(&mismatch.encode())), Err(mismatch));
        let pipe = written(&mismatch.encode());
        assert_eq!(
            Status::new().progress(pipe.as_raw_fd()),
            Err(LaunchFailure::Reported(mismatch))
        );
        assert_eq!(
            checked_control_failure(LaunchFailure::Reported(mismatch)),
            Some(mismatch)
        );
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
            let mut pidfd = -1;
            let mut args = crate::domains::CloneArgs {
                flags: libc::CLONE_PIDFD as u64,
                pidfd: (&raw mut pidfd) as u64,
                exit_signal: libc::SIGCHLD as u64,
                ..Default::default()
            };
            // SAFETY: a private fixture child; its branch calls only raw setup/pause/exit.
            let parent = unsafe { libc::getpid() };
            // SAFETY: clone3 writes its pidfd into this slot in the parent only.
            let pid = unsafe {
                libc::syscall(
                    libc::SYS_clone3,
                    &raw mut args,
                    size_of::<crate::domains::CloneArgs>(),
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
            assert!(pid > 0, "clone3: {}", io::Error::last_os_error());
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
            receipts: receipts::Receipts::new(crate::hostbind::seqpacket().unwrap().0, false),
            setup: None,
            state: State::Running,
            native: None,
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
