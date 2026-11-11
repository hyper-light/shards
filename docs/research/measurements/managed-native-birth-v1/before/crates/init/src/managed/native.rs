//! Actual owner-channel snapshot acquisition. The caller must be the protected
//! initial domains owner, borrowing its checked Domain/Prepared projection.
//! Successful acquisition/loading is not current per-effect or hard resource authority.

use std::fs::File;
use std::num::{NonZeroI32, NonZeroU64};
use std::os::fd::{AsFd as _, AsRawFd as _, FromRawFd as _, OwnedFd};

use shards_abi::run::{
    management::Context,
    native_grants::{self as wire, Bootstrap, Object, ObjectKind, Scope, operation as protocol},
};

use super::{channel, receipts};
use crate::domains::native_grants::{Failure as ProjectionFailure, HeldRoots, Projection};

mod jobs;
mod operations;

use jobs::{Job, Rejection};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    CreateSnapshot,
    WriteSnapshot,
    SealSnapshot,
    ReadSeals,
    StatSnapshot,
    CreateChannel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Binding,
    Length,
    Allocation,
    Projection(ProjectionFailure),
    Codec(wire::Error),
    Native { step: Step, errno: NonZeroI32 },
    InvalidNative { step: Step, actual: Option<i32> },
    WriteZero,
    Seals,
    Executable,
    Transport(channel::Failure),
    InitialBackpressure,
    Association(super::ControlFailure),
    State,
    Operation(protocol::Error),
    Policy(wire::policy::Error),
    Job(jobs::Failure),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "native owner preparation/control: {self:?}")
    }
}

impl std::error::Error for Failure {}

fn native(step: Step) -> Failure {
    // SAFETY: preserve this thread's actual Linux errno before another syscall.
    captured(step, Some(unsafe { *libc::__errno_location() }))
}

fn captured(step: Step, actual: Option<i32>) -> Failure {
    match actual.and_then(NonZeroI32::new).filter(|code| code.get() > 0) {
        Some(errno) => Failure::Native { step, errno },
        None => Failure::InvalidNative { step, actual },
    }
}

/// One actual queued startup transfer. It owns the original snapshot until the
/// real Loaded record and matching held helper association have both arrived.
pub(super) struct Preparation {
    channel: OwnedFd,
    snapshot: OwnedFd,
    policy: OwnedFd,
    policy_bytes: Vec<u8>,
    bytes: Vec<u8>,
    roots: HeldRoots,
    objects: Vec<Object>,
    member: u32,
    context: Context,
}

impl std::fmt::Debug for Preparation {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("NativePreparation")
            .field("snapshot_bytes", &self.bytes.len())
            .field("policy_bytes", &self.policy_bytes.len())
            .field("roots", &self.roots.count())
            .finish_non_exhaustive()
    }
}

impl Preparation {
    pub(super) fn acquire(
        projection: &Projection<'_>,
        scope: Scope,
        context: &Context,
        policy: &crate::domains::Filter,
    ) -> Result<(Self, OwnedFd), Failure> {
        if scope.epoch != context.epoch() {
            return Err(Failure::Binding);
        }
        let length = projection.encoded_len();
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(length).map_err(|_| Failure::Allocation)?;
        // The complete initialized region fits its actual acquired capacity.
        bytes.resize(length, 0);
        if projection.encode_into(&mut bytes).map_err(Failure::Projection)? != length {
            return Err(Failure::Length);
        }
        if wire::Snapshot::decode(&bytes)
            .map_err(Failure::Codec)?
            .header()
            .scope
            != scope
        {
            return Err(Failure::Binding);
        }
        let roots = projection.hold_roots().map_err(Failure::Projection)?;
        if roots.scope() != scope {
            return Err(Failure::Binding);
        }
        let mut objects = Vec::new();
        objects
            .try_reserve_exact(roots.count())
            .map_err(|_| Failure::Allocation)?;
        objects.resize(
            roots.count(),
            Object {
                device: 0,
                inode: 0,
                kind: ObjectKind::Directory,
            },
        );
        roots.objects_into(&mut objects).map_err(Failure::Projection)?;
        let snapshot = sealed_snapshot(&bytes)?;
        let (policy, policy_bytes) = sealed_policy(policy)?;
        let (channel, helper) = crate::hostbind::seqpacket()
            .map_err(|error| captured(Step::CreateChannel, error.raw_os_error()))?;
        let offer = Bootstrap::Offer {
            scope,
            bytes: u64::try_from(length).map_err(|_| Failure::Length)?,
        };
        let mut frame = [0; Bootstrap::MAX_BYTES];
        let encoded = offer.encode(&mut frame).map_err(Failure::Codec)?;
        match channel::send(
            channel.as_raw_fd(),
            frame.get(..encoded).ok_or(Failure::Length)?,
            Some(snapshot.as_raw_fd()),
        )
        .map_err(Failure::Transport)?
        {
            channel::Sent::Complete => {}
            // No child exists, and both endpoints remain private here. Dropping
            // them retires any queued rights; no constructor replay is attempted.
            channel::Sent::Pending => return Err(Failure::InitialBackpressure),
        }
        let policy_record = Bootstrap::Policy {
            scope,
            bytes: u64::try_from(policy_bytes.len()).map_err(|_| Failure::Length)?,
        };
        let encoded = policy_record.encode(&mut frame).map_err(Failure::Codec)?;
        match channel::send(
            channel.as_raw_fd(),
            frame.get(..encoded).ok_or(Failure::Length)?,
            Some(policy.as_raw_fd()),
        )
        .map_err(Failure::Transport)?
        {
            channel::Sent::Complete => {}
            channel::Sent::Pending => return Err(Failure::InitialBackpressure),
        }
        // Exactly one Context reference per actual declared member; the guest
        // supports only 64-bit targets and declared member indices are u32.
        let prepared = Self {
            channel,
            snapshot,
            policy,
            policy_bytes,
            bytes,
            roots,
            objects,
            member: scope.member,
            context: context.clone(),
        };
        Ok((prepared, helper))
    }

    pub(super) fn into_owner(self) -> Owner {
        Owner {
            channel: self.channel,
            _snapshot: self.snapshot,
            _policy: self.policy,
            policy_bytes: self.policy_bytes,
            bytes: self.bytes,
            roots: self.roots,
            objects: self.objects,
            member: self.member,
            context: self.context,
            phase: Phase::Offered,
            admission: Admission::Open,
            jobs: Vec::new(),
            pending: None,
        }
    }
}

fn sealed_policy(policy: &crate::domains::Filter) -> Result<(OwnedFd, Vec<u8>), Failure> {
    use wire::policy::{self, Instruction};
    let length = policy::encoded_len(policy.1.len()).map_err(Failure::Policy)?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length).map_err(|_| Failure::Allocation)?;
    bytes.resize(length, 0);
    let (flags, instructions) = bytes
        .split_at_mut_checked(policy::FLAGS_BYTES)
        .ok_or(Failure::Length)?;
    flags.copy_from_slice(&policy.0.to_le_bytes());
    for (output, instruction) in instructions
        .as_chunks_mut::<{ policy::INSTRUCTION_BYTES }>()
        .0
        .iter_mut()
        .zip(&policy.1)
    {
        output.copy_from_slice(
            &Instruction {
                code: instruction.code,
                jt: instruction.jt,
                jf: instruction.jf,
                k: instruction.k,
            }
            .encode(),
        );
    }
    policy::Program::decode(&bytes).map_err(Failure::Policy)?;
    Ok((sealed_snapshot(&bytes)?, bytes))
}

fn sealed_snapshot(bytes: &[u8]) -> Result<OwnedFd, Failure> {
    // Linux6.18 memfd.h: NOEXEC_SEAL removes execution and seals that mode;
    // ALLOW_SEALING permits the write/grow/shrink/seal barriers below.
    // SAFETY: fixed NUL-terminated diagnostic name and native ABI flags.
    let raw = unsafe {
        libc::memfd_create(
            c"shards-native-grants".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING | libc::MFD_NOEXEC_SEAL,
        )
    };
    if raw < 0 {
        return Err(native(Step::CreateSnapshot));
    }
    // SAFETY: memfd_create transferred one fresh descriptor owned only here.
    let snapshot = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut written = 0usize;
    while written < bytes.len() {
        let pending = bytes.get(written..).ok_or(Failure::Length)?;
        let offset = libc::off_t::try_from(written).map_err(|_| Failure::Length)?;
        // SAFETY: pwrite borrows this initialized byte suffix and held file.
        let count = unsafe { libc::pwrite(raw, pending.as_ptr().cast(), pending.len(), offset) };
        if count < 0 {
            let cause = native(Step::WriteSnapshot);
            if matches!(cause, Failure::Native { errno, .. } if errno.get() == libc::EINTR) {
                continue;
            }
            return Err(cause);
        }
        let count = usize::try_from(count).map_err(|_| Failure::Length)?;
        if count == 0 {
            return Err(Failure::WriteZero);
        }
        written = written
            .checked_add(count)
            .filter(|&end| end <= bytes.len())
            .ok_or(Failure::Length)?;
    }
    let required =
        libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL | libc::F_SEAL_EXEC;
    // SAFETY: this writable memfd is held alone and has no mappings.
    if unsafe { libc::fcntl(raw, libc::F_ADD_SEALS, required) } != 0 {
        return Err(native(Step::SealSnapshot));
    }
    // SAFETY: F_GET_SEALS reads this same held memfd's actual seal mask.
    let actual = unsafe { libc::fcntl(raw, libc::F_GET_SEALS) };
    if actual < 0 {
        return Err(native(Step::ReadSeals));
    }
    if actual & required != required {
        return Err(Failure::Seals);
    }
    // SAFETY: initialized output for the held memfd's native metadata.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat writes only this output record.
    if unsafe { libc::fstat(raw, &raw mut stat) } != 0 {
        return Err(native(Step::StatSnapshot));
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG || stat.st_mode & 0o111 != 0 {
        return Err(Failure::Executable);
    }
    if usize::try_from(stat.st_size).ok() != Some(bytes.len()) {
        return Err(Failure::Length);
    }
    Ok(snapshot)
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Offered,
    AwaitingAssociation(libc::pid_t),
    Installed,
    Disconnected { cause: Option<Failure> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Close {
    Lifecycle,
    ActorReaped,
    Boundary,
}

#[derive(Clone, Copy, Debug)]
enum Admission {
    Open,
    Closed(Close),
}

struct Pending {
    reply: protocol::Envelope<protocol::Reply>,
    // Borrow the SAME retained job directory when sending. Its lifetime was
    // published before this packet could become reachable by the helper.
    group: Option<NonZeroU64>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Retention {
    pub owner_bytes: usize,
    pub source_descriptors: usize,
    pub reported: protocol::PreparedFacts,
    pub cleanup_uncertain: usize,
    pub associated_births: usize,
    pub failure: Option<Failure>,
}

impl std::fmt::Display for Retention {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            out,
            "native retained known owner representation {} bytes, {} source descriptors; original helper reports {} bytes/{} descriptors; {} cleanup obligations/{} associated births; kernel backing unverified",
            self.owner_bytes,
            self.source_descriptors,
            self.reported.retained_bytes,
            self.reported.descriptors,
            self.cleanup_uncertain,
            self.associated_births
        )?;
        if let Some(cause) = self.failure {
            write!(out, "; retained boundary cause {cause}")?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct Refused {
    operation: NonZeroU64,
    cause: Rejection,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            out,
            "native operation {} refused: {:?}",
            self.operation, self.cause
        )
    }
}

#[derive(Debug)]
pub(super) enum Update {
    Observed,
    Refused(Refused),
}

pub(super) struct Owner {
    channel: OwnedFd,
    _snapshot: OwnedFd,
    _policy: OwnedFd,
    policy_bytes: Vec<u8>,
    bytes: Vec<u8>,
    roots: HeldRoots,
    objects: Vec<Object>,
    member: u32,
    context: Context,
    phase: Phase,
    admission: Admission,
    // Authoritative issued operations and terminal receipts, including retired
    // identities. Removing a job must not make its identifier reusable.
    jobs: Vec<Job>,
    // At most the response to the ONE consumed packet: reads stop until this
    // packet progresses on writable readiness. No extra response queue exists.
    pending: Option<Pending>,
}

impl std::fmt::Debug for Owner {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("NativeOwner")
            .field("phase", &self.phase)
            .field("admission", &self.admission)
            .field("operations", &self.jobs.len())
            .field("pending_reply", &self.pending.is_some())
            .finish_non_exhaustive()
    }
}

impl Owner {
    pub(super) fn scope(&self) -> Scope {
        Scope {
            epoch: self.context.epoch(),
            member: self.member,
        }
    }

    pub(super) fn control(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        if matches!(
            self.phase,
            Phase::Disconnected { .. } | Phase::AwaitingAssociation(_)
        ) {
            None
        } else {
            Some(self.channel.as_fd())
        }
    }

    pub(super) fn events(&self) -> libc::c_short {
        if self.pending.is_some() || self.jobs.iter().any(Job::terminal_pending) {
            libc::POLLOUT
        } else {
            libc::POLLIN
        }
    }

    pub(super) fn installed(&self) -> bool {
        matches!(self.phase, Phase::Installed)
    }

    /// Linearize closure before the lifecycle's stop/kill or early-return paths.
    /// Continue accepting original association/wait/terminal facts afterwards.
    pub(super) fn close(&mut self, cause: Close) {
        if matches!(self.admission, Admission::Open) {
            self.admission = Admission::Closed(cause);
        }
        for job in &mut self.jobs {
            job.close();
        }
        if let Some(pending) = &mut self.pending
            && pending.reply.message == protocol::Reply::Prepared
        {
            pending.reply.message = protocol::Reply::Revoked;
            pending.group = None;
        }
    }

    /// Actual acquired representation and separately labelled helper reports.
    /// These observations do not mint parent/kernel reservations or aggregate
    /// run rights. Retired identities/terminal receipts remain charged here.
    pub(super) fn retention(&self) -> Result<Retention, Failure> {
        let mut owner_bytes = size_of::<Self>();
        for bytes in [
            self.bytes.capacity(),
            self.policy_bytes.capacity(),
            self.objects
                .capacity()
                .checked_mul(size_of::<Object>())
                .ok_or(Failure::Length)?,
            self.jobs
                .capacity()
                .checked_mul(size_of::<Job>())
                .ok_or(Failure::Length)?,
        ] {
            owner_bytes = owner_bytes.checked_add(bytes).ok_or(Failure::Length)?;
        }
        let mut reported = protocol::PreparedFacts {
            retained_bytes: 0,
            descriptors: 0,
        };
        let mut cleanup_uncertain = 0usize;
        let mut associated_births = 0usize;
        for job in &self.jobs {
            let observed = job.retained();
            reported.retained_bytes = reported
                .retained_bytes
                .checked_add(observed.retained_bytes)
                .ok_or(Failure::Length)?;
            reported.descriptors = reported
                .descriptors
                .checked_add(observed.descriptors)
                .ok_or(Failure::Length)?;
            cleanup_uncertain = cleanup_uncertain
                .checked_add(usize::from(job.cleanup_failure().is_some()))
                .ok_or(Failure::Length)?;
            associated_births = associated_births
                .checked_add(usize::from(job.birth().is_some()))
                .ok_or(Failure::Length)?;
        }
        Ok(Retention {
            owner_bytes,
            source_descriptors: self.roots.count(),
            reported,
            cleanup_uncertain,
            associated_births,
            failure: match self.phase {
                Phase::Disconnected { cause } => cause,
                _ => None,
            },
        })
    }

    pub(super) fn population_events(
        &self,
    ) -> impl Iterator<Item = (NonZeroU64, std::os::fd::BorrowedFd<'_>)> {
        self.jobs
            .iter()
            .filter_map(|job| job.events().map(|fd| (job.id(), fd)))
    }

    /// Called only for this SAME held event object from the existing reactor.
    /// A real population notification is not a wait or a new admission check.
    pub(super) fn population_changed(&mut self, operation: NonZeroU64) -> Result<(), Failure> {
        self.job_mut(operation)?
            .population_changed()
            .map_err(Failure::Job)
    }

    /// The actual Owned caller supplies its current check under the SAME
    /// exclusive owner turn. No callback result or owner capability is stored.
    /// All I/O here is a bounded native operation; readiness waits live outside.
    pub(super) fn progress(
        &mut self,
        receipts: &receipts::Receipts,
        member: &File,
        mut current: impl FnMut(Scope, &Context) -> Result<(), protocol::Refusal>,
    ) -> Result<Update, Failure> {
        if receipts.actor_ended() {
            self.close(Close::ActorReaped);
        }
        let result = self.progress_inner(receipts, member, &mut current);
        if let Err(cause) = &result {
            self.close(Close::Boundary);
            self.phase = Phase::Disconnected { cause: Some(*cause) };
        }
        result
    }

    fn progress_inner(
        &mut self,
        receipts: &receipts::Receipts,
        member: &File,
        current: &mut impl FnMut(Scope, &Context) -> Result<(), protocol::Refusal>,
    ) -> Result<Update, Failure> {
        if let Phase::AwaitingAssociation(pid) = self.phase {
            if receipts.helper_association(pid).map_err(Failure::Association)? {
                self.phase = Phase::Installed;
            }
            return Ok(Update::Observed);
        }
        if matches!(self.phase, Phase::Disconnected { .. }) {
            return Ok(Update::Observed);
        }
        if self.pending.is_none()
            && let Some(operation) = self.jobs.iter().find(|job| job.terminal_pending()).map(Job::id)
        {
            self.queue(operation, protocol::Reply::Retired, None)?;
        }
        if self.pending.is_some() {
            return self.flush(current);
        }
        let mut bytes = [0; protocol::REQUEST_MAX];
        let received = match channel::receive(self.channel.as_raw_fd(), &mut bytes) {
            Ok(received) => received,
            Err(channel::Failure::Closed) => {
                self.close(Close::Boundary);
                self.phase = Phase::Disconnected {
                    cause: Some(Failure::Transport(channel::Failure::Closed)),
                };
                // No wait, population, terminal delivery or refund is inferred.
                return if receipts.actor_ended() {
                    Ok(Update::Observed)
                } else {
                    Err(Failure::Transport(channel::Failure::Closed))
                };
            }
            Err(cause) => return Err(Failure::Transport(cause)),
        };
        let channel::Received::Message { length, descriptor } = received else {
            return Ok(Update::Observed);
        };
        let bytes = bytes.get(..length).ok_or(Failure::Length)?;
        if matches!(self.phase, Phase::Offered) {
            if descriptor.is_some() {
                return Err(Failure::Transport(channel::Failure::Rights));
            }
            let Bootstrap::Loaded { scope, pid } = Bootstrap::decode(bytes).map_err(Failure::Codec)? else {
                return Err(Failure::State);
            };
            if scope != self.scope() {
                return Err(Failure::Binding);
            }
            self.phase = Phase::AwaitingAssociation(pid);
            if receipts.helper_association(pid).map_err(Failure::Association)? {
                self.phase = Phase::Installed;
            }
            return Ok(Update::Observed);
        }
        let request = protocol::Envelope::<protocol::Request>::decode(bytes).map_err(Failure::Operation)?;
        if request.scope != self.scope() {
            return Err(Failure::Binding);
        }
        if request.descriptor() != descriptor.is_some() {
            return Err(Failure::Transport(channel::Failure::Rights));
        }
        self.dispatch(request, descriptor, member, current)
    }

    fn dispatch(
        &mut self,
        request: protocol::Envelope<protocol::Request>,
        descriptor: Option<OwnedFd>,
        member: &File,
        current: &mut impl FnMut(Scope, &Context) -> Result<(), protocol::Refusal>,
    ) -> Result<Update, Failure> {
        use protocol::{Reply, Request};
        let operation = request.operation;
        let mut group = None;
        let reply = match request.message {
            Request::Prepare { effect, facts } => {
                if self.jobs.iter().any(|job| job.id() == operation) {
                    return Err(Failure::State);
                }
                if let Err(cause) = self.check_current(current) {
                    return self.refuse(operation, cause);
                }
                let selected = match self.select(effect) {
                    Ok(selected) => selected,
                    Err(cause) => return self.refuse(operation, cause),
                };
                self.jobs.try_reserve_exact(1).map_err(|_| Failure::Allocation)?;
                // This acquires actual state/objects. Reported PreparedFacts
                // remain observations, never an entitlement or backing promise.
                let job = match Job::acquire(operation, selected, facts, member) {
                    jobs::Acquisition::Prepared(job) => job,
                    jobs::Acquisition::Refused(cause) => return self.refuse(operation, cause),
                    jobs::Acquisition::Retained { job, cause } => {
                        self.jobs.push(job);
                        return self.refuse(operation, cause);
                    }
                };
                self.jobs.push(job);
                // Group acquisition/fstat are native I/O. Check the actual
                // original caller again before making any Prepared reachable.
                if let Err(cause) = self
                    .check_current(current)
                    .and_then(|()| self.select(effect).map(|_| ()))
                {
                    self.job_mut(operation)?.refuse(cause).map_err(Failure::Job)?;
                    return self.refuse(operation, cause);
                }
                group = self.job_mut(operation)?.publish().map_err(Failure::Job)?;
                Reply::Prepared
            }
            Request::Associate { namespace_pid } => {
                self.job_mut(operation)?
                    .associate(namespace_pid, descriptor.ok_or(Failure::State)?)
                    .map_err(Failure::Job)?;
                Reply::Associated { namespace_pid }
            }
            Request::Commit => {
                let effect = self.job(operation)?.effect();
                if self.job(operation)?.commitment() {
                    return Err(Failure::State);
                }
                if let Err(cause) = self.check_current(current) {
                    self.job_mut(operation)?.refuse(cause).map_err(Failure::Job)?;
                    return self.refuse(operation, cause);
                }
                self.job(operation)?.can_commit().map_err(Failure::Job)?;
                if let Err(cause) = self.select(effect).and_then(|selected| {
                    if self
                        .job(operation)
                        .map_err(|_| Rejection::Current(protocol::Refusal::State))?
                        .matches_selection(selected)
                    {
                        self.check_current(current)
                    } else {
                        Err(Rejection::Current(protocol::Refusal::Identity))
                    }
                }) {
                    self.job_mut(operation)?.refuse(cause).map_err(Failure::Job)?;
                    return self.refuse(operation, cause);
                }
                // No callback, released borrow, I/O, wait or allocation lies
                // between the final live check and this immutable acceptance.
                self.job_mut(operation)?.commit().map_err(Failure::Job)?;
                Reply::Committed
            }
            Request::Cancel => {
                let committed = self.job_mut(operation)?.cancel().map_err(Failure::Job)?;
                Reply::Cancelled { committed }
            }
            Request::Reaped { code, status } => {
                self.job_mut(operation)?
                    .reaped(code, status)
                    .map_err(Failure::Job)?;
                self.job_mut(operation)?.retire().map_err(Failure::Job)?
            }
            Request::NotBorn => {
                self.job_mut(operation)?.not_born().map_err(Failure::Job)?;
                self.job_mut(operation)?.retire().map_err(Failure::Job)?
            }
            Request::Complete {
                outcome,
                accepted_bytes,
            } => {
                self.job_mut(operation)?
                    .complete(outcome, accepted_bytes)
                    .map_err(Failure::Job)?;
                self.job_mut(operation)?.retire().map_err(Failure::Job)?
            }
        };
        self.queue(operation, reply, group)?;
        Ok(Update::Observed)
    }

    fn check_current(
        &self,
        current: &mut impl FnMut(Scope, &Context) -> Result<(), protocol::Refusal>,
    ) -> Result<(), Rejection> {
        if let Admission::Closed(cause) = self.admission {
            return Err(Rejection::Closed(cause));
        }
        current(self.scope(), &self.context).map_err(Rejection::Current)
    }

    fn select(&mut self, effect: protocol::Effect) -> Result<jobs::Selected, Rejection> {
        self.roots
            .objects_into(&mut self.objects)
            .map_err(Rejection::Source)?;
        let snapshot = wire::Snapshot::decode(&self.bytes).map_err(Rejection::Codec)?;
        let object = |ordinal| -> Result<Object, Rejection> {
            usize::try_from(ordinal)
                .ok()
                .and_then(|index| self.objects.get(index))
                .copied()
                .ok_or(Rejection::RootIndex(ordinal))
        };
        match effect {
            protocol::Effect::Process {
                executable_root,
                workdir_root,
            } => {
                let objects = [object(executable_root)?, object(workdir_root)?];
                operations::select(&snapshot, self.scope(), effect, &objects)
            }
            protocol::Effect::Read { root, .. } | protocol::Effect::Write { root, .. } => {
                operations::select(&snapshot, self.scope(), effect, &[object(root)?])
            }
        }
        .map(jobs::Selected::from_selection)
        .map_err(Rejection::Selection)
    }

    fn job(&self, operation: NonZeroU64) -> Result<&Job, Failure> {
        self.jobs
            .iter()
            .find(|job| job.id() == operation)
            .ok_or(Failure::State)
    }
    fn job_mut(&mut self, operation: NonZeroU64) -> Result<&mut Job, Failure> {
        self.jobs
            .iter_mut()
            .find(|job| job.id() == operation)
            .ok_or(Failure::State)
    }

    fn refuse(&mut self, operation: NonZeroU64, cause: Rejection) -> Result<Update, Failure> {
        self.queue(operation, protocol::Reply::Refused(cause.wire()), None)?;
        Ok(Update::Refused(Refused { operation, cause }))
    }

    fn queue(
        &mut self,
        operation: NonZeroU64,
        message: protocol::Reply,
        group: Option<NonZeroU64>,
    ) -> Result<(), Failure> {
        if self.pending.is_some() {
            return Err(Failure::State);
        }
        self.pending = Some(Pending {
            reply: protocol::Envelope {
                scope: self.scope(),
                operation,
                message,
            },
            group,
        });
        Ok(())
    }

    fn flush(
        &mut self,
        current: &mut impl FnMut(Scope, &Context) -> Result<(), protocol::Refusal>,
    ) -> Result<Update, Failure> {
        let pending = self.pending.as_ref().ok_or(Failure::State)?;
        let mut rejected = None;
        if pending.reply.message == protocol::Reply::Prepared {
            let operation = pending.reply.operation;
            let effect = self.job(operation)?.effect();
            if let Err(cause) = self
                .select(effect)
                .map(|_| ())
                .and_then(|()| self.check_current(current))
            {
                self.job_mut(operation)?.refuse(cause).map_err(Failure::Job)?;
                let pending = self.pending.as_mut().ok_or(Failure::State)?;
                pending.reply.message = protocol::Reply::Revoked;
                pending.group = None;
                rejected = Some(Update::Refused(Refused { operation, cause }));
            }
        }
        let pending = self.pending.as_ref().ok_or(Failure::State)?;
        let reply = pending.reply;
        let mut bytes = [0; protocol::REPLY_MAX];
        let length = reply.encode(&mut bytes).map_err(Failure::Operation)?;
        let descriptor = pending
            .group
            .map(|id| {
                self.job(id)
                    .and_then(|job| job.directory().map(|fd| fd.as_raw_fd()).map_err(Failure::Job))
            })
            .transpose()?;
        match channel::send(
            self.channel.as_raw_fd(),
            bytes.get(..length).ok_or(Failure::Length)?,
            descriptor,
        )
        .map_err(Failure::Transport)?
        {
            channel::Sent::Complete => {
                self.pending = None;
                if reply.message == protocol::Reply::Retired {
                    self.job_mut(reply.operation)?.terminal_delivered();
                }
            }
            channel::Sent::Pending => {}
        }
        Ok(rejected.unwrap_or(Update::Observed))
    }
}

#[cfg(test)]
mod tests;
