//! Checked peer effects for the actual per-domain instance; no native-exec authority.
use crate::server::json::Value;
use std::fmt;
use std::os::fd::{AsRawFd as _, BorrowedFd};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Received,
    Validated,
    Authorized,
    Executing,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Send,
    Answer,
    HandRequest,
    HandAnswer,
}
impl Operation {
    fn request(self) -> bool {
        matches!(self, Self::Send | Self::HandRequest)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Evidence {
    MemoryOnly,
    RequiredDurable,
}

pub(crate) struct Context<'a> {
    pub principal: &'a str,
    pub scratch: Option<BorrowedFd<'a>>,
    pub evidence: Evidence,
}
/// A borrowed view of the one actual peer and its owned request ledger.
pub(crate) struct Peer<'a> {
    pub label: &'a str,
    pub fd: BorrowedFd<'a>,
    pub send: bool,
    pub answer: bool,
    pub most: usize,
    pub last: &'a mut u64,
    pub sent: &'a mut Vec<u64>,
    pub asked: &'a mut Vec<u64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    Arguments(&'static str),
    Grant,
    RequestNotOwned,
    IdExhausted,
    Capacity,
    DurableEvidenceUnavailable,
    NoScratch,
    Path,
    PathNul,
    TooLong,
    FileKind,
    FileIo(Option<i32>),
    PeerFull,
    PeerIo(Option<i32>),
    Partial { accepted: usize, expected: usize },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub phase: Phase,
    pub reason: Reason,
}
impl Refusal {
    fn at(phase: Phase, reason: Reason) -> Self {
        Self { phase, reason }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    NotStarted,
    NotAccepted,
    /// Local kernel acceptance, never proof of remote execution.
    KernelAccepted {
        bytes: usize,
    },
    Uncertain {
        accepted: usize,
        expected: usize,
    },
}
/// Borrowed trusted scope plus actual local effects; ephemeral, not durable evidence.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Receipt<'a> {
    pub principal: &'a str,
    pub peer: &'a str,
    pub operation: Operation,
    pub request: u64,
    pub effect: Effect,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Completed,
    Refused(Refusal),
    Failed(Refusal),
    Uncertain(Refusal),
}
#[derive(Debug)]
pub(crate) struct Terminal<'a> {
    pub receipt: Receipt<'a>,
    pub outcome: Outcome,
    response: ResponseSlot,
}
impl Terminal<'_> {
    pub fn present(self) -> Reply {
        self.response.render(&self.receipt, self.outcome)
    }
}

pub(crate) enum Reply {
    Owned(String),
    Literal(&'static str),
}
impl Reply {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Owned(s) => s,
            Self::Literal(s) => s,
        }
    }
    pub fn as_bytes(&self) -> &[u8] {
        self.as_str().as_bytes()
    }
}
impl fmt::Debug for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reply")
            .field("bytes", &self.as_str().len())
            .finish()
    }
}
impl From<String> for Reply {
    fn from(s: String) -> Self {
        Self::Owned(s)
    }
}
impl std::ops::Deref for Reply {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl fmt::Display for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    Program,
    Mcp,
}

pub(crate) struct Request<'a> {
    operation: Operation,
    to: &'a str,
    id: Option<u64>,
    body: Body<'a>,
}
enum Body<'a> {
    Text(&'a str),
    Hand(&'a str),
}
impl<'a> Request<'a> {
    pub fn receive(name: &str, args: Option<&'a Value>) -> Option<Result<Self, Refusal>> {
        let operation = match name {
            "send" => Operation::Send,
            "answer" => Operation::Answer,
            "hand" => Operation::HandRequest,
            _ => return None,
        };
        Some(Self::validate(operation, args))
    }
    fn validate(mut operation: Operation, args: Option<&'a Value>) -> Result<Self, Refusal> {
        let invalid = |field| Refusal::at(Phase::Received, Reason::Arguments(field));
        let args = args
            .filter(|a| matches!(a, Value::Object(_)))
            .ok_or_else(|| invalid("arguments"))?;
        let text = |field| args.get(field).and_then(Value::str).ok_or_else(|| invalid(field));
        let to = text("to")?;
        if to.is_empty() {
            return Err(invalid("to"));
        }
        let id = match args.get("id") {
            Some(v) => Some(v.u64().ok_or_else(|| invalid("id"))?),
            None if operation == Operation::Answer => return Err(invalid("id")),
            None => None,
        };
        let body = if operation == Operation::HandRequest {
            if id.is_some() {
                operation = Operation::HandAnswer;
            }
            Body::Hand(text("path")?)
        } else {
            if operation == Operation::Send && id.is_some() {
                return Err(invalid("id"));
            }
            Body::Text(text("text")?)
        };
        Ok(Self {
            operation,
            to,
            id,
            body,
        })
    }
    pub fn destination(&self) -> &'a str {
        self.to
    }
    pub fn authorize<'b>(
        self,
        context: Context<'b>,
        peer: Option<Peer<'b>>,
    ) -> Result<Authorized<'a, 'b>, Refusal> {
        let denied = |reason| Refusal::at(Phase::Validated, reason);
        let missing = if self.id.is_some() {
            Reason::RequestNotOwned
        } else {
            Reason::Grant
        };
        let peer = peer
            .filter(|p| p.label == self.to)
            .ok_or_else(|| denied(missing))?;
        if self.id.is_none() && !peer.send {
            return Err(denied(Reason::Grant));
        }
        if let Some(id) = self.id
            && (!peer.answer || !peer.asked.contains(&id))
        {
            return Err(denied(Reason::RequestNotOwned));
        }
        if context.evidence == Evidence::RequiredDurable {
            return Err(denied(Reason::DurableEvidenceUnavailable));
        }
        if matches!(self.body, Body::Hand(_)) && context.scratch.is_none() {
            return Err(denied(Reason::NoScratch));
        }
        Ok(Authorized {
            request: self,
            context,
            peer,
        })
    }
}
pub(crate) struct Authorized<'a, 'b> {
    request: Request<'a>,
    context: Context<'b>,
    peer: Peer<'b>,
}
/// This token owns the actual packet (a hand's file bytes once their size is read),
/// success/error reply and pending-ID capacity.
pub(crate) struct Reserved<'a, 'b> {
    body: ReservedBody<'a, 'b>,
    peer: Peer<'b>,
    packet: Vec<u8>,
    response: ResponseSlot,
    receipt: Receipt<'b>,
}
enum ReservedBody<'a, 'b> {
    Text(&'a str),
    Hand(Hand<'a, 'b>),
}
fn capacity(phase: Phase) -> Refusal {
    Refusal::at(phase, Reason::Capacity)
}
impl<'a, 'b> Authorized<'a, 'b> {
    pub fn reserve(self, format: Format) -> Result<Reserved<'a, 'b>, Refusal> {
        let id = match self.request.id {
            Some(id) => id,
            None => self
                .peer
                .last
                .checked_add(1)
                .ok_or(Refusal::at(Phase::Authorized, Reason::IdExhausted))?,
        };
        let body = match self.request.body {
            Body::Hand(path) => ReservedBody::Hand(Hand::prepare(
                self.context
                    .scratch
                    .ok_or(Refusal::at(Phase::Authorized, Reason::NoScratch))?,
                path,
                self.peer.most,
            )?),
            Body::Text(text) => ReservedBody::Text(text),
        };
        let packet_bytes = match &body {
            ReservedBody::Text(text) => crate::server::MESSAGE_HEAD
                .checked_add(text.len())
                .filter(|n| *n <= self.peer.most)
                .ok_or(Refusal::at(Phase::Authorized, Reason::TooLong))?,
            // The file's bytes once its size is read (Hand::read).
            ReservedBody::Hand(hand) => hand.head,
        };
        let mut packet = Vec::new();
        packet
            .try_reserve_exact(packet_bytes)
            .map_err(|_| capacity(Phase::Authorized))?;
        if self.request.id.is_none() {
            self.peer
                .sent
                .try_reserve(1)
                .map_err(|_| capacity(Phase::Authorized))?;
        }
        let receipt = Receipt {
            principal: self.context.principal,
            peer: self.peer.label,
            operation: self.request.operation,
            request: id,
            effect: Effect::NotStarted,
        };
        let response = ResponseSlot::reserve(format, &receipt)?;
        // Reserved IDs are consumed even on later local failure; never wrap or reuse.
        if self.request.id.is_none() {
            *self.peer.last = id;
        }
        Ok(Reserved {
            body,
            peer: self.peer,
            packet,
            response,
            receipt,
        })
    }
}

struct Growing(String);
impl fmt::Write for Growing {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.try_reserve(s.len()).map_err(|_| fmt::Error)?;
        self.0.push_str(s);
        Ok(())
    }
}
struct Count(usize);
impl fmt::Write for Count {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0 = self.0.checked_add(s.len()).ok_or(fmt::Error)?;
        Ok(())
    }
}
struct Fixed<'a>(&'a mut String);
impl fmt::Write for Fixed<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self
            .0
            .len()
            .checked_add(s.len())
            .is_none_or(|n| n > self.0.capacity())
        {
            return Err(fmt::Error);
        }
        self.0.push_str(s);
        Ok(())
    }
}
/// Stream formatting directly through JSON escaping, without an intermediate String.
struct Quoted<'a, W>(&'a mut W);
impl<W: fmt::Write> fmt::Write for Quoted<'_, W> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            match c {
                '"' | '\\' => {
                    self.0.write_char('\\')?;
                    self.0.write_char(c)?;
                }
                c if c < '\u{20}' => write!(self.0, "\\u{:04x}", u32::from(c))?,
                c => self.0.write_char(c)?,
            }
        }
        Ok(())
    }
}
impl Format {
    fn content(self, out: &mut impl fmt::Write, error: bool, args: fmt::Arguments<'_>) -> fmt::Result {
        match self {
            Self::Program => out.write_str(if error { "{\"error\":\"" } else { "{\"value\":" })?,
            Self::Mcp => out.write_str("{\"content\":[{\"type\":\"text\",\"text\":\"")?,
        }
        if self == Self::Mcp || error {
            fmt::write(&mut Quoted(out), args)?;
        } else {
            fmt::write(out, args)?;
        }
        out.write_str(match (self, error) {
            (Self::Program, true) => "\"",
            (Self::Program, false) => "",
            (Self::Mcp, true) => "\"}],\"isError\":true",
            (Self::Mcp, false) => "\"}],\"isError\":false",
        })
    }
    fn response(
        self,
        out: &mut impl fmt::Write,
        principal: &str,
        peer: &str,
        receipt: Option<&Receipt<'_>>,
        outcome: Outcome,
    ) -> fmt::Result {
        let error = match outcome {
            Outcome::Completed => None,
            Outcome::Refused(e) | Outcome::Failed(e) | Outcome::Uncertain(e) => Some(e),
        };
        match (error, receipt) {
            (Some(error), _) => self.content(
                out,
                true,
                format_args!(
                    "{}",
                    Human {
                        error,
                        principal,
                        peer
                    }
                ),
            )?,
            (None, Some(r)) if r.operation.request() => {
                self.content(out, false, format_args!("{{\"id\":{}}}", r.request))?
            }
            _ => self.content(out, false, format_args!("{{}}"))?,
        }
        out.write_str(match self {
            Self::Program => ",\"broker\":",
            Self::Mcp => ",\"structuredContent\":",
        })?;
        out.write_str("{\"principal\":")?;
        quoted(out, principal)?;
        out.write_str(",\"peer\":")?;
        quoted(out, peer)?;
        out.write_str(",\"operation\":")?;
        match receipt {
            Some(r) => quoted(
                out,
                match r.operation {
                    Operation::Send => "send",
                    Operation::Answer => "answer",
                    Operation::HandRequest => "hand_request",
                    Operation::HandAnswer => "hand_answer",
                },
            )?,
            None => out.write_str("null")?,
        }
        out.write_str(",\"request\":")?;
        match receipt {
            Some(r) => write!(out, "{}", r.request)?,
            None => out.write_str("null")?,
        }
        out.write_str(",\"outcome\":")?;
        quoted(
            out,
            match outcome {
                Outcome::Completed => "completed",
                Outcome::Refused(_) => "refused",
                Outcome::Failed(_) => "failed",
                Outcome::Uncertain(_) => "uncertain",
            },
        )?;
        out.write_str(",\"effect\":")?;
        match receipt.map_or(Effect::NotStarted, |r| r.effect) {
            Effect::NotStarted => out.write_str("{\"kind\":\"not_started\"}")?,
            Effect::NotAccepted => out.write_str("{\"kind\":\"not_accepted\"}")?,
            Effect::KernelAccepted { bytes } => {
                write!(out, "{{\"kind\":\"kernel_accepted\",\"bytes\":{bytes}}}")?
            }
            Effect::Uncertain { accepted, expected } => write!(
                out,
                "{{\"kind\":\"uncertain\",\"accepted\":{accepted},\"expected\":{expected}}}"
            )?,
        }
        out.write_str(",\"phase\":")?;
        quoted(
            out,
            match error.map(|e| e.phase) {
                None => "terminal",
                Some(Phase::Received) => "received",
                Some(Phase::Validated) => "validated",
                Some(Phase::Authorized) => "authorized",
                Some(Phase::Executing) => "executing",
            },
        )?;
        out.write_str(",\"cause\":")?;
        match error {
            Some(error) => error.reason.write(out)?,
            None => out.write_str("null")?,
        }
        out.write_str("}}")
    }
    fn literal(self) -> Reply {
        Reply::Literal(match self {
            Self::Program => {
                "{\"error\":\"broker response unavailable; effect commitment is unknown\",\"broker\":{\"outcome\":\"uncertain\",\"effect\":{\"kind\":\"unknown\"},\"cause\":{\"kind\":\"response_capacity\"}}}"
            }
            Self::Mcp => {
                "{\"content\":[{\"type\":\"text\",\"text\":\"broker response unavailable; effect commitment is unknown\"}],\"isError\":true,\"structuredContent\":{\"outcome\":\"uncertain\",\"effect\":{\"kind\":\"unknown\"},\"cause\":{\"kind\":\"response_capacity\"}}}"
            }
        })
    }
    /// A refusal's answer where its own finds no storage: every refusal comes before any
    /// effect, so its outcome is known, unlike an executed answer's ([`Format::literal`]).
    fn refusal_literal(self) -> Reply {
        Reply::Literal(match self {
            Self::Program => {
                "{\"error\":\"refused before any effect; its response is unavailable\",\"broker\":{\"outcome\":\"refused\",\"effect\":{\"kind\":\"not_started\"},\"cause\":{\"kind\":\"response_capacity\"}}}"
            }
            Self::Mcp => {
                "{\"content\":[{\"type\":\"text\",\"text\":\"refused before any effect; its response is unavailable\"}],\"isError\":true,\"structuredContent\":{\"outcome\":\"refused\",\"effect\":{\"kind\":\"not_started\"},\"cause\":{\"kind\":\"response_capacity\"}}}"
            }
        })
    }
    pub fn refused(self, error: Refusal, principal: &str, peer: &str) -> Reply {
        let mut out = Growing(String::new());
        match self.response(&mut out, principal, peer, None, Outcome::Refused(error)) {
            Ok(()) => Reply::Owned(out.0),
            Err(_) => self.refusal_literal(),
        }
    }
    /// Outer presentation for the actual run-tool error bridge. The Display producer
    /// must itself be nonallocating; peer effects instead own ResponseSlot in advance.
    pub fn display_refusal(self, args: fmt::Arguments<'_>) -> Reply {
        let mut out = Growing(String::new());
        match self
            .content(&mut out, true, args)
            .and_then(|()| fmt::Write::write_str(&mut out, "}"))
        {
            Ok(()) => Reply::Owned(out.0),
            Err(_) => self.refusal_literal(),
        }
    }
}
fn quoted(out: &mut impl fmt::Write, value: &str) -> fmt::Result {
    out.write_str("\"")?;
    fmt::Write::write_str(&mut Quoted(out), value)?;
    out.write_str("\"")
}
impl Reason {
    fn write(self, out: &mut impl fmt::Write) -> fmt::Result {
        out.write_str("{\"kind\":")?;
        quoted(
            out,
            match self {
                Self::Arguments(_) => "arguments",
                Self::Grant => "grant",
                Self::RequestNotOwned => "request_not_owned",
                Self::IdExhausted => "identity_exhausted",
                Self::Capacity => "capacity",
                Self::DurableEvidenceUnavailable => "durable_evidence_unavailable",
                Self::NoScratch => "no_scratch",
                Self::Path => "path",
                Self::PathNul => "path_nul",
                Self::TooLong => "too_long",
                Self::FileKind => "file_kind",
                Self::FileIo(_) => "file_io",
                Self::PeerFull => "peer_full",
                Self::PeerIo(_) => "peer_io",
                Self::Partial { .. } => "short_send",
            },
        )?;
        match self {
            Self::Arguments(field) => {
                out.write_str(",\"field\":")?;
                quoted(out, field)?;
            }
            Self::FileIo(code) | Self::PeerIo(code) => {
                out.write_str(",\"os_code\":")?;
                match code {
                    Some(code) => write!(out, "{code}")?,
                    None => out.write_str("null")?,
                }
            }
            _ => {}
        }
        out.write_str("}")
    }
}

struct Human<'a> {
    error: Refusal,
    principal: &'a str,
    peer: &'a str,
}
impl fmt::Display for Human<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let caller = self.principal;
        let peer = self.peer;
        match self.error.reason {
            Reason::Arguments(field) => write!(out, "{field} is missing or has the wrong type"),
            Reason::Grant => write!(out, "{caller} may not send {peer} requests"),
            Reason::RequestNotOwned => {
                write!(out, "{caller} has no request of {peer}'s to answer by that ID")
            }
            Reason::IdExhausted => write!(out, "request IDs for {peer} are exhausted"),
            Reason::Capacity => out.write_str("broker operation storage is unavailable"),
            Reason::DurableEvidenceUnavailable => {
                out.write_str("required durable evidence is unavailable; no effect was admitted")
            }
            Reason::NoScratch => out.write_str("its caller has no scratch, /tmp, to hand files from"),
            Reason::Path => out.write_str("a handed file must be beneath the caller's /tmp scratch"),
            Reason::PathNul => out.write_str("a handed file path contains NUL"),
            Reason::TooLong => write!(out, "the message or file exceeds {peer}'s admitted channel size"),
            Reason::FileKind => out.write_str("a handed file must be a regular scratch file"),
            Reason::FileIo(code) => write!(out, "the checked scratch file could not be read (OS {code:?})"),
            Reason::PeerFull => write!(out, "{peer} has not taken what was sent it; try again later"),
            Reason::PeerIo(code) => write!(out, "{peer}'s channel did not accept the message (OS {code:?})"),
            Reason::Partial { accepted, expected } => write!(
                out,
                "local channel acceptance is uncertain: {accepted} of {expected} bytes; do not replay this ID"
            ),
        }
    }
}
#[derive(Debug)]
struct ResponseSlot {
    format: Format,
    bytes: String,
}
impl ResponseSlot {
    fn reserve(format: Format, receipt: &Receipt<'_>) -> Result<Self, Refusal> {
        let partial = Reason::Partial {
            accepted: usize::MAX,
            expected: usize::MAX,
        };
        let mut bound = 0;
        // Count complete real outcome shapes, actual escaped identities and maximum
        // native integer representations with the same encoder used after the effect.
        for (outcome, effect) in [
            (Outcome::Completed, Effect::KernelAccepted { bytes: usize::MAX }),
            (
                Outcome::Failed(Refusal::at(Phase::Executing, Reason::Capacity)),
                Effect::NotStarted,
            ),
            (
                Outcome::Failed(Refusal::at(Phase::Executing, Reason::TooLong)),
                Effect::NotStarted,
            ),
            (
                Outcome::Failed(Refusal::at(Phase::Executing, Reason::FileKind)),
                Effect::NotStarted,
            ),
            (
                Outcome::Failed(Refusal::at(Phase::Executing, Reason::FileIo(Some(i32::MIN)))),
                Effect::NotStarted,
            ),
            (
                Outcome::Failed(Refusal::at(Phase::Executing, Reason::PeerFull)),
                Effect::NotAccepted,
            ),
            (
                Outcome::Failed(Refusal::at(Phase::Executing, Reason::PeerIo(Some(i32::MIN)))),
                Effect::NotAccepted,
            ),
            (
                Outcome::Uncertain(Refusal::at(Phase::Executing, partial)),
                Effect::Uncertain {
                    accepted: usize::MAX,
                    expected: usize::MAX,
                },
            ),
        ] {
            let sample = Receipt { effect, ..*receipt };
            let mut count = Count(0);
            format
                .response(&mut count, sample.principal, sample.peer, Some(&sample), outcome)
                .map_err(|_| capacity(Phase::Authorized))?;
            bound = bound.max(count.0);
        }
        let mut bytes = String::new();
        bytes
            .try_reserve_exact(bound)
            .map_err(|_| capacity(Phase::Authorized))?;
        Ok(Self { format, bytes })
    }
    fn render(mut self, receipt: &Receipt<'_>, outcome: Outcome) -> Reply {
        match self.format.response(
            &mut Fixed(&mut self.bytes),
            receipt.principal,
            receipt.peer,
            Some(receipt),
            outcome,
        ) {
            Ok(()) => Reply::Owned(self.bytes),
            Err(_) => self.format.literal(),
        }
    }
}

struct Hand<'a, 'b> {
    root: BorrowedFd<'b>,
    path: std::ffi::CString,
    name: &'a str,
    /// What its message holds before the file's bytes: its kind and ID, and the file's
    /// name and that name's length.
    head: usize,
    limit: usize,
}
impl<'a, 'b> Hand<'a, 'b> {
    fn prepare(root: BorrowedFd<'b>, path: &'a str, message: usize) -> Result<Self, Refusal> {
        let denied = |reason| Refusal::at(Phase::Authorized, reason);
        let rel = path
            .strip_prefix("/tmp/")
            .filter(|rel| !rel.is_empty())
            .ok_or_else(|| denied(Reason::Path))?;
        if rel.as_bytes().contains(&0) {
            return Err(denied(Reason::PathNul));
        }
        let name = rel.rsplit('/').next().unwrap_or(rel);
        let name_bytes = u16::try_from(name.len()).map_err(|_| denied(Reason::TooLong))?;
        let head = crate::server::MESSAGE_HEAD
            .checked_add(std::mem::size_of_val(&name_bytes))
            .and_then(|n| n.checked_add(name.len()))
            .ok_or_else(|| denied(Reason::TooLong))?;
        let limit = message.checked_sub(head).ok_or_else(|| denied(Reason::TooLong))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(rel.len().checked_add(1).ok_or_else(|| denied(Reason::TooLong))?)
            .map_err(|_| capacity(Phase::Authorized))?;
        bytes.extend_from_slice(rel.as_bytes());
        bytes.push(0);
        let path = std::ffi::CString::from_vec_with_nul(bytes).map_err(|_| denied(Reason::PathNul))?;
        Ok(Self {
            root,
            path,
            name,
            head,
            limit,
        })
    }

    fn read(&self, packet: &mut Vec<u8>) -> Result<(), Refusal> {
        use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};
        let failed = |reason| Refusal::at(Phase::Executing, reason);
        // Existing open_how layout and held-scratch resolution policy (D139).
        let how: [u64; 3] = [
            (libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC) as u64,
            0,
            crate::server::RESOLVE_BENEATH
                | crate::server::RESOLVE_NO_SYMLINKS
                | crate::server::RESOLVE_NO_MAGICLINKS
                | crate::server::RESOLVE_NO_XDEV,
        ];
        // SAFETY: held root, owned NUL-terminated relative path, exact open_how layout.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.root.as_raw_fd(),
                self.path.as_ptr(),
                how.as_ptr(),
                std::mem::size_of_val(&how),
            )
        };
        if fd < 0 {
            return Err(failed(Reason::FileIo(
                std::io::Error::last_os_error().raw_os_error(),
            )));
        }
        let fd = RawFd::try_from(fd).map_err(|_| failed(Reason::FileIo(None)))?;
        // SAFETY: the successful native open transferred this descriptor here alone.
        let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        let meta = file
            .metadata()
            .map_err(|e| failed(Reason::FileIo(e.raw_os_error())))?;
        if !meta.is_file() {
            return Err(failed(Reason::FileKind));
        }
        if meta.len() > u64::try_from(self.limit).map_err(|_| failed(Reason::TooLong))? {
            return Err(failed(Reason::TooLong));
        }
        let len = u16::try_from(self.name.len()).map_err(|_| failed(Reason::TooLong))?;
        append(packet, &len.to_be_bytes())?;
        append(packet, self.name.as_bytes())?;
        let head = packet.len();
        // Room for `bytes` of the file and one probe byte past them, which finds it longer.
        let room = |bytes: usize| {
            head.checked_add(bytes)
                .and_then(|n| n.checked_add(1))
                .ok_or_else(|| failed(Reason::TooLong))
        };
        let full = room(self.limit)?;
        // What its size says first: within the limit, checked above.
        let mut end = room(usize::try_from(meta.len()).map_err(|_| failed(Reason::TooLong))?)?;
        let reserve = |packet: &mut Vec<u8>, end: usize| {
            packet
                .try_reserve_exact(end.saturating_sub(packet.len()))
                .map_err(|_| failed(Reason::Capacity))
        };
        reserve(packet, end)?;
        loop {
            let unread = end.saturating_sub(packet.len());
            let spare = packet.spare_capacity_mut();
            // SAFETY: read(2) of the held file into the vector's spare capacity, of no more
            // bytes than that capacity holds.
            let n = unsafe {
                libc::read(
                    file.as_raw_fd(),
                    spare.as_mut_ptr().cast(),
                    unread.min(spare.len()),
                )
            };
            let Ok(n) = usize::try_from(n) else {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(failed(Reason::FileIo(e.raw_os_error())));
            };
            if n == 0 {
                return Ok(());
            }
            let used = packet
                .len()
                .checked_add(n)
                .ok_or_else(|| failed(Reason::TooLong))?;
            // SAFETY: read(2) initialized these n bytes of the spare capacity it was given.
            unsafe { packet.set_len(used) };
            if used.saturating_sub(head) > self.limit {
                return Err(failed(Reason::TooLong));
            }
            if used == end {
                // Longer than its size said, and within the limit: room for all the limit
                // admits, and its probe byte.
                end = full;
                reserve(packet, end)?;
            }
        }
    }
}

fn append(packet: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Refusal> {
    if packet
        .len()
        .checked_add(bytes.len())
        .is_none_or(|n| n > packet.capacity())
    {
        return Err(Refusal::at(Phase::Executing, Reason::TooLong));
    }
    packet.extend_from_slice(bytes);
    Ok(())
}

impl<'a, 'b> Reserved<'a, 'b> {
    pub fn execute(mut self) -> Terminal<'b> {
        let id = self.receipt.request;
        let kind = match self.receipt.operation {
            Operation::Send => crate::server::REQUEST,
            Operation::Answer => crate::server::ANSWER,
            Operation::HandRequest => crate::server::REQUEST_FILE,
            Operation::HandAnswer => crate::server::ANSWER_FILE,
        };
        let prepared = append(&mut self.packet, &[kind])
            .and_then(|()| append(&mut self.packet, &id.to_be_bytes()))
            .and_then(|()| match &self.body {
                ReservedBody::Text(s) => append(&mut self.packet, s.as_bytes()),
                ReservedBody::Hand(hand) => hand.read(&mut self.packet),
            });
        if let Err(error) = prepared {
            return self.failed(error, Effect::NotStarted);
        }
        if self.packet.len() > self.peer.most {
            return self.failed(Refusal::at(Phase::Executing, Reason::TooLong), Effect::NotStarted);
        }
        // SAFETY: complete owned packet, held peer socket; no blocking or SIGPIPE.
        let sent = unsafe {
            libc::send(
                self.peer.fd.as_raw_fd(),
                self.packet.as_ptr().cast(),
                self.packet.len(),
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        let Ok(sent) = usize::try_from(sent) else {
            let code = std::io::Error::last_os_error().raw_os_error();
            return self.failed(
                Refusal::at(
                    Phase::Executing,
                    if code == Some(libc::EAGAIN) {
                        Reason::PeerFull
                    } else {
                        Reason::PeerIo(code)
                    },
                ),
                Effect::NotAccepted,
            );
        };
        // Possible local acceptance consumes answer ownership even if partial;
        // an uncertain effect cannot be retried with the same completion identity.
        if self.receipt.operation.request() {
            self.peer.sent.push(id);
        } else {
            self.peer.asked.retain(|n| *n != id);
        }
        if sent != self.packet.len() {
            let expected = self.packet.len();
            return self.failed(
                Refusal::at(
                    Phase::Executing,
                    Reason::Partial {
                        accepted: sent,
                        expected,
                    },
                ),
                Effect::Uncertain {
                    accepted: sent,
                    expected,
                },
            );
        }
        self.receipt.effect = Effect::KernelAccepted { bytes: sent };
        Terminal {
            receipt: self.receipt,
            outcome: Outcome::Completed,
            response: self.response,
        }
    }
    fn failed(mut self, error: Refusal, effect: Effect) -> Terminal<'b> {
        self.receipt.effect = effect;
        let outcome = if matches!(effect, Effect::Uncertain { .. }) {
            Outcome::Uncertain(error)
        } else {
            Outcome::Failed(error)
        };
        Terminal {
            receipt: self.receipt,
            outcome,
            response: self.response,
        }
    }
}

#[cfg(test)]
pub(crate) fn read_for_test(
    root: Option<BorrowedFd<'_>>,
    path: &str,
    message: usize,
) -> Result<(String, Vec<u8>), Refusal> {
    let root = root.ok_or(Refusal::at(Phase::Authorized, Reason::NoScratch))?;
    let hand = Hand::prepare(root, path, message)?;
    let mut packet = Vec::new();
    packet
        .try_reserve_exact(
            message
                .checked_add(1)
                .ok_or(Refusal::at(Phase::Authorized, Reason::TooLong))?,
        )
        .map_err(|_| capacity(Phase::Authorized))?;
    packet.resize(crate::server::MESSAGE_HEAD, 0);
    hand.read(&mut packet)?;
    let head = crate::server::MESSAGE_HEAD + std::mem::size_of::<u16>() + hand.name.len();
    Ok((
        hand.name.to_string(),
        packet
            .get(head..)
            .ok_or(Refusal::at(Phase::Executing, Reason::TooLong))?
            .to_vec(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsFd as _, FromRawFd as _, OwnedFd};

    fn pair() -> (OwnedFd, OwnedFd) {
        let mut fds = [-1; 2];
        assert_eq!(
            // SAFETY: socketpair writes exactly the two supplied descriptors.
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    fds.as_mut_ptr(),
                )
            },
            0
        );
        // SAFETY: both successful descriptors are transferred here alone.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }
    fn received(fd: &OwnedFd) -> Vec<u8> {
        let mut bytes = vec![0; 1024];
        // SAFETY: writable buffer and held descriptor; never wait for test progress.
        let n = unsafe {
            libc::recv(
                fd.as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                libc::MSG_DONTWAIT,
            )
        };
        assert!(n >= 0, "{}", std::io::Error::last_os_error());
        bytes.truncate(usize::try_from(n).unwrap());
        bytes
    }
    fn empty(fd: &OwnedFd) {
        let mut byte = 0u8;
        assert_eq!(
            // SAFETY: writable byte and held descriptor; no wait.
            unsafe { libc::recv(fd.as_raw_fd(), (&raw mut byte).cast(), 1, libc::MSG_DONTWAIT) },
            -1
        );
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EAGAIN));
    }
    struct Ledger {
        last: u64,
        sent: Vec<u64>,
        asked: Vec<u64>,
    }
    impl Ledger {
        fn peer<'a>(&'a mut self, fd: &'a OwnedFd) -> Peer<'a> {
            Peer {
                label: "agent peer",
                fd: fd.as_fd(),
                send: true,
                answer: true,
                most: 1024,
                last: &mut self.last,
                sent: &mut self.sent,
                asked: &mut self.asked,
            }
        }
    }
    fn context() -> Context<'static> {
        Context {
            principal: "agent caller",
            scratch: None,
            evidence: Evidence::MemoryOnly,
        }
    }
    fn parse_args(bytes: &[u8]) -> Value {
        crate::server::json::parse(bytes).unwrap()
    }

    #[test]
    fn broker_reservation_precedes_actual_send_and_retains_local_receipt() {
        let (fd, other) = pair();
        let mut ledger = Ledger {
            last: 0,
            sent: vec![],
            asked: vec![],
        };
        let args = parse_args(br#"{"to":"agent peer","text":"hi","principal":"forged"}"#);
        let request = Request::receive("send", Some(&args)).unwrap().unwrap();
        let reserved = request
            .authorize(context(), Some(ledger.peer(&fd)))
            .unwrap()
            .reserve(Format::Program)
            .unwrap();
        assert_eq!(reserved.receipt.principal, "agent caller");
        assert_eq!(reserved.receipt.peer, "agent peer");
        assert_eq!(reserved.receipt.operation, Operation::Send);
        assert_eq!(reserved.receipt.request, 1);
        assert_eq!(reserved.receipt.effect, Effect::NotStarted);
        empty(&other);
        let terminal = reserved.execute();
        assert_eq!(terminal.outcome, Outcome::Completed);
        assert_eq!(
            terminal.receipt.effect,
            Effect::KernelAccepted {
                bytes: crate::server::MESSAGE_HEAD + 2
            }
        );
        let reply = parse_args(terminal.present().as_bytes());
        assert_eq!(
            reply.get("value").and_then(|v| v.get("id")).and_then(Value::u64),
            Some(1)
        );
        let metadata = reply.get("broker").unwrap();
        assert_eq!(
            metadata.get("principal").and_then(Value::str),
            Some("agent caller")
        );
        assert_eq!(metadata.get("operation").and_then(Value::str), Some("send"));
        assert_eq!(metadata.get("request").and_then(Value::u64), Some(1));
        assert_eq!(metadata.get("outcome").and_then(Value::str), Some("completed"));
        let effect = metadata.get("effect").unwrap();
        assert_eq!(effect.get("kind").and_then(Value::str), Some("kernel_accepted"));
        assert_eq!(
            effect.get("bytes").and_then(Value::u64),
            Some((crate::server::MESSAGE_HEAD + 2) as u64)
        );
        let mut expected = vec![crate::server::REQUEST];
        expected.extend_from_slice(&1u64.to_be_bytes());
        expected.extend_from_slice(b"hi");
        assert_eq!(received(&other), expected);
        assert_eq!(ledger.last, 1);
        assert_eq!(ledger.sent, [1]);
    }

    #[test]
    fn broker_rejects_bad_arguments_ownership_and_exhausted_identity_before_send() {
        for (name, bytes, field) in [
            ("send", &br#"{"to":"agent peer"}"#[..], "text"),
            (
                "hand",
                &br#"{"to":"agent peer","path":"/tmp/x","id":"bad"}"#[..],
                "id",
            ),
            ("answer", &br#"{"to":"agent peer","text":"x"}"#[..], "id"),
        ] {
            let args = parse_args(bytes);
            let error = Request::receive(name, Some(&args)).unwrap().err().unwrap();
            assert_eq!(error, Refusal::at(Phase::Received, Reason::Arguments(field)));
        }
        let (fd, other) = pair();
        let mut ledger = Ledger {
            last: u64::MAX,
            sent: vec![],
            asked: vec![],
        };
        let args = parse_args(br#"{"to":"agent peer","text":"x"}"#);
        let error = Request::receive("send", Some(&args))
            .unwrap()
            .unwrap()
            .authorize(context(), Some(ledger.peer(&fd)))
            .unwrap()
            .reserve(Format::Program)
            .err()
            .unwrap();
        assert_eq!(error.reason, Reason::IdExhausted);
        assert_eq!(ledger.last, u64::MAX);
        empty(&other);
        let args = parse_args(br#"{"to":"agent peer","text":"x","id":7}"#);
        let error = Request::receive("answer", Some(&args))
            .unwrap()
            .unwrap()
            .authorize(context(), Some(ledger.peer(&fd)))
            .err()
            .unwrap();
        assert_eq!(error.reason, Reason::RequestNotOwned);
        empty(&other);
    }

    #[test]
    fn broker_completion_consumes_only_the_owned_request_once() {
        let (fd, other) = pair();
        let mut ledger = Ledger {
            last: 3,
            sent: vec![3],
            asked: vec![7],
        };
        let args = parse_args(br#"{"to":"agent peer","text":"answer","id":7}"#);
        fn submit<'a>(
            ledger: &'a mut Ledger,
            fd: &'a OwnedFd,
            args: &Value,
        ) -> Result<Terminal<'a>, Refusal> {
            Request::receive("answer", Some(args))
                .unwrap()
                .unwrap()
                .authorize(context(), Some(ledger.peer(fd)))
                .and_then(|a| a.reserve(Format::Program))
                .map(Reserved::execute)
        }
        let terminal = submit(&mut ledger, &fd, &args).unwrap();
        assert_eq!(terminal.outcome, Outcome::Completed);
        assert_eq!(terminal.receipt.operation, Operation::Answer);
        let reply = parse_args(terminal.present().as_bytes());
        assert!(matches!(reply.get("value"), Some(Value::Object(fields)) if fields.is_empty()));
        assert_eq!(received(&other).first(), Some(&crate::server::ANSWER));
        assert!(ledger.asked.is_empty());
        assert_eq!(ledger.sent, [3]);
        assert_eq!(
            submit(&mut ledger, &fd, &args).err().unwrap().reason,
            Reason::RequestNotOwned
        );
        empty(&other);
    }

    #[test]
    fn broker_local_send_failure_preserves_cause_without_reusing_identity() {
        let (fd, other) = pair();
        let mut ledger = Ledger {
            last: 0,
            sent: vec![],
            asked: vec![],
        };
        let args = parse_args(br#"{"to":"agent peer","text":"hi"}"#);
        let reserved = Request::receive("send", Some(&args))
            .unwrap()
            .unwrap()
            .authorize(context(), Some(ledger.peer(&fd)))
            .unwrap()
            .reserve(Format::Mcp)
            .unwrap();
        drop(other);
        let terminal = reserved.execute();
        let Outcome::Failed(error) = terminal.outcome else {
            panic!("{terminal:?}");
        };
        assert!(matches!(error.reason, Reason::PeerIo(Some(_))));
        let receipt = terminal.receipt;
        assert_eq!(receipt.effect, Effect::NotAccepted);
        assert_eq!(receipt.request, 1);
        let reply = terminal.present();
        assert!(reply.as_str().contains("did not accept"));
        let json = parse_args(reply.as_bytes());
        let metadata = json.get("structuredContent").unwrap();
        assert_eq!(metadata.get("outcome").and_then(Value::str), Some("failed"));
        assert_eq!(metadata.get("phase").and_then(Value::str), Some("executing"));
        assert_eq!(
            metadata
                .get("effect")
                .and_then(|e| e.get("kind"))
                .and_then(Value::str),
            Some("not_accepted")
        );
        assert_eq!(
            metadata
                .get("cause")
                .and_then(|e| e.get("kind"))
                .and_then(Value::str),
            Some("peer_io")
        );
        let Reason::PeerIo(code) = error.reason else {
            panic!("{error:?}");
        };
        assert_eq!(
            metadata
                .get("cause")
                .and_then(|e| e.get("os_code"))
                .and_then(Value::u64),
            code.and_then(|c| u64::try_from(c).ok())
        );
        assert_eq!(ledger.last, 1);
        assert!(ledger.sent.is_empty());
    }

    #[test]
    fn broker_required_durable_refuses_before_opening_or_sending() {
        let (fd, other) = pair();
        let tmp = shards_testdir::TempDir::new("broker").unwrap();
        let root = std::fs::File::open(&*tmp).unwrap();
        let mut ledger = Ledger {
            last: 0,
            sent: vec![],
            asked: vec![],
        };
        let args = parse_args(br#"{"to":"agent peer","path":"/tmp/does-not-exist"}"#);
        let context = Context {
            principal: "agent caller",
            scratch: Some(root.as_fd()),
            evidence: Evidence::RequiredDurable,
        };
        let error = Request::receive("hand", Some(&args))
            .unwrap()
            .unwrap()
            .authorize(context, Some(ledger.peer(&fd)))
            .err()
            .unwrap();
        assert_eq!(error.reason, Reason::DurableEvidenceUnavailable);
        assert_eq!(ledger.last, 0);
        empty(&other);
        assert!(!tmp.join("does-not-exist").exists());
    }

    /// A refusal is made before any effect: one whose answer finds no storage still says it
    /// was refused, no effect begun, as the run tool's refusals do.
    #[test]
    fn a_refusal_without_storage_says_it_was_refused() {
        let refusal = Refusal::at(Phase::Validated, Reason::Grant);
        for format in [Format::Program, Format::Mcp] {
            // Each one's first allocation refused: that of its answer.
            let (refused, _) = crate::program_admission_allocation::during(1, || {
                format.refused(refusal, "agent caller", "agent peer")
            });
            let (displayed, _) = crate::program_admission_allocation::during(1, || {
                format.display_refusal(format_args!("code mode is not available here"))
            });
            for (path, reply) in [("refused", refused), ("display_refusal", displayed)] {
                let literal = matches!(reply, Reply::Literal(_));
                let json = parse_args(reply.as_bytes());
                let metadata = json
                    .get(match format {
                        Format::Program => "broker",
                        Format::Mcp => "structuredContent",
                    })
                    .unwrap();
                let outcome = metadata.get("outcome").and_then(Value::str);
                let effect = metadata
                    .get("effect")
                    .and_then(|e| e.get("kind"))
                    .and_then(Value::str);
                eprintln!(
                    "REFUSAL_FALLBACK format={format:?} path={path} literal={literal} outcome={outcome:?} effect={effect:?} reply={}",
                    reply.as_str()
                );
                assert!(literal, "{path}: {reply}");
                assert_eq!(outcome, Some("refused"), "{path}: {reply}");
                assert_eq!(effect, Some("not_started"), "{path}: {reply}");
            }
        }
    }

    /// What one message on `fd` holds, as the instance reads it (`server::message_most`):
    /// its SO_SNDBUF less the 32 bytes the kernel keeps of it.
    fn channel_most(fd: &OwnedFd) -> usize {
        let mut size: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: getsockopt(2) of an int into one of ours, its length given.
        let rc = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&raw mut size).cast(),
                &raw mut len,
            )
        };
        assert_eq!(rc, 0);
        usize::try_from(size).unwrap() - 32
    }

    /// A hand of a small file holds what the file does, and its probe byte: not what the
    /// largest message to its peer would.
    #[test]
    fn a_small_hand_reserves_what_its_file_holds() {
        let (fd, _other) = pair();
        let tmp = shards_testdir::TempDir::new("hand-size").unwrap();
        std::fs::write(tmp.join("small"), b"ten bytes!").unwrap();
        let root = std::fs::File::open(&*tmp).unwrap();
        let mut ledger = Ledger {
            last: 0,
            sent: vec![],
            asked: vec![],
        };
        let mut peer = ledger.peer(&fd);
        peer.most = channel_most(&fd);
        let most = peer.most;
        let context = Context {
            principal: "agent caller",
            scratch: Some(root.as_fd()),
            evidence: Evidence::MemoryOnly,
        };
        let args = parse_args(br#"{"to":"agent peer","path":"/tmp/small"}"#);
        let mut reserved = Request::receive("hand", Some(&args))
            .unwrap()
            .unwrap()
            .authorize(context, Some(peer))
            .unwrap()
            .reserve(Format::Program)
            .unwrap();
        let at_admission = reserved.packet.capacity();
        // As execute makes it: its kind and ID, then the file's name and bytes.
        append(&mut reserved.packet, &[crate::server::REQUEST_FILE]).unwrap();
        append(&mut reserved.packet, &1u64.to_be_bytes()).unwrap();
        let ReservedBody::Hand(hand) = &reserved.body else {
            panic!("a hand reserves no file");
        };
        hand.read(&mut reserved.packet).unwrap();
        let packet = crate::server::MESSAGE_HEAD + 2 + "small".len() + 10;
        eprintln!(
            "HAND_RESERVATION channel_most={most} file_bytes=10 packet_len={} reserved_at_admission={at_admission} capacity_after_read={}",
            reserved.packet.len(),
            reserved.packet.capacity()
        );
        assert_eq!(reserved.packet.len(), packet);
        assert!(
            reserved.packet.capacity() <= packet + 1,
            "a {packet}-byte hand held {} bytes",
            reserved.packet.capacity()
        );
    }

    /// A file that holds more than its size says (procfs says 0 bytes of each, and reads it
    /// whole) is handed whole within its peer's limit, and refused past it, as before.
    #[test]
    fn a_file_longer_than_its_size_says_is_handed_as_before() {
        let (fd, other) = pair();
        let root = std::fs::File::open("/proc/self").unwrap();
        assert_eq!(std::fs::metadata("/proc/self/status").unwrap().len(), 0);
        let head = crate::server::MESSAGE_HEAD + 2 + "status".len();
        let mut ledger = Ledger {
            last: 0,
            sent: vec![],
            asked: vec![],
        };
        for (limit, whole) in [(channel_most(&fd) - head, true), (16, false)] {
            let mut peer = ledger.peer(&fd);
            peer.most = head + limit;
            let context = Context {
                principal: "agent caller",
                scratch: Some(root.as_fd()),
                evidence: Evidence::MemoryOnly,
            };
            let args = parse_args(br#"{"to":"agent peer","path":"/tmp/status"}"#);
            let terminal = Request::receive("hand", Some(&args))
                .unwrap()
                .unwrap()
                .authorize(context, Some(peer))
                .unwrap()
                .reserve(Format::Program)
                .unwrap()
                .execute();
            eprintln!(
                "HAND_GROWN limit={limit} outcome={:?} effect={:?}",
                terminal.outcome, terminal.receipt.effect
            );
            if whole {
                assert_eq!(terminal.outcome, Outcome::Completed);
                let got = received(&other);
                let bytes = &got[head..];
                assert!(
                    bytes.starts_with(b"Name:"),
                    "{:?}",
                    String::from_utf8_lossy(bytes)
                );
                assert!(bytes.len() > 16, "{}", bytes.len());
            } else {
                assert_eq!(
                    terminal.outcome,
                    Outcome::Failed(Refusal::at(Phase::Executing, Reason::TooLong))
                );
                empty(&other);
            }
        }
    }

    #[test]
    fn broker_prebuilt_error_capacity_covers_actual_escaped_completion() {
        for format in [Format::Program, Format::Mcp] {
            for reason in [
                Reason::FileIo(None),
                Reason::FileIo(Some(i32::MIN)),
                Reason::PeerIo(Some(i32::MAX)),
                Reason::Partial {
                    accepted: usize::MAX,
                    expected: usize::MAX,
                },
            ] {
                let error = Refusal::at(Phase::Executing, reason);
                let principal = "caller\"\n\\\u{0}";
                let peer = "peer世界\"\n";
                let mut receipt = Receipt {
                    principal,
                    peer,
                    operation: Operation::HandRequest,
                    request: u64::MAX,
                    effect: Effect::NotStarted,
                };
                let slot = ResponseSlot::reserve(format, &receipt).unwrap();
                let outcome = match reason {
                    Reason::Partial { accepted, expected } => {
                        receipt.effect = Effect::Uncertain { accepted, expected };
                        Outcome::Uncertain(error)
                    }
                    Reason::PeerIo(_) => {
                        receipt.effect = Effect::NotAccepted;
                        Outcome::Failed(error)
                    }
                    _ => Outcome::Failed(error),
                };
                let reply = slot.render(&receipt, outcome);
                assert!(matches!(reply, Reply::Owned(_)));
                let json = parse_args(reply.as_bytes());
                let metadata = json
                    .get(match format {
                        Format::Program => "broker",
                        Format::Mcp => "structuredContent",
                    })
                    .unwrap();
                assert_eq!(metadata.get("principal").and_then(Value::str), Some(principal));
                assert_eq!(metadata.get("request").and_then(Value::u64), Some(u64::MAX));
                if matches!(reason, Reason::Partial { .. }) {
                    assert_eq!(metadata.get("outcome").and_then(Value::str), Some("uncertain"));
                    assert_eq!(
                        metadata
                            .get("effect")
                            .and_then(|e| e.get("kind"))
                            .and_then(Value::str),
                        Some("uncertain")
                    );
                    assert_eq!(
                        metadata
                            .get("cause")
                            .and_then(|e| e.get("kind"))
                            .and_then(Value::str),
                        Some("short_send")
                    );
                }
            }
        }
    }
}
