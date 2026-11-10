//! The single writer of a private run export. There is no public File+size mint.
//! The checked owner producer must establish the directory/name lifetime before
//! constructing this state; ordinary guest volumes never enter this path.
use std::fs::File;

use shards_abi::run::audit::{
    Cause, FileStep, Native, Platform, Reply, Request, Scope, Stage, StorageRequest,
};

use super::{Errno, host};

/// A slot's immutable call and two maxima, and each record's committed length.
/// Unused tails remain charged and allocated until owner deletion.
const SLOT_HEADER: u64 = 3 * size_of::<u64>() as u64;
const LENGTH_BYTES: u64 = size_of::<u64>() as u64;
pub(super) const SLOT_OVERHEAD: u64 = shards_abi::run::audit::SLOT_OVERHEAD;

#[derive(Debug)]
pub(super) struct Member {
    object: Object,
    scope: Scope,
    key: (u64, u64),
    lease: Option<Lease>,
    life: Life,
}
#[derive(Debug, Clone, Copy)]
enum Life {
    Unopened,
    Held,
    Closed,
    Failed {
        cause: Cause,
        native: Option<Native>,
        step: FileStep,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Lease {
    call: u64,
    intent: u64,
    terminal: u64,
    intent_offset: u64,
    terminal_offset: u64,
    stage: Stage,
    accepted: u64,
}

#[derive(Debug)]
enum Object {
    Available(File),
    InFlight {
        operation: Operation,
        disposition: Disposition,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    Continue,
    CloseRequested,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Operation {
    lease: Option<Lease>,
    kind: Kind,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Reserve { start: u64, charge: u64 },
    Append { offset: u64, count: u64, terminal: bool },
    Commit { terminal: bool },
    Sync,
}

pub(super) enum Action<'a> {
    Control(Request),
    Write { offset: u64, bytes: &'a [u8] },
    Sync,
}
pub(super) enum Answer {
    Control(Reply),
    Written(usize),
    Synced,
}
pub(super) enum Prepared<'a> {
    Ready(Answer),
    Io(IoTicket<'a>),
}
/// The same held file moves here; no duplicate descriptor or second lease owner.
/// Dropping an unfinished ticket leaves retained InFlight uncertainty in Run.
#[must_use]
pub(super) struct IoTicket<'a> {
    file: File,
    member: usize,
    key: (u64, u64),
    operation: Operation,
    bytes: &'a [u8],
}
pub(super) struct Completion<'a> {
    ticket: IoTicket<'a>,
    result: Result<usize, FileFailure>,
}
pub(super) enum Finalized<'a> {
    Recorded(Result<Answer, Errno>),
    Unmatched(Completion<'a>),
}
struct FileFailure {
    cause: Cause,
    native: Option<Native>,
    step: FileStep,
    accepted: u64,
}
impl FileFailure {
    fn io(step: FileStep, native: Native) -> Self {
        Self {
            cause: Cause::Io,
            native: Some(native),
            step,
            accepted: 0,
        }
    }
}
impl<'a> IoTicket<'a> {
    pub(super) fn perform(self) -> Completion<'a> {
        let result = self.perform_file();
        Completion { ticket: self, result }
    }
    fn perform_file(&self) -> Result<usize, FileFailure> {
        let file = &self.file;
        match self.operation.kind {
            Kind::Reserve { start, charge } => {
                let lease = self
                    .operation
                    .lease
                    .ok_or_else(|| FileFailure::io(FileStep::ReserveAllocate, native(None)))?;
                crate::platform::reserve_file_extent(file, start, charge)
                    .map_err(|e| FileFailure::io(FileStep::ReserveAllocate, native(e.raw_os_error())))?;
                let mut header = [0; SLOT_HEADER as usize];
                let mut out = header.as_mut_slice();
                for number in [lease.call, lease.intent, lease.terminal] {
                    let (field, rest) = out
                        .split_first_chunk_mut::<8>()
                        .ok_or_else(|| FileFailure::io(FileStep::HeaderAppend, native(None)))?;
                    *field = number.to_be_bytes();
                    out = rest;
                }
                exact(file, &header, start).map_err(|e| FileFailure::io(FileStep::HeaderAppend, e))?;
                sync(file).map_err(|e| FileFailure::io(FileStep::ReserveSync, e))?;
            }
            Kind::Append { offset, terminal, .. } => {
                let step = if terminal {
                    FileStep::TerminalAppend
                } else {
                    FileStep::IntentAppend
                };
                let n = write_once(file, self.bytes, offset).map_err(|e| FileFailure::io(step, e))?;
                if n != self.bytes.len() {
                    return Err(FileFailure {
                        cause: Cause::Partial,
                        native: None,
                        step,
                        accepted: u64::try_from(n).unwrap_or(u64::MAX),
                    });
                }
                return Ok(n);
            }
            Kind::Commit { terminal } => {
                let (length_step, sync_step) = if terminal {
                    (FileStep::TerminalLength, FileStep::TerminalSync)
                } else {
                    (FileStep::IntentLength, FileStep::IntentSync)
                };
                let lease = self
                    .operation
                    .lease
                    .ok_or_else(|| FileFailure::io(length_step, native(None)))?;
                let offset = if terminal {
                    lease.terminal_offset
                } else {
                    lease.intent_offset
                };
                let prefix = offset
                    .checked_sub(LENGTH_BYTES)
                    .ok_or_else(|| FileFailure::io(length_step, native(None)))?;
                exact(file, &lease.accepted.to_be_bytes(), prefix)
                    .map_err(|e| FileFailure::io(length_step, e))?;
                sync(file).map_err(|e| FileFailure::io(sync_step, e))?;
            }
            Kind::Sync => sync(file).map_err(|e| FileFailure::io(FileStep::FileSync, e))?,
        }
        Ok(0)
    }
}

#[derive(Debug)]
pub(super) struct Run {
    members: Vec<Member>,
    remaining: u64,
    session: Session,
}
#[derive(Debug, Clone, Copy)]
enum Session {
    Fresh,
    Active,
    Closed,
}

#[derive(Debug, Clone, Copy)]
pub enum CreateCause {
    Capacity,
    Allocation,
    Filesystem(Errno),
    ExistingObject,
    Io { step: FileStep, native: Native },
    Encoding,
}
/// Partial objects and their charges survive refusal. No implicit owner deletion.
#[derive(Debug)]
pub struct CreateFailure {
    pub cause: CreateCause,
    retained: Run,
}
#[derive(Debug)]
pub enum StartFailure {
    Directory(String),
    Writer(CreateFailure),
}
impl std::fmt::Display for CreateCause {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Capacity => out.write_str("journal byte-extent capacity exhausted"),
            Self::Allocation => out.write_str("writer state storage unavailable"),
            Self::Filesystem(errno) => write!(out, "filesystem operation (Linux errno {errno})"),
            Self::ExistingObject => out.write_str("journal name already exists; retained state is unknown"),
            Self::Io { step, native: cause } => write!(
                out,
                "held file {step:?} ({:?}, OS {:?})",
                cause.platform, cause.code
            ),
            Self::Encoding => out.write_str("journal header encoding failed"),
        }
    }
}
impl std::fmt::Display for CreateFailure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            out,
            "{}; {} retained objects, {} byte-extent capacity remaining",
            self.cause,
            self.retained.members.len(),
            self.retained.remaining
        )
    }
}
impl std::fmt::Display for StartFailure {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Directory(error) => write!(out, "private directory: {error}"),
            Self::Writer(error) => error.fmt(out),
        }
    }
}
impl StartFailure {
    pub fn status(&self) -> shards_abi::run::audit::ExportStatus {
        use shards_abi::run::audit::{ExportCause, ExportStatus, Retained};
        let (cause, retained) = match self {
            Self::Directory(_) => (ExportCause::Directory, Retained::Unknown),
            Self::Writer(error) => (
                match error.cause {
                    CreateCause::Capacity => ExportCause::Capacity,
                    CreateCause::Allocation => ExportCause::Allocation,
                    CreateCause::Filesystem(errno) => ExportCause::Filesystem(errno),
                    CreateCause::ExistingObject => ExportCause::ExistingObject,
                    CreateCause::Io { step, native } => ExportCause::Io { step, native },
                    CreateCause::Encoding => ExportCause::Encoding,
                },
                if matches!(error.cause, CreateCause::ExistingObject) {
                    Retained::Unknown
                } else {
                    error.retained.retained()
                },
            ),
        };
        ExportStatus::Refused { cause, retained }
    }
}

fn raw(file: &File) -> host::Raw {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        file.as_raw_fd()
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        file.as_raw_handle()
    }
}
fn native(code: Option<i32>) -> Native {
    Native {
        #[cfg(target_os = "linux")]
        platform: Platform::Linux,
        #[cfg(target_os = "macos")]
        platform: Platform::Darwin,
        #[cfg(windows)]
        platform: Platform::Windows,
        code,
    }
}
fn write_once(file: &File, bytes: &[u8], offset: u64) -> Result<usize, Native> {
    // SAFETY: a live immutable buffer and checked native offset; one operation,
    // including EINTR/short writes, never an automatic append retry.
    unsafe { crate::platform::write_at(file, bytes.as_ptr(), bytes.len(), offset) }
        .map_err(|error| native(error.raw_os_error()))
}
fn exact(file: &File, bytes: &[u8], offset: u64) -> Result<(), Native> {
    match write_once(file, bytes, offset)? {
        n if n == bytes.len() => Ok(()),
        _ => Err(native(None)),
    }
}
fn sync(file: &File) -> Result<(), Native> {
    crate::platform::sync_reserved_file(file).map_err(|error| native(error.raw_os_error()))
}

impl Member {
    /// Internal only: the producer supplies the already durably named, held
    /// regular object and immutable checked scope. It cannot be selected by wire.
    pub(super) fn held(file: File, scope: Scope) -> Result<Self, Errno> {
        let stat = super::stat_fd(raw(&file))?;
        if super::mode_of(&stat) & super::S_IFMT != super::S_IFREG || stat.nlink != 1 {
            return Err(super::EINVAL);
        }
        Ok(Self {
            object: Object::Available(file),
            scope,
            key: super::key(&stat),
            lease: None,
            life: Life::Unopened,
        })
    }
    fn reply(&self, remaining: u64, cause: Cause) -> Reply {
        let cause = match (cause, self.pending()) {
            (Cause::None, Some(Disposition::Continue)) => Cause::InFlight,
            (Cause::None, Some(Disposition::CloseRequested)) => Cause::Closed,
            (cause, _) => cause,
        };
        let (stage, cause, native, failure) = match self.life {
            Life::Unopened | Life::Held => (
                self.lease.map_or(Stage::Empty, |lease| lease.stage),
                cause,
                None,
                None,
            ),
            Life::Closed => (Stage::Closed, cause, None, None),
            Life::Failed { cause, native, step } => (Stage::Failed, cause, native, Some(step)),
        };
        let mut reply = Reply {
            stage,
            cause,
            call: 0,
            intent: 0,
            terminal: 0,
            intent_offset: 0,
            terminal_offset: 0,
            remaining,
            accepted: self.lease.map_or(0, |lease| lease.accepted),
            scope: self.scope,
            native,
            failure,
        };
        if let Some(lease) = self.lease {
            reply.call = lease.call;
            reply.intent = lease.intent;
            reply.terminal = lease.terminal;
            reply.intent_offset = lease.intent_offset;
            reply.terminal_offset = lease.terminal_offset;
        }
        reply
    }
    fn file(&self) -> Result<&File, Errno> {
        match &self.object {
            Object::Available(file) => Ok(file),
            Object::InFlight { .. } => Err(super::EIO),
        }
    }
    fn close(&mut self) {
        match &mut self.object {
            Object::InFlight { disposition, .. } => *disposition = Disposition::CloseRequested,
            Object::Available(_) if !matches!(self.life, Life::Failed { .. }) => self.life = Life::Closed,
            Object::Available(_) => {}
        }
    }
    fn pending(&self) -> Option<Disposition> {
        match self.object {
            Object::InFlight { disposition, .. } => Some(disposition),
            Object::Available(_) => None,
        }
    }
}

impl Run {
    fn retained(&self) -> shards_abi::run::audit::Retained {
        match u32::try_from(self.members.len()) {
            Ok(objects) => shards_abi::run::audit::Retained::Known {
                remaining: self.remaining,
                objects,
            },
            Err(_) => shards_abi::run::audit::Retained::Unknown,
        }
    }
    pub(super) fn readiness(
        &self,
    ) -> Result<shards_abi::run::audit::ExportStatus, shards_abi::run::audit::Error> {
        Ok(shards_abi::run::audit::ExportStatus::Ready {
            remaining: self.remaining,
            objects: u32::try_from(self.members.len()).map_err(|_| shards_abi::run::audit::Error::Bounds)?,
        })
    }
    /// Private placement implementation. The public checked owner issuer/caller
    /// is deliberately not minted here from a File and an arbitrary size.
    pub(super) fn create(
        root: &host::Owned,
        config: &shards_abi::run::audit::ExportConfig,
    ) -> Result<Self, CreateFailure> {
        let storage = config.storage();
        let mut run = Self {
            members: Vec::new(),
            remaining: storage.bytes(),
            session: Session::Fresh,
        };
        let result = run.create_members(root, storage, config.scopes());
        match result {
            Ok(()) => Ok(run),
            Err(cause) => Err(CreateFailure { cause, retained: run }),
        }
    }
    fn create_members(
        &mut self,
        root: &host::Owned,
        storage: StorageRequest,
        scopes: &[Scope],
    ) -> Result<(), CreateCause> {
        self.members
            .try_reserve_exact(scopes.len())
            .map_err(|_| CreateCause::Allocation)?;
        let header_bytes = shards_abi::run::audit::JOURNAL_HEADER_BYTES;
        for &scope in scopes {
            self.remaining = self
                .remaining
                .checked_sub(header_bytes as u64)
                .ok_or(CreateCause::Capacity)?;
            // An immutable member index as eight lowercase hexadecimal digits;
            // no actor pathname, label, scope bytes or credential enters a name.
            let name = scope.member_name();
            let name = std::ffi::CStr::from_bytes_with_nul(&name).map_err(|_| CreateCause::Encoding)?;
            use host::AsRaw as _;
            let (fd, _) = super::open_regular(
                root.raw(),
                name,
                host::O_RDWR | host::O_CREAT | host::O_EXCL | host::O_NOFOLLOW | host::O_CLOEXEC,
                0o600,
            )
            .map_err(|errno| {
                if errno == super::EEXIST {
                    CreateCause::ExistingObject
                } else {
                    CreateCause::Filesystem(errno)
                }
            })?;
            let member = Member::held(File::from(fd), scope).map_err(CreateCause::Filesystem)?;
            self.members.push(member);
            let member = self.members.last().ok_or(CreateCause::Encoding)?;
            crate::platform::reserve_file_extent(
                member.file().map_err(CreateCause::Filesystem)?,
                0,
                header_bytes as u64,
            )
            .map_err(|error| CreateCause::Io {
                step: FileStep::ReserveAllocate,
                native: native(error.raw_os_error()),
            })?;
            let header = shards_abi::run::audit::JournalHeader { scope, storage }
                .encode()
                .map_err(|_| CreateCause::Encoding)?;
            exact(member.file().map_err(CreateCause::Filesystem)?, &header, 0).map_err(|native| {
                CreateCause::Io {
                    step: FileStep::HeaderAppend,
                    native,
                }
            })?;
            sync(member.file().map_err(CreateCause::Filesystem)?).map_err(|native| CreateCause::Io {
                step: FileStep::ReserveSync,
                native,
            })?;
        }
        // This is the actual directory barrier, not FUSE FLUSH/SYNCFS. The
        // caller must also durably establish this private directory's own name.
        use host::AsRaw as _;
        super::durable(root.raw()).map_err(CreateCause::Filesystem)
    }
    pub(super) fn begin(&mut self) {
        match self.session {
            Session::Fresh => self.session = Session::Active,
            Session::Active | Session::Closed => {
                self.close_all();
                self.session = Session::Closed;
            }
        }
    }
    fn member(&self, key: (u64, u64)) -> Option<usize> {
        self.members.iter().position(|member| member.key == key)
    }
    pub(super) fn contains(&self, key: (u64, u64)) -> bool {
        self.member(key).is_some()
    }
    pub(super) fn accepts_name(&self, name: &std::ffi::CStr) -> bool {
        if name.to_bytes().len() != 2 * size_of::<u32>() {
            return false;
        }
        let member = name.to_bytes().iter().try_fold(0u32, |value, &digit| {
            let digit = match digit {
                b'0'..=b'9' => u32::from(digit - b'0'),
                b'a'..=b'f' => u32::from(digit - b'a' + 10),
                _ => return None,
            };
            value.checked_mul(16)?.checked_add(digit)
        });
        member.is_some_and(|id| self.members.iter().any(|member| member.scope.member == id))
    }
    pub(super) fn open(&mut self, key: (u64, u64)) -> Result<(), Errno> {
        let member = self
            .members
            .iter_mut()
            .find(|member| member.key == key)
            .ok_or(super::EPERM)?;
        if !matches!(member.life, Life::Unopened) {
            return Err(super::EPERM);
        }
        member.life = Life::Held;
        Ok(())
    }
    pub(super) fn close(&mut self, key: (u64, u64)) {
        if let Some(member) = self.members.iter_mut().find(|member| member.key == key) {
            member.close();
        }
    }
    pub(super) fn close_all(&mut self) {
        self.session = Session::Closed;
        for member in &mut self.members {
            member.close();
        }
    }
    pub(super) fn prepare<'a>(&mut self, key: (u64, u64), action: Action<'a>) -> Result<Prepared<'a>, Errno> {
        let index = self.member(key).ok_or(super::EPERM)?;
        let member = self.members.get_mut(index).ok_or(super::EPERM)?;
        let (kind, bytes) = match action {
            Action::Control(request) => {
                let kind = match Self::control(member, &mut self.remaining, request) {
                    Ok(kind) => kind,
                    Err(cause) => {
                        return Ok(Prepared::Ready(Answer::Control(
                            member.reply(self.remaining, cause),
                        )));
                    }
                };
                (kind, &[][..])
            }
            Action::Write { offset, bytes } => {
                if !matches!(member.life, Life::Held) || member.pending().is_some() {
                    return Err(super::EIO);
                }
                let lease = member.lease.ok_or(super::EPERM)?;
                let (base, maximum, terminal) = match lease.stage {
                    Stage::Reserved | Stage::IntentWritten => (lease.intent_offset, lease.intent, false),
                    Stage::IntentSynced | Stage::TerminalWritten => {
                        (lease.terminal_offset, lease.terminal, true)
                    }
                    _ => return Err(super::EPERM),
                };
                let count = u64::try_from(bytes.len()).map_err(|_| super::EINVAL)?;
                let end = lease.accepted.checked_add(count).ok_or(super::EINVAL)?;
                if count == 0 || end > maximum || base.checked_add(lease.accepted) != Some(offset) {
                    return Err(super::EPERM);
                }
                (
                    Kind::Append {
                        offset,
                        count,
                        terminal,
                    },
                    bytes,
                )
            }
            Action::Sync => {
                if !matches!(member.life, Life::Held) || member.pending().is_some() {
                    return Err(super::EIO);
                }
                (Kind::Sync, &[][..])
            }
        };
        let operation = Operation {
            lease: member.lease,
            kind,
        };
        let pending = Object::InFlight {
            operation,
            disposition: Disposition::Continue,
        };
        let file = match std::mem::replace(&mut member.object, pending) {
            Object::Available(file) => file,
            inflight @ Object::InFlight { .. } => {
                member.object = inflight;
                return Err(super::EIO);
            }
        };
        Ok(Prepared::Io(IoTicket {
            file,
            member: index,
            key,
            operation,
            bytes,
        }))
    }
    /// An unmatched completion retains its file and result for the owning caller.
    pub(super) fn finish<'a>(&mut self, completion: Completion<'a>) -> Finalized<'a> {
        let Some(member) = self.members.get_mut(completion.ticket.member) else {
            return Finalized::Unmatched(completion);
        };
        let disposition = match &member.object {
            Object::InFlight {
                operation,
                disposition,
            } if member.key == completion.ticket.key
                && *operation == completion.ticket.operation
                && member.lease == operation.lease =>
            {
                *disposition
            }
            _ => return Finalized::Unmatched(completion),
        };
        let Completion { ticket, result } = completion;
        let kind = ticket.operation.kind;
        member.object = Object::Available(ticket.file);
        let mut count = 0;
        match result {
            Ok(n) => {
                count = n;
                if let Some(lease) = member.lease.as_mut() {
                    match kind {
                        Kind::Append { count, terminal, .. } => {
                            // The same checked operation bounds this addition.
                            match lease.accepted.checked_add(count) {
                                Some(n) => lease.accepted = n,
                                None => {
                                    member.life = Life::Failed {
                                        cause: Cause::Io,
                                        native: Some(native(None)),
                                        step: if terminal {
                                            FileStep::TerminalAppend
                                        } else {
                                            FileStep::IntentAppend
                                        },
                                    };
                                }
                            }
                            lease.stage = if terminal {
                                Stage::TerminalWritten
                            } else {
                                Stage::IntentWritten
                            };
                        }
                        Kind::Commit { terminal } => {
                            lease.stage = if terminal {
                                Stage::TerminalSynced
                            } else {
                                Stage::IntentSynced
                            };
                            lease.accepted = 0;
                        }
                        Kind::Reserve { .. } | Kind::Sync => {}
                    }
                }
            }
            Err(failure) => {
                if let Some(lease) = member.lease.as_mut() {
                    lease.accepted = lease.accepted.saturating_add(failure.accepted);
                }
                member.life = Life::Failed {
                    cause: failure.cause,
                    native: failure.native,
                    step: failure.step,
                };
            }
        }
        if disposition == Disposition::CloseRequested {
            member.close();
        }
        let cause = if disposition == Disposition::CloseRequested {
            Cause::Closed
        } else {
            Cause::None
        };
        Finalized::Recorded(match kind {
            Kind::Reserve { .. } | Kind::Commit { .. } => {
                Ok(Answer::Control(member.reply(self.remaining, cause)))
            }
            Kind::Append { .. } if matches!(member.life, Life::Held) => Ok(Answer::Written(count)),
            Kind::Sync if matches!(member.life, Life::Held) => Ok(Answer::Synced),
            Kind::Append { .. } | Kind::Sync => Err(super::EIO),
        })
    }
    /// Ok is one concrete disk operation; Err is a typed immediate control result.
    fn control(member: &mut Member, remaining: &mut u64, request: Request) -> Result<Kind, Cause> {
        if matches!(request, Request::Identify) {
            return Err(Cause::None);
        }
        if let Request::Reserve {
            call,
            intent,
            terminal,
        } = request
        {
            return Self::reserve(member, remaining, call, intent, terminal);
        }
        match (member.lease, request.call()) {
            (Some(lease), Some(call)) if call == lease.call => match request {
                Request::Inspect { .. } => Err(Cause::None),
                Request::Close { .. } => {
                    member.close();
                    Err(Cause::None)
                }
                Request::CommitIntent { .. } | Request::CommitTerminal { .. }
                    if matches!(member.life, Life::Held) =>
                {
                    if member.pending().is_some() {
                        return Err(Cause::None);
                    }
                    let terminal = matches!(request, Request::CommitTerminal { .. });
                    match (terminal, lease.stage) {
                        (false, Stage::IntentSynced | Stage::TerminalWritten | Stage::TerminalSynced)
                        | (true, Stage::TerminalSynced) => Err(Cause::None),
                        (false, Stage::IntentWritten) | (true, Stage::TerminalWritten) => {
                            Ok(Kind::Commit { terminal })
                        }
                        _ => Err(Cause::Order),
                    }
                }
                _ => Err(Cause::Closed),
            },
            (Some(_), _) => Err(Cause::Stale),
            (None, _) => Err(Cause::Order),
        }
    }
    fn reserve(
        member: &mut Member,
        remaining: &mut u64,
        call: u64,
        intent: u64,
        terminal: u64,
    ) -> Result<Kind, Cause> {
        if !matches!(member.life, Life::Held) {
            return Err(Cause::Closed);
        }
        if let Some(lease) = member.lease {
            if call == lease.call {
                return Err(if (intent, terminal) == (lease.intent, lease.terminal) {
                    Cause::None
                } else {
                    Cause::Conflict
                });
            }
            if call < lease.call {
                return Err(Cause::Stale);
            }
            if member.pending().is_some() || lease.stage != Stage::TerminalSynced {
                return Err(Cause::Order);
            }
        }
        if intent == 0 || terminal == 0 {
            return Err(Cause::Order);
        }
        let charge = intent
            .checked_add(terminal)
            .and_then(|n| n.checked_add(SLOT_OVERHEAD))
            .ok_or(Cause::Overflow)?;
        let left = remaining.checked_sub(charge).ok_or(Cause::Exhausted)?;
        // The sole writer's last checked lease defines its next extent. The
        // actual allocation adapter independently checks the held file's EOF.
        let start = match member.lease {
            Some(lease) => lease
                .terminal_offset
                .checked_add(lease.terminal)
                .ok_or(Cause::Overflow)?,
            None => shards_abi::run::audit::JOURNAL_HEADER_BYTES as u64,
        };
        let intent_offset = start
            .checked_add(SLOT_HEADER + LENGTH_BYTES)
            .ok_or(Cause::Overflow)?;
        let terminal_offset = intent_offset
            .checked_add(intent)
            .and_then(|n| n.checked_add(LENGTH_BYTES))
            .ok_or(Cause::Overflow)?;
        terminal_offset
            .checked_add(terminal)
            .filter(|&end| i64::try_from(end).is_ok())
            .ok_or(Cause::Overflow)?;
        *remaining = left;
        member.lease = Some(Lease {
            call,
            intent,
            terminal,
            intent_offset,
            terminal_offset,
            stage: Stage::Reserved,
            accepted: 0,
        });
        Ok(Kind::Reserve { start, charge })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::virtio::fs::server::{
        Server,
        tests::{answer, name, req, root_of},
    };
    use shards_abi::run::audit as wire;

    fn scope(member: u32) -> Scope {
        Scope {
            owner: [11; 32],
            run: [12; 32],
            generation: 13,
            member,
            policy: [14; 32],
        }
    }
    fn fixture(bytes: u64) -> (shards_testdir::TempDir, Server) {
        let dir = shards_testdir::TempDir::new("audit-writer").unwrap();
        let root = root_of(&dir);
        let config = wire::ExportConfig::new(
            StorageRequest::new(bytes, wire::Retention::UntilOwnerDeletion).unwrap(),
            vec![scope(0), scope(2)],
        )
        .unwrap();
        (dir, Server::audited(root, &config, 0).unwrap())
    }
    fn opened(server: &Server, member: u32) -> (u64, u64) {
        let (error, found) = answer(
            server,
            &req(super::super::op::LOOKUP, 1, 0, &name(&format!("{member:08x}"))),
        );
        assert_eq!(error, 0);
        let node = u64::from_le_bytes(found[..8].try_into().unwrap());
        let (error, open) = answer(server, &req(super::super::op::OPEN, node, 0, &2u32.to_le_bytes()));
        assert_eq!(error, 0);
        assert_eq!(u32::from_le_bytes(open[8..12].try_into().unwrap()), 1);
        (node, u64::from_le_bytes(open[..8].try_into().unwrap()))
    }
    fn control(server: &Server, node: u64, fh: u64, request: Request) -> Reply {
        let mut body = Vec::new();
        body.extend_from_slice(&fh.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&wire::IOCTL.to_le_bytes());
        // Never followed: this value has no authority or meaning on the host.
        body.extend_from_slice(&u64::MAX.to_le_bytes());
        body.extend_from_slice(&(wire::CONTROL_BYTES as u32).to_le_bytes());
        body.extend_from_slice(&(wire::CONTROL_BYTES as u32).to_le_bytes());
        body.extend_from_slice(&request.encode().unwrap());
        let (error, bytes) = answer(server, &req(super::super::op::IOCTL, node, 0, &body));
        assert_eq!(error, 0);
        assert_eq!(&bytes[..16], &[0; 16]);
        Reply::decode(&bytes[16..]).unwrap()
    }
    fn write(server: &Server, node: u64, fh: u64, offset: u64, bytes: &[u8]) -> i32 {
        let mut body = Vec::new();
        body.extend_from_slice(&fh.to_le_bytes());
        body.extend_from_slice(&offset.to_le_bytes());
        body.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        body.extend_from_slice(&[0; 20]);
        body.extend_from_slice(bytes);
        answer(server, &req(super::super::op::WRITE, node, 0, &body)).0
    }
    fn prepared_control(server: &Server, fh: u64, request: Request) -> IoTicket<'static> {
        let mut state = server.state.lock().unwrap();
        let key = state
            .handles
            .get(&fh)
            .and_then(|h| state.nodes.get(&h.node))
            .unwrap()
            .key;
        match state
            .audit
            .as_mut()
            .unwrap()
            .prepare(key, Action::Control(request))
            .unwrap()
        {
            Prepared::Io(ticket) => ticket,
            Prepared::Ready(_) => panic!("expected a concrete file operation"),
        }
    }
    fn finish_control(server: &Server, ticket: IoTicket<'_>) -> Reply {
        let completion = ticket.perform();
        let mut state = server.state.lock().unwrap();
        let answer = match state.audit.as_mut().unwrap().finish(completion) {
            Finalized::Recorded(answer) => answer.unwrap(),
            Finalized::Unmatched(_) => panic!("completion did not match its owner"),
        };
        match answer {
            Answer::Control(reply) => reply,
            _ => panic!("expected control completion"),
        }
    }
    #[test]
    fn private_audit_ticket_retains_one_ledger_duplicate_close_and_actual_file() {
        let headers = 2 * wire::JOURNAL_HEADER_BYTES as u64;
        let slot = SLOT_OVERHEAD + 3 + 5;
        let (dir, server) = fixture(headers + 2 * slot);
        let (node, fh) = opened(&server, 0);
        let (other, other_fh) = opened(&server, 2);
        let request = Request::Reserve {
            call: 0,
            intent: 3,
            terminal: 5,
        };
        let ticket = prepared_control(&server, fh, request);
        // A retained ticket owns the original file, never the global state guard.
        assert!(server.state.try_lock().unwrap().audit.is_some());
        assert_eq!(
            std::fs::metadata(dir.join("00000000")).unwrap().len(),
            headers / 2
        );
        let pending = control(&server, node, fh, request);
        assert_eq!((pending.cause, pending.remaining), (Cause::InFlight, slot));
        assert_eq!(control(&server, node, fh, Request::Inspect { call: 0 }), pending);
        // The other member spends the same ledger's actual remaining extent.
        assert_eq!(control(&server, other, other_fh, request).remaining, 0);
        let reserved = finish_control(&server, ticket);
        assert_eq!(
            (reserved.stage, reserved.cause, reserved.remaining),
            (Stage::Reserved, Cause::None, 0)
        );
        assert_eq!(control(&server, node, fh, request), reserved);
        assert_eq!(
            std::fs::metadata(dir.join("00000000")).unwrap().len(),
            headers / 2 + slot
        );
        assert_eq!(write(&server, node, fh, reserved.intent_offset, b"abc"), 0);
        let commit = prepared_control(&server, fh, Request::CommitIntent { call: 0 });
        assert_eq!(
            control(&server, node, fh, Request::CommitIntent { call: 0 }).cause,
            Cause::InFlight
        );
        assert_eq!(
            control(&server, node, fh, Request::Close { call: 0 }).cause,
            Cause::Closed
        );
        let closed = finish_control(&server, commit);
        assert_eq!(
            (closed.stage, closed.cause, closed.remaining),
            (Stage::Closed, Cause::Closed, 0)
        );
        let state = server.state.lock().unwrap();
        let member = state
            .audit
            .as_ref()
            .unwrap()
            .members
            .iter()
            .find(|m| m.scope.member == 0)
            .unwrap();
        assert_eq!(member.lease.unwrap().stage, Stage::IntentSynced);
        assert!(matches!(member.object, Object::Available(_)));
        let file = std::fs::File::open(dir.join("00000000")).unwrap();
        let mut length = [0; 8];
        crate::platform::read_exact_at(&file, &mut length, reserved.intent_offset - 8).unwrap();
        assert_eq!(u64::from_be_bytes(length), 3);
    }
    #[test]
    fn private_audit_dropped_ticket_stays_uncertain_without_retry_or_second_charge() {
        let headers = 2 * wire::JOURNAL_HEADER_BYTES as u64;
        let slot = SLOT_OVERHEAD + 3 + 5;
        let (dir, server) = fixture(headers + slot);
        let (node, fh) = opened(&server, 0);
        let request = Request::Reserve {
            call: 0,
            intent: 3,
            terminal: 5,
        };
        drop(prepared_control(&server, fh, request));
        let pending = control(&server, node, fh, request);
        assert_eq!((pending.cause, pending.remaining), (Cause::InFlight, 0));
        assert_eq!(
            control(&server, node, fh, Request::Identify).cause,
            Cause::InFlight
        );
        assert_eq!(
            std::fs::metadata(dir.join("00000000")).unwrap().len(),
            headers / 2
        );
        assert_eq!(
            control(&server, node, fh, Request::Close { call: 0 }).cause,
            Cause::Closed
        );
        assert_eq!(control(&server, node, fh, request).remaining, 0);
        assert!(server.state.try_lock().unwrap().audit.is_some());
    }

    #[test]
    fn private_audit_actual_extent_barriers_shared_budget_and_duplicate_controls() {
        // Explicit fixture allocation, derived from two headers and two exact
        // I=3/T=5 slots. Neither a product quota nor free-space admission.
        let headers = 2 * wire::JOURNAL_HEADER_BYTES as u64;
        let slot = SLOT_OVERHEAD + 3 + 5;
        let (dir, server) = fixture(headers + 2 * slot);
        let (node, fh) = opened(&server, 0);
        let (other, other_fh) = opened(&server, 2);
        assert_eq!(control(&server, node, fh, Request::Identify).scope, scope(0));
        let reserve = Request::Reserve {
            call: 0,
            intent: 3,
            terminal: 5,
        };
        let first = control(&server, node, fh, reserve);
        assert_eq!(
            (first.stage, first.cause, first.remaining),
            (Stage::Reserved, Cause::None, slot)
        );
        let file = std::fs::File::open(dir.join("00000000")).unwrap();
        let mut header = [0; wire::JOURNAL_HEADER_BYTES];
        crate::platform::read_exact_at(&file, &mut header, 0).unwrap();
        assert_eq!(wire::JournalHeader::decode(&header).unwrap().scope, scope(0));
        assert_eq!(file.metadata().unwrap().len(), headers / 2 + slot);
        assert_eq!(control(&server, node, fh, reserve), first);
        assert_eq!(
            control(
                &server,
                node,
                fh,
                Request::Reserve {
                    call: 0,
                    intent: 4,
                    terminal: 5
                }
            )
            .cause,
            Cause::Conflict
        );
        assert_eq!(
            control(&server, node, fh, Request::CommitIntent { call: 0 }).cause,
            Cause::Order
        );
        assert_eq!(
            write(&server, node, fh, first.terminal_offset, b"bad"),
            -super::super::EPERM
        );
        assert_eq!(write(&server, node, fh, first.intent_offset, b"abc"), 0);
        // Generic actual FSYNC does not stand in for CommitIntent.
        assert_eq!(
            answer(&server, &req(super::super::op::FSYNC, node, 0, &fh.to_le_bytes())).0,
            0
        );
        assert_eq!(
            control(&server, node, fh, Request::Inspect { call: 0 }).stage,
            Stage::IntentWritten
        );
        let intent = control(&server, node, fh, Request::CommitIntent { call: 0 });
        assert_eq!(intent.stage, Stage::IntentSynced);
        assert_eq!(
            control(&server, node, fh, Request::CommitIntent { call: 0 }),
            intent
        );
        assert_eq!(
            write(&server, node, fh, first.intent_offset, b"abc"),
            -super::super::EPERM
        );
        assert_eq!(write(&server, node, fh, first.terminal_offset, b"ok"), 0);
        let terminal = control(&server, node, fh, Request::CommitTerminal { call: 0 });
        assert_eq!(terminal.stage, Stage::TerminalSynced);
        assert_eq!(
            control(&server, node, fh, Request::CommitTerminal { call: 0 }),
            terminal
        );
        let mut length = [0; 8];
        crate::platform::read_exact_at(&file, &mut length, first.intent_offset - 8).unwrap();
        assert_eq!(u64::from_be_bytes(length), 3);
        crate::platform::read_exact_at(&file, &mut length, first.terminal_offset - 8).unwrap();
        assert_eq!(u64::from_be_bytes(length), 2);
        // The unused terminal tail is retained/charged, not given to a sibling.
        let second = control(
            &server,
            other,
            other_fh,
            Request::Reserve {
                call: 0,
                intent: 3,
                terminal: 5,
            },
        );
        assert_eq!(second.remaining, 0);
        assert_eq!(second.scope, scope(2));
        assert_eq!(
            control(
                &server,
                node,
                fh,
                Request::Reserve {
                    call: 1,
                    intent: 1,
                    terminal: 1
                }
            )
            .cause,
            Cause::Exhausted
        );
        assert!(server.save().is_err());
        assert!(server.restore(&[]).is_err());
        assert!(server.descriptor(node, true).is_err());
        assert!(server.descriptor(node, false).is_err());
    }
    #[test]
    fn private_audit_namespace_stage_and_control_guards() {
        let headers = 2 * wire::JOURNAL_HEADER_BYTES as u64;
        let (dir, server) = fixture(headers + SLOT_OVERHEAD + 2);
        let (node, fh) = opened(&server, 0);
        for opcode in [
            super::super::op::SETATTR,
            super::super::op::UNLINK,
            super::super::op::RENAME,
            super::super::op::LINK,
            super::super::op::FALLOCATE,
            super::super::op::SETXATTR,
            super::super::op::CREATE,
            super::super::op::READ,
            super::super::op::READDIR,
        ] {
            assert_eq!(
                answer(&server, &req(opcode, node, 0, &[])).0,
                -super::super::EPERM
            );
        }
        assert_eq!(
            answer(
                &server,
                &req(super::super::op::OPEN, node, 0, &2u32.to_le_bytes())
            )
            .0,
            -super::super::EPERM
        );
        let mut body = Vec::new();
        body.extend_from_slice(&fh.to_le_bytes());
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&wire::IOCTL.to_le_bytes());
        body.extend_from_slice(&0u64.to_le_bytes());
        body.extend_from_slice(&(wire::CONTROL_BYTES as u32).to_le_bytes());
        body.extend_from_slice(&(wire::CONTROL_BYTES as u32).to_le_bytes());
        body.extend_from_slice(&Request::Identify.encode().unwrap());
        for (offset, value) in [
            (8, 1),
            (8, 2),
            (8, 4),
            (8, 8),
            (8, 16),
            (8, 32),
            (13, 0),
            (24, 0),
            (28, 0),
            (32, 2),
            (34, 1),
        ] {
            let mut malformed = body.clone();
            malformed[offset] = value;
            assert_eq!(
                answer(&server, &req(super::super::op::IOCTL, node, 0, &malformed)).0,
                -super::super::EINVAL
            );
        }
        assert_eq!(
            control(
                &server,
                node,
                fh,
                Request::Reserve {
                    call: 0,
                    intent: 1,
                    terminal: 1
                }
            )
            .cause,
            Cause::None
        );
        let before = std::fs::metadata(dir.join("00000000")).unwrap().len();
        assert_eq!(
            answer(
                &server,
                &req(super::super::op::RELEASE, node, 0, &fh.to_le_bytes())
            )
            .0,
            0
        );
        assert_eq!(std::fs::metadata(dir.join("00000000")).unwrap().len(), before);
        assert_eq!(
            answer(
                &server,
                &req(super::super::op::OPEN, node, 0, &2u32.to_le_bytes())
            )
            .0,
            -super::super::EPERM
        );
    }

    #[test]
    fn private_audit_constructor_reports_real_readiness_and_never_truncates_existing_data() {
        let dir = shards_testdir::TempDir::new("audit-retained").unwrap();
        let storage = StorageRequest::new(
            wire::JOURNAL_HEADER_BYTES as u64,
            wire::Retention::UntilOwnerDeletion,
        )
        .unwrap();
        let config = wire::ExportConfig::new(storage, vec![scope(0)]).unwrap();
        let server = Server::audited(root_of(&dir), &config, 0).unwrap();
        let ready = server.audit_readiness().unwrap();
        assert_eq!(
            ready,
            wire::ExportStatus::Ready {
                remaining: 0,
                objects: 1
            }
        );
        assert_eq!(wire::ExportStatus::decode(&ready.encode().unwrap()), Ok(ready));
        let before = std::fs::read(dir.join("00000000")).unwrap();
        let error = Server::audited(root_of(&dir), &config, 0).unwrap_err();
        assert_eq!(
            error.status(),
            wire::ExportStatus::Refused {
                cause: wire::ExportCause::ExistingObject,
                retained: wire::Retained::Unknown
            }
        );
        assert_eq!(std::fs::read(dir.join("00000000")).unwrap(), before);
        assert_eq!(
            std::fs::metadata(dir.join("00000000")).unwrap().len(),
            wire::JOURNAL_HEADER_BYTES as u64
        );
    }
}
