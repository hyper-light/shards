//! The user's service manager, asked over its message bus to make a process the one
//! process of a scope of its own in a slice (D183): systemd's `StartTransientUnit`
//! (org.freedesktop.systemd1(5)), waited for to its `JobRemoved`. The bus is spoken as
//! the D-Bus Specification 0.43 has it, as much as that takes: EXTERNAL authentication as
//! the process's own uid, messages written little-endian and read in either order, a
//! method call and its reply, and the signals a match rule asks for. Std alone, so that
//! the harness measuring it (docs/research/measurements/cgroup-parent) runs the very code
//! the daemon does.

#![cfg(unix)]

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStringExt as _;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// The most a message may be, its header, padding and body (the Specification, "Message
/// Format"); an authentication line is held to it too.
const MAX_MESSAGE: usize = 1 << 27;
/// The most an array's elements may be (the Specification, "Marshalling containers").
const MAX_ARRAY: usize = 1 << 26;
/// How deep containers may nest (the Specification, "Valid Signatures").
const MAX_DEPTH: usize = 64;

const BUS: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const MANAGER: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";

const METHOD_CALL: u8 = 1;
const METHOD_RETURN: u8 = 2;
const ERROR: u8 = 3;
const SIGNAL: u8 = 4;

/// A value in a message, as this client writes and reads them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Byte(u8),
    Bool(bool),
    U32(u32),
    Str(String),
    Path(String),
    Signature(String),
    /// Of the type its signature names, and its elements.
    Array(String, Vec<Value>),
    Struct(Vec<Value>),
    Variant(Box<Value>),
    /// A value of a type this client reads past and keeps nothing of.
    Other,
}

impl Value {
    fn signature(&self) -> String {
        match self {
            Value::Byte(_) => "y".to_string(),
            Value::Bool(_) => "b".to_string(),
            Value::U32(_) => "u".to_string(),
            Value::Str(_) => "s".to_string(),
            Value::Path(_) => "o".to_string(),
            Value::Signature(_) => "g".to_string(),
            Value::Array(of, _) => format!("a{of}"),
            Value::Struct(fields) => format!("({})", fields.iter().map(Value::signature).collect::<String>()),
            Value::Variant(_) => "v".to_string(),
            Value::Other => String::new(),
        }
    }
}

/// A message as it is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub kind: u8,
    pub serial: u32,
    pub reply_to: Option<u32>,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error: Option<String>,
    pub body: Vec<Value>,
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("the bus sent {what}"))
}

fn too_big(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{what} is past what a message may hold"),
    )
}

/// Where the type a signature starts with is aligned.
fn alignment(code: u8) -> usize {
    match code {
        b'n' | b'q' => 2,
        b'b' | b'i' | b'u' | b'h' | b's' | b'o' | b'a' => 4,
        b'x' | b't' | b'd' | b'(' | b'{' => 8,
        _ => 1,
    }
}

/// How long the complete type `sig` starts with is.
fn type_len(sig: &[u8], depth: usize) -> io::Result<usize> {
    if depth > MAX_DEPTH {
        return Err(invalid("a signature nested past 64 containers"));
    }
    match sig.first() {
        Some(b'a') => Ok(1 + type_len(sig.get(1..).unwrap_or_default(), depth + 1)?),
        Some(&open @ (b'(' | b'{')) => {
            let close = if open == b'(' { b')' } else { b'}' };
            let mut at = 1;
            loop {
                match sig.get(at) {
                    Some(&c) if c == close => return Ok(at + 1),
                    Some(_) => at += type_len(sig.get(at..).unwrap_or_default(), depth + 1)?,
                    None => return Err(invalid("a signature cut short")),
                }
            }
        }
        Some(_) => Ok(1),
        None => Err(invalid("a signature cut short")),
    }
}

/// Each complete type of `sig`, in order.
fn types(sig: &[u8]) -> io::Result<Vec<&[u8]>> {
    let mut all = Vec::new();
    let mut rest = sig;
    while !rest.is_empty() {
        let len = type_len(rest, 0)?;
        let (one, after) = rest
            .split_at_checked(len)
            .ok_or_else(|| invalid("a signature cut short"))?;
        all.push(one);
        rest = after;
    }
    Ok(all)
}

#[derive(Debug, Default)]
struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    fn pad(&mut self, to: usize) {
        while !self.buf.len().is_multiple_of(to) {
            self.buf.push(0);
        }
    }

    fn u32(&mut self, v: u32) {
        self.pad(4);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn string(&mut self, s: &str) -> io::Result<()> {
        if s.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a string with a NUL in it",
            ));
        }
        self.u32(u32::try_from(s.len()).map_err(|_| too_big("a string"))?);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
        Ok(())
    }

    fn signature(&mut self, s: &str) -> io::Result<()> {
        self.buf
            .push(u8::try_from(s.len()).map_err(|_| too_big("a signature"))?);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
        Ok(())
    }

    fn value(&mut self, v: &Value) -> io::Result<()> {
        match v {
            Value::Byte(b) => self.buf.push(*b),
            Value::Bool(b) => self.u32(u32::from(*b)),
            Value::U32(n) => self.u32(*n),
            Value::Str(s) | Value::Path(s) => self.string(s)?,
            Value::Signature(s) => self.signature(s)?,
            Value::Array(of, items) => {
                self.pad(4);
                let at = self.buf.len();
                self.buf.extend_from_slice(&[0; 4]);
                self.pad(alignment(of.as_bytes().first().copied().unwrap_or(b'y')));
                let start = self.buf.len();
                for item in items {
                    self.value(item)?;
                }
                let len = self.buf.len().saturating_sub(start);
                if len > MAX_ARRAY {
                    return Err(too_big("an array"));
                }
                let len = u32::try_from(len).map_err(|_| too_big("an array"))?;
                if let Some(slot) = self.buf.get_mut(at..at + 4) {
                    slot.copy_from_slice(&len.to_le_bytes());
                }
            }
            Value::Struct(fields) => {
                self.pad(8);
                for f in fields {
                    self.value(f)?;
                }
            }
            Value::Variant(inner) => {
                self.signature(&inner.signature())?;
                self.value(inner)?;
            }
            Value::Other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a value of no type to write",
                ));
            }
        }
        Ok(())
    }
}

/// A header field: its code, and its value.
fn field(code: u8, v: Value) -> Value {
    Value::Struct(vec![Value::Byte(code), Value::Variant(Box::new(v))])
}

/// A message of `kind` and `serial`, with `fields` and `body`, as it is sent.
fn message(kind: u8, serial: u32, mut fields: Vec<Value>, body: &[Value]) -> io::Result<Vec<u8>> {
    let mut b = Writer::default();
    for v in body {
        b.value(v)?;
    }
    let sig: String = body.iter().map(Value::signature).collect();
    if !sig.is_empty() {
        fields.push(field(8, Value::Signature(sig)));
    }
    let mut h = Writer::default();
    h.buf.extend_from_slice(&[b'l', kind, 0, 1]);
    h.u32(u32::try_from(b.buf.len()).map_err(|_| too_big("a body"))?);
    h.u32(serial);
    h.value(&Value::Array("(yv)".to_string(), fields))?;
    h.pad(8);
    h.buf.extend_from_slice(&b.buf);
    if h.buf.len() > MAX_MESSAGE {
        return Err(too_big("a message"));
    }
    Ok(h.buf)
}

#[derive(Debug)]
struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
    big: bool,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| invalid("a message cut short"))?;
        let got = self
            .buf
            .get(self.at..end)
            .ok_or_else(|| invalid("a message cut short"))?;
        self.at = end;
        Ok(got)
    }

    fn pad(&mut self, to: usize) -> io::Result<()> {
        let over = self.at % to;
        if over != 0 {
            self.take(to - over)?;
        }
        Ok(())
    }

    fn byte(&mut self) -> io::Result<u8> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| invalid("a message cut short"))
    }

    fn u32(&mut self) -> io::Result<u32> {
        self.pad(4)?;
        let b: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| invalid("a message cut short"))?;
        Ok(if self.big {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    }

    fn text(&mut self, len: usize) -> io::Result<String> {
        let bytes = self.take(len)?.to_vec();
        if self.byte()? != 0 {
            return Err(invalid("a string not ended by a NUL"));
        }
        String::from_utf8(bytes).map_err(|_| invalid("a string that is not UTF-8"))
    }

    /// A value of the complete type `sig`.
    fn value(&mut self, sig: &[u8], depth: usize) -> io::Result<Value> {
        if depth > MAX_DEPTH {
            return Err(invalid("a value nested past 64 containers"));
        }
        let code = sig
            .first()
            .copied()
            .ok_or_else(|| invalid("a value of no type"))?;
        Ok(match code {
            b'y' => Value::Byte(self.byte()?),
            b'b' => Value::Bool(self.u32()? != 0),
            b'u' => Value::U32(self.u32()?),
            b'i' | b'h' => {
                self.u32()?;
                Value::Other
            }
            b'n' | b'q' => {
                self.pad(2)?;
                self.take(2)?;
                Value::Other
            }
            b'x' | b't' | b'd' => {
                self.pad(8)?;
                self.take(8)?;
                Value::Other
            }
            b's' | b'o' => {
                let len = usize::try_from(self.u32()?).map_err(|_| invalid("a string too long"))?;
                let s = self.text(len)?;
                if code == b's' {
                    Value::Str(s)
                } else {
                    Value::Path(s)
                }
            }
            b'g' => {
                let len = usize::from(self.byte()?);
                Value::Signature(self.text(len)?)
            }
            b'v' => {
                let len = usize::from(self.byte()?);
                let inner = self.text(len)?;
                let one = types(inner.as_bytes())?;
                let [t] = one.as_slice() else {
                    return Err(invalid("a variant of other than one type"));
                };
                Value::Variant(Box::new(self.value(t, depth + 1)?))
            }
            b'a' => {
                let of = sig.get(1..).unwrap_or_default();
                let of = of
                    .get(..type_len(of, depth + 1)?)
                    .ok_or_else(|| invalid("a signature cut short"))?;
                let len = usize::try_from(self.u32()?).map_err(|_| invalid("an array too long"))?;
                if len > MAX_ARRAY {
                    return Err(invalid("an array past 64 MiB"));
                }
                self.pad(alignment(of.first().copied().unwrap_or(b'y')))?;
                let end = self
                    .at
                    .checked_add(len)
                    .ok_or_else(|| invalid("a message cut short"))?;
                let mut items = Vec::new();
                while self.at < end {
                    items.push(self.value(of, depth + 1)?);
                }
                if self.at != end {
                    return Err(invalid("an array whose elements overrun it"));
                }
                Value::Array(String::from_utf8_lossy(of).into_owned(), items)
            }
            b'(' | b'{' => {
                let whole = sig
                    .get(..type_len(sig, depth)?)
                    .ok_or_else(|| invalid("a signature cut short"))?;
                let inner = whole.get(1..whole.len().saturating_sub(1)).unwrap_or_default();
                self.pad(8)?;
                let mut fields = Vec::new();
                for t in types(inner)? {
                    fields.push(self.value(t, depth + 1)?);
                }
                Value::Struct(fields)
            }
            _ => return Err(invalid("a value of a type the Specification does not name")),
        })
    }
}

/// The next message on `r`.
fn read_message(r: &mut impl Read) -> io::Result<Message> {
    let mut fixed = [0u8; 16];
    r.read_exact(&mut fixed)?;
    let big = match fixed.first() {
        Some(b'l') => false,
        Some(b'B') => true,
        _ => return Err(invalid("a message of no byte order")),
    };
    let mut head = Reader {
        buf: &fixed,
        at: 4,
        big,
    };
    let body_len = usize::try_from(head.u32()?).map_err(|_| invalid("a body too long"))?;
    let serial = head.u32()?;
    let fields_len = usize::try_from(head.u32()?).map_err(|_| invalid("header fields too long"))?;
    let header_len = 16usize
        .checked_add(fields_len)
        .and_then(|n| n.checked_next_multiple_of(8))
        .ok_or_else(|| invalid("header fields too long"))?;
    let total = header_len
        .checked_add(body_len)
        .ok_or_else(|| invalid("a body too long"))?;
    if total > MAX_MESSAGE {
        return Err(invalid("a message past 128 MiB"));
    }
    let mut buf = fixed.to_vec();
    buf.resize(total, 0);
    r.read_exact(buf.get_mut(16..).unwrap_or_default())?;
    let mut rd = Reader {
        buf: &buf,
        at: 12,
        big,
    };
    let fields = rd.value(b"a(yv)", 0)?;
    rd.pad(8)?;
    let mut m = Message {
        kind: fixed.get(1).copied().unwrap_or_default(),
        serial,
        reply_to: None,
        path: None,
        interface: None,
        member: None,
        error: None,
        body: Vec::new(),
    };
    let mut sig = String::new();
    if let Value::Array(_, items) = fields {
        for f in items {
            let Value::Struct(parts) = f else { continue };
            let [Value::Byte(code), Value::Variant(v)] = parts.as_slice() else {
                continue;
            };
            match (code, v.as_ref()) {
                (1, Value::Path(p)) => m.path = Some(p.clone()),
                (2, Value::Str(s)) => m.interface = Some(s.clone()),
                (3, Value::Str(s)) => m.member = Some(s.clone()),
                (4, Value::Str(s)) => m.error = Some(s.clone()),
                (5, Value::U32(n)) => m.reply_to = Some(*n),
                (8, Value::Signature(s)) => sig.clone_from(s),
                _ => {}
            }
        }
    }
    for t in types(sig.as_bytes())? {
        m.body.push(rd.value(t, 0)?);
    }
    if rd.at != total {
        return Err(invalid("a body other than its signature says"));
    }
    Ok(m)
}

/// A line of the authentication conversation, its CRLF taken off.
fn line(r: &mut impl Read) -> io::Result<String> {
    let mut got = Vec::new();
    let mut byte = [0u8; 1];
    while !got.ends_with(b"\r\n") {
        r.read_exact(&mut byte)?;
        got.extend_from_slice(&byte);
        if got.len() > MAX_MESSAGE {
            return Err(invalid("an authentication line past 128 MiB"));
        }
    }
    got.truncate(got.len().saturating_sub(2));
    String::from_utf8(got).map_err(|_| invalid("an authentication line that is not UTF-8"))
}

/// A connection to a message bus, authenticated and named by it (`Hello`).
#[derive(Debug)]
pub struct Connection<S> {
    stream: S,
    serial: u32,
    /// Signals read while waiting for a reply, for [`Self::signal`].
    held: Vec<Message>,
}

impl<S: Read + Write> Connection<S> {
    /// Authenticated as `uid` over `stream`, connected to the bus, which names it.
    pub fn open(mut stream: S, uid: u32) -> io::Result<Self> {
        // The authorization identity: the uid in ASCII decimal, hex-encoded (the
        // Specification, "Authentication mechanisms", EXTERNAL), after the one NUL byte a
        // client starts with.
        let id: String = uid.to_string().bytes().map(|b| format!("{b:02x}")).collect();
        stream.write_all(format!("\0AUTH EXTERNAL {id}\r\n").as_bytes())?;
        let said = line(&mut stream)?;
        if !said.starts_with("OK ") {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the bus took no credentials of uid {uid}: {said}"),
            ));
        }
        stream.write_all(b"BEGIN\r\n")?;
        let mut c = Connection {
            stream,
            serial: 0,
            held: Vec::new(),
        };
        c.call(BUS, BUS_PATH, BUS, "Hello", &[])?;
        Ok(c)
    }

    /// `member` of `interface` at `path` of `destination`, called with `args`: its reply's
    /// body, or the error it answered with.
    pub fn call(
        &mut self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        args: &[Value],
    ) -> io::Result<Vec<Value>> {
        self.serial = self.serial.checked_add(1).unwrap_or(1);
        let serial = self.serial;
        let fields = vec![
            field(1, Value::Path(path.to_string())),
            field(2, Value::Str(interface.to_string())),
            field(3, Value::Str(member.to_string())),
            field(6, Value::Str(destination.to_string())),
        ];
        self.stream
            .write_all(&message(METHOD_CALL, serial, fields, args)?)?;
        loop {
            let m = read_message(&mut self.stream)?;
            match m.kind {
                METHOD_RETURN if m.reply_to == Some(serial) => return Ok(m.body),
                ERROR if m.reply_to == Some(serial) => {
                    let why = match m.body.first() {
                        Some(Value::Str(s)) => s.clone(),
                        _ => String::new(),
                    };
                    return Err(io::Error::other(format!(
                        "{member}: {} {why}",
                        m.error.as_deref().unwrap_or("an error")
                    )));
                }
                SIGNAL => self.held.push(m),
                _ => {}
            }
        }
    }

    /// The next signal `wanted` takes, of those held or those to come; the others held.
    pub fn signal(&mut self, wanted: impl Fn(&Message) -> bool) -> io::Result<Message> {
        if let Some(at) = self.held.iter().position(&wanted) {
            return Ok(self.held.remove(at));
        }
        loop {
            let m = read_message(&mut self.stream)?;
            if m.kind == SIGNAL {
                if wanted(&m) {
                    return Ok(m);
                }
                self.held.push(m);
            }
        }
    }
}

/// The path of the user's bus: `DBUS_SESSION_BUS_ADDRESS`'s first `unix:path=` address
/// (the Specification, "Server Addresses"), or the user's runtime directory's `bus`, the
/// systemd user manager's: `XDG_RUNTIME_DIR`, or `/run/user/UID`, which pam_systemd(8)
/// sets it to.
pub fn user_bus_path(uid: u32) -> PathBuf {
    if let Some(path) =
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").and_then(|a| unix_path(&a.to_string_lossy()))
    {
        return path;
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("bus"),
        _ => PathBuf::from(format!("/run/user/{uid}/bus")),
    }
}

/// The path of an address's first `unix:` transport with a `path`, its value unescaped.
fn unix_path(address: &str) -> Option<PathBuf> {
    for one in address.split(';') {
        let Some(keys) = one.strip_prefix("unix:") else {
            continue;
        };
        for kv in keys.split(',') {
            if let Some(v) = kv.strip_prefix("path=") {
                return unescaped(v).map(|b| PathBuf::from(OsString::from_vec(b)));
            }
        }
    }
    None
}

/// An address value's bytes, each `%XX` the byte it names.
fn unescaped(v: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut bytes = v.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = char::from(bytes.next()?).to_digit(16)?;
            let lo = char::from(bytes.next()?).to_digit(16)?;
            out.push(u8::try_from(hi * 16 + lo).ok()?);
        } else {
            out.push(b);
        }
    }
    Some(out)
}

/// The user's bus, connected and authenticated as `uid`.
pub fn user_bus(uid: u32) -> io::Result<Connection<UnixStream>> {
    let path = user_bus_path(uid);
    let stream = UnixStream::connect(&path)
        .map_err(|e| io::Error::new(e.kind(), format!("the user's bus at {}: {e}", path.display())))?;
    Connection::open(stream, uid)
}

/// Asks the bus for the manager's `JobRemoved` signals, before any job of this
/// connection's is started: one, sent to the client that started its job (systemd's
/// src/core/dbus-job.c), may come at once.
pub fn watch_jobs<S: Read + Write>(bus: &mut Connection<S>) -> io::Result<()> {
    bus.call(
        BUS,
        BUS_PATH,
        BUS,
        "AddMatch",
        &[Value::Str(format!(
            "type='signal',sender='{MANAGER}',path='{MANAGER_PATH}',interface='{MANAGER_INTERFACE}',member='JobRemoved'"
        ))],
    )
    .map(drop)
}

/// Asks the user's service manager to start a new scope `unit` in `slice` whose one
/// process is `pid` (`StartTransientUnit`), the scope's cgroup delegated to its processes
/// where `delegate` says (systemd.resource-control(5), `Delegate=`): the path of the job
/// that starts it. Where the manager may not move `pid` itself, it asks the system's
/// manager to (systemd's src/core/cgroup.c, `unit_attach_pids_to_cgroup`), which moves only
/// a process of the uid that asks. The scope goes once its processes have, failed or not
/// (`CollectMode`).
pub fn start_scope<S: Read + Write>(
    bus: &mut Connection<S>,
    unit: &str,
    slice: &str,
    pid: u32,
    description: &str,
    delegate: bool,
) -> io::Result<String> {
    let property =
        |name: &str, v: Value| Value::Struct(vec![Value::Str(name.to_string()), Value::Variant(Box::new(v))]);
    let mut properties = vec![
        property("PIDs", Value::Array("u".to_string(), vec![Value::U32(pid)])),
        property("Slice", Value::Str(slice.to_string())),
        property("Description", Value::Str(description.to_string())),
        property("CollectMode", Value::Str("inactive-or-failed".to_string())),
    ];
    if delegate {
        properties.push(property("Delegate", Value::Bool(true)));
    }
    let reply = bus.call(
        MANAGER,
        MANAGER_PATH,
        MANAGER_INTERFACE,
        "StartTransientUnit",
        &[
            Value::Str(unit.to_string()),
            Value::Str("fail".to_string()),
            Value::Array("(sv)".to_string(), properties),
            Value::Array("(sa(sv))".to_string(), Vec::new()),
        ],
    )?;
    match reply.first() {
        Some(Value::Path(job)) => Ok(job.clone()),
        _ => Err(io::Error::other(format!(
            "the user's manager started no job for {unit}"
        ))),
    }
}

/// Waits for `job`, the start of `unit`, to end (its `JobRemoved`): its process is in the
/// scope's cgroup once it has ended done.
pub fn started<S: Read + Write>(bus: &mut Connection<S>, job: &str, unit: &str) -> io::Result<()> {
    let ended = bus.signal(|m| {
        m.member.as_deref() == Some("JobRemoved") && matches!(m.body.get(1), Some(Value::Path(p)) if p == job)
    })?;
    match ended.body.get(3) {
        Some(Value::Str(result)) if result == "done" => Ok(()),
        Some(Value::Str(result)) => Err(io::Error::other(format!(
            "the user's manager's start of {unit} ended {result}"
        ))),
        _ => Err(io::Error::other(format!(
            "the user's manager's start of {unit} ended saying nothing"
        ))),
    }
}

/// Makes `pid` the one process of a new scope `unit` in `slice` ([`start_scope`]), and
/// returns once the scope has started, `pid` then in its cgroup.
pub fn place<S: Read + Write>(
    bus: &mut Connection<S>,
    unit: &str,
    slice: &str,
    pid: u32,
    description: &str,
    delegate: bool,
) -> io::Result<()> {
    watch_jobs(bus)?;
    let job = start_scope(bus, unit, slice, pid, description, delegate)?;
    started(bus, &job, unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Specification's own examples ("Marshalling containers"), little-endian: an
    /// array holding the 64-bit 5 pads to 8 after its length, a variant holding it pads
    /// after its signature.
    #[test]
    fn values_are_aligned_as_the_specification_has_them() {
        let mut w = Writer::default();
        w.value(&Value::Array("u".to_string(), vec![Value::U32(5)]))
            .unwrap();
        assert_eq!(w.buf, [4, 0, 0, 0, 5, 0, 0, 0]);
        let mut w = Writer::default();
        w.value(&Value::Array("(sv)".to_string(), Vec::new())).unwrap();
        // An empty array still pads to its element's alignment.
        assert_eq!(w.buf, [0, 0, 0, 0, 0, 0, 0, 0]);
        let mut w = Writer::default();
        w.value(&Value::Variant(Box::new(Value::Str("ab".to_string()))))
            .unwrap();
        assert_eq!(w.buf, [1, b's', 0, 0, 2, 0, 0, 0, b'a', b'b', 0]);
    }

    #[test]
    fn a_message_reads_back_as_it_was_written() {
        let body = [
            Value::Str("shards-1.scope".to_string()),
            Value::Str("fail".to_string()),
            Value::Array(
                "(sv)".to_string(),
                vec![Value::Struct(vec![
                    Value::Str("PIDs".to_string()),
                    Value::Variant(Box::new(Value::Array("u".to_string(), vec![Value::U32(42)]))),
                ])],
            ),
            Value::Array("(sa(sv))".to_string(), Vec::new()),
            Value::Bool(true),
            Value::Path("/a/b".to_string()),
        ];
        let fields = vec![
            field(1, Value::Path(MANAGER_PATH.to_string())),
            field(3, Value::Str("StartTransientUnit".to_string())),
        ];
        let bytes = message(METHOD_CALL, 7, fields, &body).unwrap();
        let m = read_message(&mut bytes.as_slice()).unwrap();
        assert_eq!((m.kind, m.serial), (METHOD_CALL, 7));
        assert_eq!(m.path.as_deref(), Some(MANAGER_PATH));
        assert_eq!(m.member.as_deref(), Some("StartTransientUnit"));
        assert_eq!(m.body, body);
    }

    #[test]
    fn a_big_endian_message_is_read_too() {
        // A METHOD_RETURN, serial 2, replying to 1, of one string "x": its fields
        // REPLY_SERIAL (a variant of u, padded to 4) and SIGNATURE (a variant of g), then
        // the header padded to 8, then the body.
        let mut bytes = vec![b'B', METHOD_RETURN, 0, 1, 0, 0, 0, 6, 0, 0, 0, 2];
        let fields: &[u8] = &[5, 1, b'u', 0, 0, 0, 0, 1, 8, 1, b'g', 0, 1, b's', 0];
        bytes.extend_from_slice(&u32::try_from(fields.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(fields);
        bytes.push(0);
        bytes.extend_from_slice(&[0, 0, 0, 1, b'x', 0]);
        let m = read_message(&mut bytes.as_slice()).unwrap();
        assert_eq!(m.reply_to, Some(1));
        assert_eq!(m.body, [Value::Str("x".to_string())]);
    }

    #[test]
    fn what_a_message_cannot_be_is_refused() {
        // Cut short, of no byte order, and a signature naming more than the body holds.
        let good = message(
            SIGNAL,
            1,
            vec![field(3, Value::Str("JobRemoved".to_string()))],
            &[Value::U32(1)],
        )
        .unwrap();
        assert!(read_message(&mut good.get(..good.len() - 1).unwrap()).is_err());
        let mut bad = good.clone();
        bad[0] = b'x';
        assert!(read_message(&mut bad.as_slice()).is_err());
        let longer = message(SIGNAL, 1, vec![field(8, Value::Signature("uu".to_string()))], &[]).unwrap();
        assert!(read_message(&mut longer.as_slice()).is_err());
        // Nested past 64 containers.
        let deep = "a".repeat(70) + "u";
        assert!(type_len(deep.as_bytes(), 0).is_err());
        assert_eq!(types(b"uoss").unwrap(), [b"u".as_slice(), b"o", b"s", b"s"]);
        assert_eq!(types(b"a(sv)a{sv}").unwrap(), [b"a(sv)".as_slice(), b"a{sv}"]);
    }

    #[test]
    fn the_user_bus_is_found_where_its_address_says() {
        assert_eq!(
            unix_path("unix:path=/run/user/1000/bus"),
            Some(PathBuf::from("/run/user/1000/bus"))
        );
        assert_eq!(
            unix_path("tcp:host=x,port=1;unix:guid=1,path=/tmp/a%20b"),
            Some(PathBuf::from("/tmp/a b"))
        );
        assert_eq!(unix_path("unix:abstract=/x"), None);
        assert_eq!(unix_path("unix:path=/tmp/%zz"), None);
    }

    /// A bus of the test's own over a socket pair: it takes the client's credentials,
    /// names it, holds the match rule, starts the job and says it ended `result`, a
    /// signal of another job's first.
    fn a_bus(result: &'static str) -> (UnixStream, std::thread::JoinHandle<Vec<Message>>) {
        let (client, mut bus) = UnixStream::pair().unwrap();
        let served = std::thread::spawn(move || {
            let mut nul = [0u8; 1];
            bus.read_exact(&mut nul).unwrap();
            assert_eq!(nul, [0]);
            assert_eq!(line(&mut bus).unwrap(), format!("AUTH EXTERNAL {}", "31303030"));
            bus.write_all(b"OK 0123456789abcdef0123456789abcdef\r\n").unwrap();
            assert_eq!(line(&mut bus).unwrap(), "BEGIN");
            let mut seen = Vec::new();
            let reply = |to: &Message, body: &[Value]| {
                message(
                    METHOD_RETURN,
                    100 + to.serial,
                    vec![field(5, Value::U32(to.serial))],
                    body,
                )
                .unwrap()
            };
            let removed = |job: &str| {
                message(
                    SIGNAL,
                    90,
                    vec![
                        field(1, Value::Path(MANAGER_PATH.to_string())),
                        field(2, Value::Str(MANAGER_INTERFACE.to_string())),
                        field(3, Value::Str("JobRemoved".to_string())),
                    ],
                    &[
                        Value::U32(1),
                        Value::Path(job.to_string()),
                        Value::Str("shards-1.scope".to_string()),
                        Value::Str(result.to_string()),
                    ],
                )
                .unwrap()
            };
            for _ in 0..3 {
                let m = read_message(&mut bus).unwrap();
                match m.member.as_deref() {
                    Some("Hello") => bus
                        .write_all(&reply(&m, &[Value::Str(":1.9".to_string())]))
                        .unwrap(),
                    Some("AddMatch") => bus.write_all(&reply(&m, &[])).unwrap(),
                    Some("StartTransientUnit") => {
                        // Another job's end, then this one's, before its reply: held.
                        bus.write_all(&removed("/org/freedesktop/systemd1/job/6"))
                            .unwrap();
                        bus.write_all(&removed("/org/freedesktop/systemd1/job/7"))
                            .unwrap();
                        let job = Value::Path("/org/freedesktop/systemd1/job/7".to_string());
                        bus.write_all(&reply(&m, &[job])).unwrap();
                    }
                    _ => panic!("an unexpected call {m:?}"),
                }
                seen.push(m);
            }
            seen
        });
        (client, served)
    }

    #[test]
    fn a_process_is_placed_once_its_start_has_ended() {
        let (client, served) = a_bus("done");
        let mut bus = Connection::open(client, 1000).unwrap();
        place(&mut bus, "shards-1.scope", "agents.slice", 42, "a microVM", false).unwrap();
        let seen = served.join().unwrap();
        let start = seen.last().unwrap();
        assert_eq!(
            start.body.first(),
            Some(&Value::Str("shards-1.scope".to_string()))
        );
        let Some(Value::Array(_, properties)) = start.body.get(2) else {
            panic!("no properties: {start:?}");
        };
        let named = |name: &str| {
            properties.iter().find_map(|p| match p {
                Value::Struct(kv) if kv.first() == Some(&Value::Str(name.to_string())) => kv.get(1).cloned(),
                _ => None,
            })
        };
        assert_eq!(
            named("PIDs"),
            Some(Value::Variant(Box::new(Value::Array(
                "u".to_string(),
                vec![Value::U32(42)]
            ))))
        );
        assert_eq!(
            named("Slice"),
            Some(Value::Variant(Box::new(Value::Str("agents.slice".to_string()))))
        );
        // The other job's end is still held; this one's was taken.
        assert_eq!(bus.held.len(), 1);
    }

    #[test]
    fn a_start_that_failed_says_so() {
        let (client, served) = a_bus("failed");
        let mut bus = Connection::open(client, 1000).unwrap();
        let err = place(&mut bus, "shards-1.scope", "agents.slice", 42, "a microVM", false).unwrap_err();
        assert!(err.to_string().ends_with("ended failed"), "{err}");
        served.join().unwrap();
    }
}
