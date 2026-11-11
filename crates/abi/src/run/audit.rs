//! Restricted, fixed-buffer control for a private bound audit file. Payloads never select scope.
use alloc::vec::Vec;
use core::num::NonZeroU64;

pub const VERSION: u8 = 1;
pub const SCOPE_BYTES: usize = 3 * 32 + size_of::<u64>() + size_of::<u32>();
pub const CONTROL_BYTES: usize = 4 + 7 * size_of::<u64>() + SCOPE_BYTES + 8;
pub const STORAGE_BYTES: usize = 1 + size_of::<u64>();
pub const STARTUP_BYTES: usize = 1 + SCOPE_BYTES;
pub const JOURNAL_HEADER_BYTES: usize = STARTUP_BYTES + STORAGE_BYTES;
pub const EXPORT_HEADER_BYTES: usize = 1 + STORAGE_BYTES + size_of::<u32>();
pub const EXPORT_STATUS_BYTES: usize = 8 + size_of::<i32>() + size_of::<u64>() + size_of::<u32>();
pub const MEMBER_NAME_BYTES: usize = 2 * size_of::<u32>() + 1;
/// Three immutable u64 slot fields and two committed-length u64 prefixes.
pub const SLOT_OVERHEAD: u64 = 5 * size_of::<u64>() as u64;
/// Linux6.18 asm-generic/ioctl.h: _IOWR(0xe6,0,CONTROL_BYTES). The pinned registry
/// does not list 0xe6; this private regular FUSE object's selector grants nothing.
pub const IOCTL: u32 = (3 << 30) | ((CONTROL_BYTES as u32) << 16) | (0xe6 << 8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Length,
    Version,
    Tag,
    Reserved,
    Bounds,
    Allocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    UntilOwnerDeletion,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageRequest {
    bytes: NonZeroU64,
    retention: Retention,
}
impl StorageRequest {
    pub fn new(bytes: u64, retention: Retention) -> Result<Self, Error> {
        Ok(Self {
            bytes: NonZeroU64::new(bytes).ok_or(Error::Bounds)?,
            retention,
        })
    }
    /// Admitted journal byte extents. Filesystem blocks, metadata and parent
    /// resource guarantees require independent backing; this request mints none.
    pub fn bytes(self) -> u64 {
        self.bytes.get()
    }
    pub fn retention(self) -> Retention {
        self.retention
    }
    pub fn encode(self) -> Result<[u8; STORAGE_BYTES], Error> {
        let mut bytes = [0; STORAGE_BYTES];
        let mut out = Output(bytes.as_mut_slice());
        out.put([match self.retention {
            Retention::UntilOwnerDeletion => 1,
        }])?;
        out.put(self.bytes().to_be_bytes())?;
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != STORAGE_BYTES {
            return Err(Error::Length);
        }
        let mut input = Input(bytes);
        let retention = match input.take::<1>()? {
            [1] => Retention::UntilOwnerDeletion,
            _ => return Err(Error::Tag),
        };
        Self::new(u64::from_be_bytes(input.take()?), retention)
    }
}
/// Immutable file metadata, never a durable-effect receipt or a member budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalHeader {
    pub scope: Scope,
    pub storage: StorageRequest,
}
impl JournalHeader {
    pub fn encode(self) -> Result<[u8; JOURNAL_HEADER_BYTES], Error> {
        let mut bytes = [0; JOURNAL_HEADER_BYTES];
        let mut out = Output(bytes.as_mut_slice());
        out.put(self.scope.encode_startup()?)?;
        out.put(self.storage.encode()?)?;
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != JOURNAL_HEADER_BYTES {
            return Err(Error::Length);
        }
        let mut input = Input(bytes);
        Ok(Self {
            scope: Scope::decode_startup(&input.take::<STARTUP_BYTES>()?)?,
            storage: StorageRequest::decode(&input.take::<STORAGE_BYTES>()?)?,
        })
    }
}

/// Data on the existing trusted share-child startup channel. This checked
/// representation is owner policy/binding, not actual acquired storage backing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportConfig {
    storage: StorageRequest,
    scopes: Vec<Scope>,
}
/// Accepted only on the protected initial run after actual host producer Ready.
/// Ordinary exec/setup data cannot mint this startup transport input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportMount {
    pub share_index: u32,
    pub config: ExportConfig,
}
impl ExportConfig {
    pub fn new(storage: StorageRequest, scopes: Vec<Scope>) -> Result<Self, Error> {
        let count = u32::try_from(scopes.len()).map_err(|_| Error::Bounds)?;
        if u64::from(count)
            .checked_mul(JOURNAL_HEADER_BYTES as u64)
            .is_none_or(|n| n > storage.bytes())
        {
            return Err(Error::Bounds);
        }
        let first = scopes.first().ok_or(Error::Bounds)?;
        if first.generation == 0 {
            return Err(Error::Bounds);
        }
        if scopes.iter().any(|scope| {
            (scope.owner, scope.run, scope.generation, scope.policy)
                != (first.owner, first.run, first.generation, first.policy)
        }) || scopes
            .windows(2)
            .any(|pair| !matches!(pair,[a,b] if a.member < b.member))
        {
            return Err(Error::Bounds);
        }
        Ok(Self { storage, scopes })
    }
    pub fn storage(&self) -> StorageRequest {
        self.storage
    }
    pub fn scopes(&self) -> &[Scope] {
        &self.scopes
    }
    pub fn header_length(bytes: &[u8]) -> Result<usize, Error> {
        if bytes.len() != EXPORT_HEADER_BYTES {
            return Err(Error::Length);
        }
        let mut input = Input(bytes);
        if input.take::<1>()? != [VERSION] {
            return Err(Error::Version);
        }
        let storage = StorageRequest::decode(&input.take::<STORAGE_BYTES>()?)?;
        let members = u32::from_be_bytes(input.take()?);
        if members == 0 {
            return Err(Error::Bounds);
        }
        if u64::from(members)
            .checked_mul(JOURNAL_HEADER_BYTES as u64)
            .is_none_or(|n| n > storage.bytes())
        {
            return Err(Error::Bounds);
        }
        usize::try_from(members)
            .ok()
            .and_then(|n| n.checked_mul(SCOPE_BYTES))
            .and_then(|n| n.checked_add(EXPORT_HEADER_BYTES))
            .ok_or(Error::Bounds)
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let size = self
            .scopes
            .len()
            .checked_mul(SCOPE_BYTES)
            .and_then(|n| n.checked_add(EXPORT_HEADER_BYTES))
            .ok_or(Error::Bounds)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(size).map_err(|_| Error::Allocation)?;
        bytes.resize(size, 0);
        let mut out = Output(bytes.as_mut_slice());
        out.put([VERSION])?;
        out.put(self.storage.encode()?)?;
        out.put(
            u32::try_from(self.scopes.len())
                .map_err(|_| Error::Bounds)?
                .to_be_bytes(),
        )?;
        for &scope in &self.scopes {
            scope.write(&mut out)?;
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let (header, records) = bytes.split_at_checked(EXPORT_HEADER_BYTES).ok_or(Error::Length)?;
        if Self::header_length(header)? != bytes.len() {
            return Err(Error::Length);
        }
        let mut input = Input(header);
        input.take::<1>()?;
        let storage = StorageRequest::decode(&input.take::<STORAGE_BYTES>()?)?;
        let count = u32::from_be_bytes(input.take()?);
        let mut scopes = Vec::new();
        scopes
            .try_reserve_exact(usize::try_from(count).map_err(|_| Error::Bounds)?)
            .map_err(|_| Error::Allocation)?;
        let mut input = Input(records);
        for _ in 0..count {
            scopes.push(Scope::read(&mut input)?);
        }
        Self::new(storage, scopes)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    pub owner: [u8; 32],
    pub run: [u8; 32],
    pub generation: u64,
    pub member: u32,
    pub policy: [u8; 32],
}
impl Scope {
    pub fn member_name(self) -> [u8; MEMBER_NAME_BYTES] {
        let mut name = [0; MEMBER_NAME_BYTES];
        for (out, digit) in name.iter_mut().zip(
            self.member
                .to_be_bytes()
                .into_iter()
                .flat_map(|byte| [byte >> 4, byte & 15]),
        ) {
            *out = if digit < 10 {
                b'0' + digit
            } else {
                b'a' + digit - 10
            };
        }
        name
    }
    /// Expected binding on init's protected startup pipe, never actor input.
    pub fn encode_startup(self) -> Result<[u8; STARTUP_BYTES], Error> {
        let mut bytes = [0; STARTUP_BYTES];
        let mut out = Output(bytes.as_mut_slice());
        out.put([VERSION])?;
        self.write(&mut out)?;
        Ok(bytes)
    }
    pub fn decode_startup(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != STARTUP_BYTES {
            return Err(Error::Length);
        }
        let mut input = Input(bytes);
        if input.take::<1>()? != [VERSION] {
            return Err(Error::Version);
        }
        Self::read(&mut input)
    }
    fn write(self, out: &mut Output<'_>) -> Result<(), Error> {
        out.put(self.owner)?;
        out.put(self.run)?;
        out.put(self.generation.to_be_bytes())?;
        out.put(self.member.to_be_bytes())?;
        out.put(self.policy)
    }
    fn read(input: &mut Input<'_>) -> Result<Self, Error> {
        Ok(Self {
            owner: input.take()?,
            run: input.take()?,
            generation: u64::from_be_bytes(input.take()?),
            member: u32::from_be_bytes(input.take()?),
            policy: input.take()?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Identify,
    Reserve { call: u64, intent: u64, terminal: u64 },
    CommitIntent { call: u64 },
    CommitTerminal { call: u64 },
    Close { call: u64 },
    Inspect { call: u64 },
}
impl Request {
    pub fn call(self) -> Option<u64> {
        match self {
            Self::Identify => None,
            Self::Reserve { call, .. }
            | Self::CommitIntent { call }
            | Self::CommitTerminal { call }
            | Self::Close { call }
            | Self::Inspect { call } => Some(call),
        }
    }
    pub fn encode(self) -> Result<[u8; CONTROL_BYTES], Error> {
        let (op, call, intent, terminal) = match self {
            Self::Identify => (0, 0, 0, 0),
            Self::Reserve {
                call,
                intent,
                terminal,
            } => {
                if intent == 0 || terminal == 0 || intent.checked_add(terminal).is_none() {
                    return Err(Error::Bounds);
                }
                (1, call, intent, terminal)
            }
            Self::CommitIntent { call } => (2, call, 0, 0),
            Self::CommitTerminal { call } => (3, call, 0, 0),
            Self::Close { call } => (4, call, 0, 0),
            Self::Inspect { call } => (5, call, 0, 0),
        };
        let mut bytes = [0; CONTROL_BYTES];
        let mut out = Output(bytes.as_mut_slice());
        out.put([VERSION, op, 0, 0])?;
        out.put(call.to_be_bytes())?;
        out.put(intent.to_be_bytes())?;
        out.put(terminal.to_be_bytes())?;
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != CONTROL_BYTES {
            return Err(Error::Length);
        }
        let mut input = Input(bytes);
        let [version, op, _, _] = input.take::<4>()?;
        if version != VERSION {
            return Err(Error::Version);
        }
        let call = u64::from_be_bytes(input.take()?);
        let intent = u64::from_be_bytes(input.take()?);
        let terminal = u64::from_be_bytes(input.take()?);
        let request = match op {
            0 => Self::Identify,
            1 => Self::Reserve {
                call,
                intent,
                terminal,
            },
            2 => Self::CommitIntent { call },
            3 => Self::CommitTerminal { call },
            4 => Self::Close { call },
            5 => Self::Inspect { call },
            _ => return Err(Error::Tag),
        };
        // All unused fields, flags and padding must be zero, including scope bytes.
        // Canonical serialization checks them without following any guest pointer.
        if request.encode()?.as_slice() != bytes {
            return Err(Error::Reserved);
        }
        Ok(request)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Stage {
    Empty = 0,
    Reserved,
    IntentWritten,
    IntentSynced,
    TerminalWritten,
    TerminalSynced,
    Closed,
    Failed,
}
impl TryFrom<u8> for Stage {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self, Error> {
        Ok(match value {
            0 => Self::Empty,
            1 => Self::Reserved,
            2 => Self::IntentWritten,
            3 => Self::IntentSynced,
            4 => Self::TerminalWritten,
            5 => Self::TerminalSynced,
            6 => Self::Closed,
            7 => Self::Failed,
            _ => return Err(Error::Tag),
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Cause {
    None = 0,
    Exhausted,
    Order,
    Conflict,
    Stale,
    Closed,
    Overflow,
    Io,
    Partial,
    InFlight,
}
impl TryFrom<u8> for Cause {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self, Error> {
        Ok(match value {
            0 => Self::None,
            1 => Self::Exhausted,
            2 => Self::Order,
            3 => Self::Conflict,
            4 => Self::Stale,
            5 => Self::Closed,
            6 => Self::Overflow,
            7 => Self::Io,
            8 => Self::Partial,
            9 => Self::InFlight,
            _ => return Err(Error::Tag),
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Platform {
    Linux = 1,
    Darwin,
    Windows,
}
impl TryFrom<u8> for Platform {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self, Error> {
        Ok(match value {
            1 => Self::Linux,
            2 => Self::Darwin,
            3 => Self::Windows,
            _ => return Err(Error::Tag),
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Native {
    pub platform: Platform,
    pub code: Option<i32>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FileStep {
    ReserveAllocate = 1,
    HeaderAppend,
    ReserveSync,
    IntentAppend,
    IntentLength,
    IntentSync,
    TerminalAppend,
    TerminalLength,
    TerminalSync,
    FileSync,
}
impl TryFrom<u8> for FileStep {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self, Error> {
        Ok(match value {
            1 => Self::ReserveAllocate,
            2 => Self::HeaderAppend,
            3 => Self::ReserveSync,
            4 => Self::IntentAppend,
            5 => Self::IntentLength,
            6 => Self::IntentSync,
            7 => Self::TerminalAppend,
            8 => Self::TerminalLength,
            9 => Self::TerminalSync,
            10 => Self::FileSync,
            _ => return Err(Error::Tag),
        })
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retained {
    Unknown,
    Known { remaining: u64, objects: u32 },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportCause {
    Config(Error),
    DirectoryNative(i32),
    NotDirectory,
    StateCapacity,
    Capacity,
    Allocation,
    Filesystem(i32),
    Io { step: FileStep, native: Native },
    Encoding,
    ExistingObject,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportStatus {
    Ready { remaining: u64, objects: u32 },
    Refused { cause: ExportCause, retained: Retained },
}
impl ExportStatus {
    pub fn encode(self) -> Result<[u8; EXPORT_STATUS_BYTES], Error> {
        let (tag, cause, retained) = match self {
            Self::Ready { remaining, objects } => {
                if objects == 0 {
                    return Err(Error::Bounds);
                }
                (1, None, Retained::Known { remaining, objects })
            }
            Self::Refused { cause, retained } => (2, Some(cause), retained),
        };
        let (kind, platform, code_kind, step, code) = match cause {
            None => (0, 0, 0, 0, 0),
            Some(ExportCause::Config(error)) => (
                1,
                0,
                1,
                0,
                match error {
                    Error::Length => 1,
                    Error::Version => 2,
                    Error::Tag => 3,
                    Error::Reserved => 4,
                    Error::Bounds => 5,
                    Error::Allocation => 6,
                },
            ),
            Some(ExportCause::DirectoryNative(code)) => (2, 0, 1, 0, code),
            Some(ExportCause::NotDirectory) => (9, 0, 0, 0, 0),
            Some(ExportCause::StateCapacity) => (10, 0, 0, 0, 0),
            Some(ExportCause::Capacity) => (3, 0, 0, 0, 0),
            Some(ExportCause::Allocation) => (4, 0, 0, 0, 0),
            Some(ExportCause::Filesystem(code)) => (5, 0, 1, 0, code),
            Some(ExportCause::Io { step, native }) => (
                6,
                native.platform as u8,
                u8::from(native.code.is_some()),
                step as u8,
                native.code.unwrap_or(0),
            ),
            Some(ExportCause::Encoding) => (7, 0, 0, 0, 0),
            Some(ExportCause::ExistingObject) => (8, 0, 0, 0, 0),
        };
        let (retention, remaining, objects) = match retained {
            Retained::Unknown => (0, 0, 0),
            Retained::Known { remaining, objects } => (1, remaining, objects),
        };
        let mut bytes = [0; EXPORT_STATUS_BYTES];
        let mut out = Output(bytes.as_mut_slice());
        out.put([VERSION, tag, kind, platform, code_kind, retention, step, 0])?;
        out.put(code.to_be_bytes())?;
        out.put(remaining.to_be_bytes())?;
        out.put(objects.to_be_bytes())?;
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != EXPORT_STATUS_BYTES {
            return Err(Error::Length);
        }
        let mut input = Input(bytes);
        let [version, tag, kind, platform, code_kind, retention, step, _] = input.take()?;
        if version != VERSION {
            return Err(Error::Version);
        }
        let code = i32::from_be_bytes(input.take()?);
        let remaining = u64::from_be_bytes(input.take()?);
        let objects = u32::from_be_bytes(input.take()?);
        let retained = match retention {
            0 => Retained::Unknown,
            1 => Retained::Known { remaining, objects },
            _ => return Err(Error::Tag),
        };
        let cause = match kind {
            0 => None,
            1 => Some(ExportCause::Config(match code {
                1 => Error::Length,
                2 => Error::Version,
                3 => Error::Tag,
                4 => Error::Reserved,
                5 => Error::Bounds,
                6 => Error::Allocation,
                _ => return Err(Error::Tag),
            })),
            2 => Some(ExportCause::DirectoryNative(code)),
            3 => Some(ExportCause::Capacity),
            4 => Some(ExportCause::Allocation),
            5 => Some(ExportCause::Filesystem(code)),
            6 => Some(ExportCause::Io {
                step: step.try_into()?,
                native: Native {
                    platform: platform.try_into()?,
                    code: match code_kind {
                        0 => None,
                        1 => Some(code),
                        _ => return Err(Error::Tag),
                    },
                },
            }),
            7 => Some(ExportCause::Encoding),
            8 => Some(ExportCause::ExistingObject),
            9 => Some(ExportCause::NotDirectory),
            10 => Some(ExportCause::StateCapacity),
            _ => return Err(Error::Tag),
        };
        let status = match (tag, cause, retained) {
            (1, None, Retained::Known { remaining, objects }) => Self::Ready { remaining, objects },
            (2, Some(cause), retained) => Self::Refused { cause, retained },
            _ => return Err(Error::Tag),
        };
        if status.encode()?.as_slice() != bytes {
            return Err(Error::Reserved);
        }
        Ok(status)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    pub stage: Stage,
    pub cause: Cause,
    pub call: u64,
    pub intent: u64,
    pub terminal: u64,
    pub intent_offset: u64,
    pub terminal_offset: u64,
    pub remaining: u64,
    pub accepted: u64,
    pub scope: Scope,
    pub native: Option<Native>,
    pub failure: Option<FileStep>,
}
impl Reply {
    pub fn encode(self) -> Result<[u8; CONTROL_BYTES], Error> {
        if (self.cause == Cause::Io) != self.native.is_some() {
            return Err(Error::Reserved);
        }
        if matches!(self.cause, Cause::Io | Cause::Partial) != self.failure.is_some() {
            return Err(Error::Reserved);
        }
        let mut bytes = [0; CONTROL_BYTES];
        let mut out = Output(bytes.as_mut_slice());
        out.put([VERSION, 0x80, self.stage as u8, self.cause as u8])?;
        for number in [
            self.call,
            self.intent,
            self.terminal,
            self.intent_offset,
            self.terminal_offset,
            self.remaining,
            self.accepted,
        ] {
            out.put(number.to_be_bytes())?;
        }
        self.scope.write(&mut out)?;
        let (platform, kind, code) = match self.native {
            None => (0, 0, 0),
            Some(Native { platform, code: None }) => (platform as u8, 0, 0),
            Some(Native {
                platform,
                code: Some(code),
            }) => (platform as u8, 1, code),
        };
        out.put([platform, kind, self.failure.map_or(0, |step| step as u8), 0])?;
        out.put(code.to_be_bytes())?;
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != CONTROL_BYTES {
            return Err(Error::Length);
        }
        let mut input = Input(bytes);
        let [version, op, stage, cause] = input.take::<4>()?;
        if version != VERSION {
            return Err(Error::Version);
        }
        if op != 0x80 {
            return Err(Error::Tag);
        }
        let mut reply = Self {
            stage: stage.try_into()?,
            cause: cause.try_into()?,
            call: u64::from_be_bytes(input.take()?),
            intent: u64::from_be_bytes(input.take()?),
            terminal: u64::from_be_bytes(input.take()?),
            intent_offset: u64::from_be_bytes(input.take()?),
            terminal_offset: u64::from_be_bytes(input.take()?),
            remaining: u64::from_be_bytes(input.take()?),
            accepted: u64::from_be_bytes(input.take()?),
            scope: Scope::read(&mut input)?,
            native: None,
            failure: None,
        };
        let [platform, kind, failure, _] = input.take::<4>()?;
        reply.failure = if failure == 0 {
            None
        } else {
            Some(failure.try_into()?)
        };
        let code = i32::from_be_bytes(input.take()?);
        reply.native = match (platform, kind) {
            (0, 0) => None,
            (platform, 0) => Some(Native {
                platform: platform.try_into()?,
                code: None,
            }),
            (platform, 1) => Some(Native {
                platform: platform.try_into()?,
                code: Some(code),
            }),
            _ => return Err(Error::Tag),
        };
        if reply.encode()?.as_slice() != bytes {
            return Err(Error::Reserved);
        }
        reply
            .intent_offset
            .checked_add(reply.intent)
            .ok_or(Error::Bounds)?;
        reply
            .terminal_offset
            .checked_add(reply.terminal)
            .ok_or(Error::Bounds)?;
        if reply.accepted > reply.intent.max(reply.terminal) {
            return Err(Error::Bounds);
        }
        if reply.cause == Cause::None
            && !matches!(reply.stage, Stage::Empty | Stage::Closed)
            && (reply.intent == 0
                || reply.terminal == 0
                || reply
                    .intent_offset
                    .checked_add(reply.intent)
                    .is_none_or(|end| end > reply.terminal_offset))
        {
            return Err(Error::Bounds);
        }
        Ok(reply)
    }
}

struct Input<'a>(&'a [u8]);
impl Input<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let (bytes, rest) = self.0.split_first_chunk::<N>().ok_or(Error::Length)?;
        self.0 = rest;
        Ok(*bytes)
    }
}
struct Output<'a>(&'a mut [u8]);
impl Output<'_> {
    fn put<const N: usize>(&mut self, bytes: [u8; N]) -> Result<(), Error> {
        let (target, rest) = core::mem::take(&mut self.0)
            .split_first_chunk_mut::<N>()
            .ok_or(Error::Length)?;
        target.copy_from_slice(&bytes);
        self.0 = rest;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn private_audit_control_is_fixed_canonical_and_pointer_free() {
        for request in [
            Request::Identify,
            Request::Reserve {
                call: 0,
                intent: 17,
                terminal: 19,
            },
            Request::CommitIntent { call: 0 },
            Request::CommitTerminal { call: 0 },
            Request::Close { call: 0 },
            Request::Inspect { call: 0 },
        ] {
            let bytes = request.encode().unwrap();
            assert_eq!(Request::decode(&bytes), Ok(request));
            for offset in [2, 3, CONTROL_BYTES - 1] {
                let mut bad = bytes;
                bad[offset] = 1;
                assert_eq!(Request::decode(&bad), Err(Error::Reserved));
            }
            assert_eq!(Request::decode(&bytes[..CONTROL_BYTES - 1]), Err(Error::Length));
        }
        assert_eq!(
            Request::Reserve {
                call: 0,
                intent: u64::MAX,
                terminal: 1
            }
            .encode(),
            Err(Error::Bounds)
        );
        assert_eq!(
            StorageRequest::new(0, Retention::UntilOwnerDeletion),
            Err(Error::Bounds)
        );
        let storage = StorageRequest::new(u64::MAX, Retention::UntilOwnerDeletion).unwrap();
        assert_eq!(StorageRequest::decode(&storage.encode().unwrap()), Ok(storage));
        assert_eq!((IOCTL >> 16) & ((1 << 14) - 1), CONTROL_BYTES as u32);
    }
    #[test]
    fn private_audit_reply_preserves_scope_native_error_and_unsigned_bounds() {
        let reply = Reply {
            stage: Stage::Failed,
            cause: Cause::Io,
            call: u64::MAX,
            intent: 13,
            terminal: 17,
            intent_offset: 19,
            terminal_offset: 32,
            remaining: 0,
            accepted: 0,
            scope: Scope {
                owner: [1; 32],
                run: [2; 32],
                generation: u64::MAX,
                member: u32::MAX,
                policy: [3; 32],
            },
            native: Some(Native {
                platform: Platform::Windows,
                code: Some(i32::MIN),
            }),
            failure: Some(FileStep::TerminalSync),
        };
        let bytes = reply.encode().unwrap();
        assert_eq!(Reply::decode(&bytes), Ok(reply));
        let mut bad = bytes;
        bad[2] = u8::MAX;
        assert_eq!(Reply::decode(&bad), Err(Error::Tag));
        let mut bad = bytes;
        bad[CONTROL_BYTES - 5] = 1;
        assert_eq!(Reply::decode(&bad), Err(Error::Reserved));
    }

    #[test]
    fn private_audit_export_layout_headers_and_typed_status_are_canonical() {
        let scope = Scope {
            owner: [1; 32],
            run: [2; 32],
            generation: 3,
            member: 0,
            policy: [4; 32],
        };
        let storage =
            StorageRequest::new(2 * JOURNAL_HEADER_BYTES as u64, Retention::UntilOwnerDeletion).unwrap();
        let header = JournalHeader { scope, storage };
        assert_eq!(JournalHeader::decode(&header.encode().unwrap()), Ok(header));
        assert_eq!(
            JournalHeader::decode(&[0; CONTROL_BYTES + STORAGE_BYTES]),
            Err(Error::Length)
        );
        let mut unknown = header.encode().unwrap();
        unknown[0] = VERSION + 1;
        assert_eq!(JournalHeader::decode(&unknown), Err(Error::Version));
        let config = ExportConfig::new(storage, alloc::vec![scope, Scope { member: 2, ..scope }]).unwrap();
        let bytes = config.encode().unwrap();
        assert_eq!(bytes.len(), EXPORT_HEADER_BYTES + 2 * SCOPE_BYTES);
        assert_eq!(ExportConfig::decode(&bytes), Ok(config));
        assert_eq!(
            ExportConfig::decode(&bytes[..bytes.len() - 1]),
            Err(Error::Length)
        );
        let mut enormous = bytes[..EXPORT_HEADER_BYTES].to_vec();
        enormous[EXPORT_HEADER_BYTES - 4..].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(ExportConfig::header_length(&enormous), Err(Error::Bounds));
        assert!(ExportConfig::new(storage, alloc::vec![Scope { member: 2, ..scope }, scope]).is_err());
        assert!(
            ExportConfig::new(
                storage,
                alloc::vec![Scope {
                    generation: 0,
                    ..scope
                }]
            )
            .is_err()
        );
        let mut old_directory = [0; EXPORT_STATUS_BYTES];
        old_directory[..3].copy_from_slice(&[VERSION, 2, 2]);
        assert_eq!(ExportStatus::decode(&old_directory), Err(Error::Reserved));
        for status in [
            ExportStatus::Ready {
                remaining: 0,
                objects: 2,
            },
            ExportStatus::Refused {
                cause: ExportCause::Capacity,
                retained: Retained::Known {
                    remaining: 0,
                    objects: 1,
                },
            },
            ExportStatus::Refused {
                cause: ExportCause::ExistingObject,
                retained: Retained::Unknown,
            },
            ExportStatus::Refused {
                cause: ExportCause::DirectoryNative(i32::MIN),
                retained: Retained::Unknown,
            },
            ExportStatus::Refused {
                cause: ExportCause::NotDirectory,
                retained: Retained::Unknown,
            },
            ExportStatus::Refused {
                cause: ExportCause::StateCapacity,
                retained: Retained::Unknown,
            },
            ExportStatus::Refused {
                cause: ExportCause::Filesystem(i32::MIN),
                retained: Retained::Unknown,
            },
            ExportStatus::Refused {
                cause: ExportCause::Io {
                    step: FileStep::ReserveSync,
                    native: Native {
                        platform: Platform::Windows,
                        code: Some(i32::MIN),
                    },
                },
                retained: Retained::Known {
                    remaining: u64::MAX,
                    objects: u32::MAX,
                },
            },
            ExportStatus::Refused {
                cause: ExportCause::Config(Error::Version),
                retained: Retained::Unknown,
            },
        ] {
            let bytes = status.encode().unwrap();
            assert_eq!(ExportStatus::decode(&bytes), Ok(status));
            for (offset, value) in [(0, VERSION + 1), (1, u8::MAX), (2, u8::MAX), (6, u8::MAX), (7, 1)] {
                let mut bad = bytes;
                bad[offset] = value;
                assert!(ExportStatus::decode(&bad).is_err());
            }
        }
    }
}
