//! Protocol buffers' JSON as google.golang.org/protobuf's protojson.Unmarshal reads the
//! Sigstore messages (encoding/protojson/decode.go over internal/encoding/json): a token
//! at a time against the message's schema, so a failure is the first Go meets, in its
//! words. Fields by their JSON or proto name, none twice, none unknown, one per oneof;
//! `null` leaves a field unset; bytes in base64 of either alphabet, padded or not as
//! their length says; 64-bit integers as numbers or strings; enums by name or number;
//! google.protobuf.Timestamp in RFC 3339.
//!
//! Go prints `proto:` followed by a space or a no-break space, chosen per binary to keep
//! callers from comparing messages; this prints a space (D105).

use crate::gobase64;

/// protojson's error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtoError(pub String);

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "proto: {}", self.0)
    }
}

/// The kinds of token (json.Kind), as bits.
const EOF: u32 = 1;
const NULL: u32 = 2;
const BOOL: u32 = 4;
const NUMBER: u32 = 8;
const STRING: u32 = 16;
const NAME: u32 = 32;
const OBJECT_OPEN: u32 = 64;
const OBJECT_CLOSE: u32 = 128;
const ARRAY_OPEN: u32 = 256;
const ARRAY_CLOSE: u32 = 512;
const COMMA: u32 = 1024;

#[derive(Debug, Clone, Default)]
struct Token {
    kind: u32,
    pos: usize,
    raw: Vec<u8>,
    text: String,
    boolean: bool,
}

/// json.Decoder.
struct Decoder<'a> {
    orig: &'a [u8],
    at: usize,
    last: Token,
    last_err: Option<ProtoError>,
    peeked: bool,
    stack: Vec<u32>,
}

fn err(s: impl Into<String>) -> ProtoError {
    ProtoError(s.into())
}

fn unexpected_eof() -> ProtoError {
    err("unexpected EOF")
}

fn not_delim(c: u8) -> bool {
    c == b'-' || c == b'+' || c == b'.' || c == b'_' || c.is_ascii_alphanumeric()
}

/// parseNumber: the length of a JSON number at the start of `s`.
fn number_len(input: &[u8]) -> Option<usize> {
    let mut n = 0;
    let mut s = input;
    let first = *s.first()?;
    if first == b'-' {
        s = s.get(1..)?;
        n += 1;
        s.first()?;
    }
    match *s.first()? {
        b'0' => {
            s = s.get(1..)?;
            n += 1;
        }
        b'1'..=b'9' => {
            s = s.get(1..)?;
            n += 1;
            while s.first().is_some_and(u8::is_ascii_digit) {
                s = s.get(1..)?;
                n += 1;
            }
        }
        _ => return None,
    }
    if s.len() >= 2 && s.first() == Some(&b'.') && s.get(1).is_some_and(u8::is_ascii_digit) {
        s = s.get(2..)?;
        n += 2;
        while s.first().is_some_and(u8::is_ascii_digit) {
            s = s.get(1..)?;
            n += 1;
        }
    }
    if s.len() >= 2 && matches!(s.first(), Some(b'e' | b'E')) {
        s = s.get(1..)?;
        n += 1;
        if matches!(s.first(), Some(b'+' | b'-')) {
            s = s.get(1..)?;
            n += 1;
            s.first()?;
        }
        while s.first().is_some_and(u8::is_ascii_digit) {
            s = s.get(1..)?;
            n += 1;
        }
    }
    if input.get(n).is_some_and(|c| not_delim(*c)) {
        return None;
    }
    Some(n)
}

/// A number token's text as an integer's (parseNumberParts, normalizeToIntString).
fn int_string(raw: &[u8]) -> Option<String> {
    let mut s = raw;
    let neg = s.first() == Some(&b'-');
    if neg {
        s = s.get(1..)?;
    }
    let mut intp: &[u8] = &[];
    match *s.first()? {
        b'0' => s = s.get(1..)?,
        b'1'..=b'9' => {
            let n = 1 + s.get(1..)?.iter().take_while(|c| c.is_ascii_digit()).count();
            intp = s.get(..n)?;
            s = s.get(n..)?;
        }
        _ => return None,
    }
    let mut frac: &[u8] = &[];
    if s.len() >= 2 && s.first() == Some(&b'.') && s.get(1).is_some_and(u8::is_ascii_digit) {
        let n = 1 + s.get(2..)?.iter().take_while(|c| c.is_ascii_digit()).count();
        frac = s.get(1..1 + n)?;
        s = s.get(1 + n..)?;
    }
    let mut exp: &[u8] = &[];
    if s.len() >= 2 && matches!(s.first(), Some(b'e' | b'E')) {
        s = s.get(1..)?;
        let mut n = 0;
        if matches!(s.first(), Some(b'+' | b'-')) {
            n += 1;
            s.get(1..)?.first()?;
        }
        n += s.get(n..)?.iter().take_while(|c| c.is_ascii_digit()).count();
        exp = s.get(..n)?;
    }
    while frac.last() == Some(&b'0') {
        frac = frac.get(..frac.len() - 1)?;
    }
    if intp.is_empty() && frac.is_empty() {
        return Some("0".into());
    }
    let e: i64 = if exp.is_empty() {
        0
    } else {
        let t = std::str::from_utf8(exp).ok()?;
        let v: i64 = t.parse().ok()?;
        i32::try_from(v).ok()?;
        v
    };
    let num: Vec<u8> = if e >= 0 {
        let e = usize::try_from(e).ok()?;
        if frac.len() > e || intp.len() + e > 20 {
            return None;
        }
        let mut num = intp.to_vec();
        num.extend_from_slice(frac);
        num.extend(std::iter::repeat_n(b'0', e - frac.len()));
        num
    } else {
        if !frac.is_empty() {
            return None;
        }
        let index = i64::try_from(intp.len()).ok()? + e;
        let index = usize::try_from(index).ok()?;
        if intp.get(index..)?.iter().any(|c| *c != b'0') {
            return None;
        }
        intp.get(..index)?.to_vec()
    };
    let num = String::from_utf8(num).ok()?;
    Some(if neg { format!("-{num}") } else { num })
}

/// strconv.ParseInt/ParseUint of an integer's text: digits after an optional sign
/// ("" and "-" are not numbers).
fn parse_int(s: &str, bits: u32, signed: bool) -> Option<i128> {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) || (neg && !signed) {
        return None;
    }
    let mut v: i128 = 0;
    for c in digits.bytes() {
        v = v.checked_mul(10)?.checked_add(i128::from(c - b'0'))?;
        if v > i128::from(u64::MAX) + 1 {
            return None;
        }
    }
    if neg {
        v = -v;
    }
    let (min, max) = if signed {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
    } else {
        (0, (1i128 << bits) - 1)
    };
    (min..=max).contains(&v).then_some(v)
}

impl<'a> Decoder<'a> {
    fn new(b: &'a [u8]) -> Decoder<'a> {
        Decoder {
            orig: b,
            at: 0,
            last: Token::default(),
            last_err: None,
            peeked: false,
            stack: Vec::new(),
        }
    }

    fn rest(&self) -> &'a [u8] {
        self.orig.get(self.at..).unwrap_or_default()
    }

    /// Position: the line and column (in runes) of an offset.
    fn position(&self, idx: usize) -> (usize, usize) {
        let b = self.orig.get(..idx).unwrap_or_default();
        let line = b.iter().filter(|c| **c == b'\n').count() + 1;
        let tail = match b.iter().rposition(|c| *c == b'\n') {
            Some(i) => b.get(i + 1..).unwrap_or_default(),
            None => b,
        };
        let column = String::from_utf8_lossy(tail).chars().count() + 1;
        (line, column)
    }

    fn syntax_error(&self, pos: usize, msg: &str) -> ProtoError {
        let (l, c) = self.position(pos);
        err(format!("syntax error (line {l}:{c}): {msg}"))
    }

    fn new_error(&self, pos: usize, msg: &str) -> ProtoError {
        let (l, c) = self.position(pos);
        err(format!("(line {l}:{c}): {msg}"))
    }

    fn unexpected(&self, tok: &Token) -> ProtoError {
        self.syntax_error(
            tok.pos,
            &format!("unexpected token {}", String::from_utf8_lossy(&tok.raw)),
        )
    }

    /// consume: n octets, then whitespace.
    fn consume(&mut self, n: usize) {
        self.at += n;
        while matches!(self.orig.get(self.at), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.at += 1;
        }
    }

    fn token(&mut self, kind: u32, size: usize) -> Token {
        let raw = self.rest().get(..size).unwrap_or_default().to_vec();
        let pos = self.at;
        self.consume(size);
        Token {
            kind,
            pos,
            raw,
            ..Token::default()
        }
    }

    /// parseString.
    fn string(&self) -> Result<(String, usize), ProtoError> {
        let input = self.rest();
        if input.first() != Some(&b'"') {
            return Err(unexpected_eof());
        }
        let mut out: Vec<u8> = Vec::new();
        let mut i = 1;
        loop {
            let Some(&c) = input.get(i) else {
                return Err(unexpected_eof());
            };
            if c >= 0x80 {
                let width = (2..=4)
                    .find(|w| {
                        input
                            .get(i..i + w)
                            .is_some_and(|s| std::str::from_utf8(s).is_ok())
                    })
                    .ok_or_else(|| self.syntax_error(self.at, "invalid UTF-8 in string"))?;
                out.extend_from_slice(input.get(i..i + width).unwrap_or_default());
                i += width;
                continue;
            }
            if c < b' ' {
                return Err(self.syntax_error(
                    self.at,
                    &format!("invalid character {} in string", go_quote_rune(char::from(c))),
                ));
            }
            match c {
                b'"' => {
                    let s = String::from_utf8(out)
                        .map_err(|_| self.syntax_error(self.at, "invalid UTF-8 in string"))?;
                    return Ok((s, i + 1));
                }
                b'\\' => {
                    let Some(&e) = input.get(i + 1) else {
                        return Err(unexpected_eof());
                    };
                    match e {
                        b'"' | b'\\' | b'/' => out.push(e),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let hex = input.get(i + 2..i + 6).ok_or_else(unexpected_eof)?;
                            // Go reports every failure in a string at the string's start.
                            let bad = |_: usize, s: &[u8]| {
                                self.syntax_error(
                                    self.at,
                                    &format!(
                                        "invalid escape code {} in string",
                                        shards_dockerfile::go::quote(s)
                                    ),
                                )
                            };
                            let v = std::str::from_utf8(hex)
                                .ok()
                                .and_then(|h| u16::from_str_radix(h, 16).ok().filter(|_| !h.starts_with('+')))
                                .ok_or_else(|| bad(i, input.get(i..i + 6).unwrap_or_default()))?;
                            i += 6;
                            let r = if (0xd800..0xe000).contains(&v) {
                                let next = input.get(i..i + 6).ok_or_else(unexpected_eof)?;
                                let low = std::str::from_utf8(next.get(2..6).unwrap_or_default())
                                    .ok()
                                    .and_then(|h| {
                                        u16::from_str_radix(h, 16).ok().filter(|_| !h.starts_with('+'))
                                    });
                                let pair = match low {
                                    Some(lo)
                                        if (0xd800..0xdc00).contains(&v)
                                            && (0xdc00..0xe000).contains(&lo) =>
                                    {
                                        char::from_u32(
                                            0x10000
                                                + ((u32::from(v) - 0xd800) << 10)
                                                + (u32::from(lo) - 0xdc00),
                                        )
                                    }
                                    _ => None,
                                };
                                match (next.first(), next.get(1), pair) {
                                    (Some(b'\\'), Some(b'u'), Some(ch)) => {
                                        i += 6;
                                        ch
                                    }
                                    _ => return Err(bad(i, next)),
                                }
                            } else {
                                char::from_u32(u32::from(v)).unwrap_or('\u{fffd}')
                            };
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(r.encode_utf8(&mut buf).as_bytes());
                            continue;
                        }
                        _ => {
                            return Err(self.syntax_error(
                                self.at,
                                &format!(
                                    "invalid escape code {} in string",
                                    shards_dockerfile::go::quote(input.get(i..i + 2).unwrap_or_default())
                                ),
                            ));
                        }
                    }
                    i += 2;
                }
                _ => {
                    out.push(c);
                    i += 1;
                }
            }
        }
    }

    /// parseNext.
    fn next(&mut self) -> Result<Token, ProtoError> {
        self.consume(0);
        let input = self.rest();
        let Some(&c) = input.first() else {
            return Ok(self.token(EOF, 0));
        };
        let with_delim = |lit: &[u8]| -> Option<usize> {
            if !input.starts_with(lit) || input.get(lit.len()).is_some_and(|c| not_delim(*c)) {
                return None;
            }
            Some(lit.len())
        };
        match c {
            b'n' => {
                if let Some(n) = with_delim(b"null") {
                    return Ok(self.token(NULL, n));
                }
            }
            b't' => {
                if let Some(n) = with_delim(b"true") {
                    let mut t = self.token(BOOL, n);
                    t.boolean = true;
                    return Ok(t);
                }
            }
            b'f' => {
                if let Some(n) = with_delim(b"false") {
                    return Ok(self.token(BOOL, n));
                }
            }
            b'-' | b'0'..=b'9' => {
                if let Some(n) = number_len(input) {
                    return Ok(self.token(NUMBER, n));
                }
            }
            b'"' => {
                let (s, n) = self.string()?;
                let mut t = self.token(STRING, n);
                t.text = s;
                return Ok(t);
            }
            b'{' => return Ok(self.token(OBJECT_OPEN, 1)),
            b'}' => return Ok(self.token(OBJECT_CLOSE, 1)),
            b'[' => return Ok(self.token(ARRAY_OPEN, 1)),
            b']' => return Ok(self.token(ARRAY_CLOSE, 1)),
            b',' => return Ok(self.token(COMMA, 1)),
            _ => {}
        }
        // errRegexp: up to 32 of [-+._a-zA-Z0-9], or one character.
        let n = input.iter().take(32).take_while(|c| not_delim(**c)).count();
        let shown = if n > 0 {
            String::from_utf8_lossy(input.get(..n).unwrap_or_default()).into_owned()
        } else {
            match std::str::from_utf8(input.get(..input.len().min(4)).unwrap_or_default()) {
                Ok(s) => s.chars().next().map(String::from).unwrap_or_default(),
                Err(e) => {
                    let valid = input.get(..e.valid_up_to()).unwrap_or_default();
                    match std::str::from_utf8(valid).ok().and_then(|s| s.chars().next()) {
                        Some(ch) => ch.to_string(),
                        None => String::from_utf8_lossy(input.get(..1).unwrap_or_default()).into_owned(),
                    }
                }
            }
        };
        Err(self.syntax_error(self.at, &format!("invalid value {shown}")))
    }

    fn value_next(&self) -> bool {
        match self.stack.last() {
            None => self.last.kind == 0,
            Some(&OBJECT_OPEN) => self.last.kind & NAME != 0,
            Some(_) => self.last.kind & (ARRAY_OPEN | COMMA) != 0,
        }
    }

    /// Read.
    fn read(&mut self) -> Result<Token, ProtoError> {
        if self.peeked {
            self.peeked = false;
            return match self.last_err.take() {
                Some(e) => {
                    self.last_err = Some(e.clone());
                    Err(e)
                }
                None => Ok(self.last.clone()),
            };
        }
        self.read_fresh()
    }

    fn read_fresh(&mut self) -> Result<Token, ProtoError> {
        let mut tok = self.next()?;
        match tok.kind {
            EOF => {
                if !self.stack.is_empty() {
                    return Err(unexpected_eof());
                }
            }
            NULL | BOOL | NUMBER => {
                if !self.value_next() {
                    return Err(self.unexpected(&tok));
                }
            }
            STRING => {
                if !self.value_next() {
                    if self.last.kind & (OBJECT_OPEN | COMMA) == 0 {
                        return Err(self.unexpected(&tok));
                    }
                    let Some(&c) = self.rest().first() else {
                        return Err(unexpected_eof());
                    };
                    if c != b':' {
                        return Err(self.syntax_error(
                            self.at,
                            &format!(
                                "unexpected character {}, missing \":\" after field name",
                                char::from(c)
                            ),
                        ));
                    }
                    tok.kind = NAME;
                    self.consume(1);
                }
            }
            OBJECT_OPEN | ARRAY_OPEN => {
                if !self.value_next() {
                    return Err(self.unexpected(&tok));
                }
                self.stack.push(tok.kind);
            }
            OBJECT_CLOSE => {
                if self.stack.last() != Some(&OBJECT_OPEN) || self.last.kind & (NAME | COMMA) != 0 {
                    return Err(self.unexpected(&tok));
                }
                self.stack.pop();
            }
            ARRAY_CLOSE => {
                if self.stack.last() != Some(&ARRAY_OPEN) || self.last.kind == COMMA {
                    return Err(self.unexpected(&tok));
                }
                self.stack.pop();
            }
            _ => {
                if self.stack.is_empty()
                    || self.last.kind & (NULL | BOOL | NUMBER | STRING | OBJECT_CLOSE | ARRAY_CLOSE) == 0
                {
                    return Err(self.unexpected(&tok));
                }
            }
        }
        self.last = tok.clone();
        if tok.kind == COMMA {
            return self.read_fresh();
        }
        Ok(tok)
    }

    /// Peek.
    fn peek(&mut self) -> Result<Token, ProtoError> {
        if !self.peeked {
            match self.read_fresh() {
                Ok(t) => {
                    self.last = t;
                    self.last_err = None;
                }
                Err(e) => self.last_err = Some(e),
            }
            self.peeked = true;
        }
        match &self.last_err {
            Some(e) => Err(e.clone()),
            None => Ok(self.last.clone()),
        }
    }
}

/// %q of a rune, as fmt quotes it.
fn go_quote_rune(c: char) -> String {
    match c {
        '\x07' => "'\\a'".into(),
        '\x08' => "'\\b'".into(),
        '\x0c' => "'\\f'".into(),
        '\n' => "'\\n'".into(),
        '\r' => "'\\r'".into(),
        '\t' => "'\\t'".into(),
        '\x0b' => "'\\v'".into(),
        c if (c as u32) < 0x20 => format!("'\\x{:02x}'", c as u32),
        c => format!("'{c}'"),
    }
}

/// A field's kind.
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Bool,
    Int32,
    Int64,
    Uint64,
    Str,
    Bytes,
    Enum(&'static [(&'static str, i32)]),
    Message(&'static Schema),
    Timestamp,
    /// map<string, string>.
    StringMap,
    /// google.protobuf.Struct, read for its validity.
    Struct,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Bool => "bool",
            Kind::Int32 => "int32",
            Kind::Int64 => "int64",
            Kind::Uint64 => "uint64",
            Kind::Str => "string",
            Kind::Bytes => "bytes",
            Kind::Enum(_) => "enum",
            Kind::Message(_) | Kind::Timestamp | Kind::StringMap | Kind::Struct => "message",
        }
    }
}

/// A field: its JSON and proto names, number, kind, whether repeated, its oneof (by
/// index into the message's), and whether it keeps presence (optional).
#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub json: &'static str,
    pub name: &'static str,
    pub number: u32,
    pub kind: Kind,
    pub repeated: bool,
    pub oneof: Option<usize>,
    pub nullable: bool,
}

/// A message's schema: its full name, fields and oneofs' full names.
#[derive(Debug)]
pub struct Schema {
    pub name: &'static str,
    pub fields: &'static [Field],
    pub oneofs: &'static [&'static str],
}

/// A shorthand for a field of the schemas below.
pub const fn field(json: &'static str, name: &'static str, number: u32, kind: Kind) -> Field {
    Field {
        json,
        name,
        number,
        kind,
        repeated: false,
        oneof: None,
        nullable: false,
    }
}

impl Field {
    pub const fn repeated(mut self) -> Field {
        self.repeated = true;
        self
    }

    pub const fn oneof(mut self, i: usize) -> Field {
        self.oneof = Some(i);
        self
    }

    /// proto3 `optional`: presence kept (its synthetic oneof is its own).
    pub const fn optional(mut self) -> Field {
        self.nullable = true;
        self
    }
}

/// A decoded value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Val {
    Bool(bool),
    Int(i64),
    Uint(u64),
    Str(String),
    Bytes(Vec<u8>),
    Enum(i32),
    Msg(Msg),
    List(Vec<Val>),
    Time { secs: i64, nanos: u32 },
    Map(Vec<(String, String)>),
}

/// A decoded message: its fields set, by number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Msg {
    pub fields: Vec<(u32, Val)>,
}

impl Msg {
    pub fn get(&self, number: u32) -> Option<&Val> {
        self.fields.iter().find(|(n, _)| *n == number).map(|(_, v)| v)
    }

    pub fn msg(&self, number: u32) -> Option<&Msg> {
        match self.get(number) {
            Some(Val::Msg(m)) => Some(m),
            _ => None,
        }
    }

    pub fn string(&self, number: u32) -> String {
        match self.get(number) {
            Some(Val::Str(s)) => s.clone(),
            _ => String::new(),
        }
    }

    /// A bytes field as protobuf-go holds it: None where unset (an empty implicit one
    /// is unset).
    pub fn bytes(&self, number: u32) -> Option<Vec<u8>> {
        match self.get(number) {
            Some(Val::Bytes(b)) => Some(b.clone()),
            _ => None,
        }
    }

    pub fn int(&self, number: u32) -> i64 {
        match self.get(number) {
            Some(Val::Int(i)) => *i,
            _ => 0,
        }
    }

    pub fn enumeration(&self, number: u32) -> i32 {
        match self.get(number) {
            Some(Val::Enum(i)) => *i,
            _ => 0,
        }
    }

    pub fn list(&self, number: u32) -> &[Val] {
        match self.get(number) {
            Some(Val::List(l)) => l,
            _ => &[],
        }
    }

    pub fn time(&self, number: u32) -> Option<(i64, u32)> {
        match self.get(number) {
            Some(Val::Time { secs, nanos }) => Some((*secs, *nanos)),
            _ => None,
        }
    }
}

/// protojson.Unmarshal of `b` as a `schema` message.
pub fn unmarshal(b: &[u8], schema: &'static Schema) -> Result<Msg, ProtoError> {
    let mut d = Decoder::new(b);
    let m = message(&mut d, schema, 1)?;
    let tok = d.read()?;
    if tok.kind != EOF {
        return Err(d.unexpected(&tok));
    }
    Ok(m)
}

const RECURSION_LIMIT: usize = 10_000;

fn message(d: &mut Decoder<'_>, schema: &'static Schema, depth: usize) -> Result<Msg, ProtoError> {
    if depth > RECURSION_LIMIT {
        return Err(err("exceeded max recursion depth"));
    }
    let tok = d.read()?;
    if tok.kind != OBJECT_OPEN {
        return Err(d.unexpected(&tok));
    }
    let mut out = Msg::default();
    let mut seen: Vec<u32> = Vec::new();
    let mut seen_oneofs: Vec<usize> = Vec::new();
    loop {
        let tok = d.read()?;
        match tok.kind {
            OBJECT_CLOSE => return Ok(out),
            NAME => {}
            _ => return Err(d.unexpected(&tok)),
        }
        let name = tok.text.clone();
        let raw = String::from_utf8_lossy(&tok.raw).into_owned();
        let fd = schema
            .fields
            .iter()
            .find(|f| f.json == name)
            .or_else(|| schema.fields.iter().find(|f| f.name == name));
        let Some(fd) = fd else {
            return Err(d.new_error(tok.pos, &format!("unknown field {raw}")));
        };
        if seen.contains(&fd.number) {
            return Err(d.new_error(tok.pos, &format!("duplicate field {raw}")));
        }
        seen.push(fd.number);
        if d.peek().map(|t| t.kind) == Ok(NULL) {
            d.read()?;
            continue;
        }
        if fd.repeated {
            let v = list(d, fd, depth)?;
            out.fields.push((fd.number, Val::List(v)));
            continue;
        }
        if let Kind::StringMap = fd.kind {
            let v = string_map(d)?;
            out.fields.push((fd.number, Val::Map(v)));
            continue;
        }
        if let Some(i) = fd.oneof {
            if seen_oneofs.contains(&i) {
                return Err(d.new_error(
                    tok.pos,
                    &format!(
                        "error parsing {raw}, oneof {} is already set",
                        schema.oneofs.get(i).copied().unwrap_or_default()
                    ),
                ));
            }
            seen_oneofs.push(i);
        }
        if let Some(v) = singular(d, fd, depth)? {
            out.fields.push((fd.number, v));
        }
    }
}

fn singular(d: &mut Decoder<'_>, fd: &Field, depth: usize) -> Result<Option<Val>, ProtoError> {
    match fd.kind {
        Kind::Message(s) => Ok(Some(Val::Msg(message(d, s, depth + 1)?))),
        Kind::Timestamp => timestamp(d).map(Some),
        Kind::Struct => {
            structure(d, depth + 1)?;
            Ok(Some(Val::Msg(Msg::default())))
        }
        _ => scalar(d, fd),
    }
}

fn list(d: &mut Decoder<'_>, fd: &Field, depth: usize) -> Result<Vec<Val>, ProtoError> {
    let tok = d.read()?;
    if tok.kind != ARRAY_OPEN {
        return Err(d.unexpected(&tok));
    }
    let mut out = Vec::new();
    loop {
        let tok = d.peek()?;
        if tok.kind == ARRAY_CLOSE {
            d.read()?;
            return Ok(out);
        }
        match fd.kind {
            Kind::Message(s) => out.push(Val::Msg(message(d, s, depth + 1)?)),
            Kind::Timestamp => out.push(timestamp(d)?),
            _ => {
                if let Some(v) = scalar(d, fd)? {
                    out.push(v);
                }
            }
        }
    }
}

/// A number given as a number or a string of one (unmarshalInt's).
fn number_text(tok: &Token) -> Option<String> {
    match tok.kind {
        NUMBER => int_string(&tok.raw),
        STRING => {
            let s = tok.text.as_str();
            if s.trim() != s || s.trim_matches(|c: char| c.is_whitespace()) != s {
                return None;
            }
            let mut inner = Decoder::new(s.as_bytes());
            let t = inner.read().ok()?;
            let next = inner.read().ok()?;
            if next.kind != EOF || t.kind != NUMBER {
                return None;
            }
            int_string(&t.raw)
        }
        _ => None,
    }
}

fn scalar(d: &mut Decoder<'_>, fd: &Field) -> Result<Option<Val>, ProtoError> {
    let tok = d.read()?;
    let v = match fd.kind {
        Kind::Bool => (tok.kind == BOOL).then_some(Val::Bool(tok.boolean)),
        Kind::Int32 => number_text(&tok)
            .and_then(|s| parse_int(&s, 32, true))
            .map(|v| Val::Int(i64::try_from(v).unwrap_or(0))),
        Kind::Int64 => number_text(&tok)
            .and_then(|s| parse_int(&s, 64, true))
            .map(|v| Val::Int(i64::try_from(v).unwrap_or(0))),
        Kind::Uint64 => number_text(&tok)
            .and_then(|s| parse_int(&s, 64, false))
            .map(|v| Val::Uint(u64::try_from(v).unwrap_or(0))),
        Kind::Str => (tok.kind == STRING).then(|| Val::Str(tok.text.clone())),
        Kind::Bytes => {
            if tok.kind == STRING {
                let s = tok.text.as_bytes();
                let url = s.iter().any(|c| *c == b'-' || *c == b'_');
                let padded = s.len().is_multiple_of(4);
                gobase64::decode(s, url, padded).ok().map(Val::Bytes)
            } else {
                None
            }
        }
        Kind::Enum(names) => match tok.kind {
            STRING => names
                .iter()
                .find(|(n, _)| *n == tok.text)
                .map(|(_, v)| Val::Enum(*v)),
            NUMBER => int_string(&tok.raw)
                .and_then(|s| parse_int(&s, 32, true))
                .map(|v| Val::Enum(i32::try_from(v).unwrap_or(0))),
            _ => None,
        },
        Kind::Message(_) | Kind::Timestamp | Kind::StringMap | Kind::Struct => None,
    };
    let Some(v) = v else {
        return Err(d.new_error(
            tok.pos,
            &format!(
                "invalid value for {} field {}: {}",
                fd.kind.name(),
                fd.json,
                String::from_utf8_lossy(&tok.raw)
            ),
        ));
    };
    // protobuf-go keeps an empty implicit bytes field unset.
    if let Val::Bytes(b) = &v
        && b.is_empty()
        && !fd.nullable
        && !fd.repeated
    {
        return Ok(None);
    }
    Ok(Some(v))
}

/// unmarshalMap of a map<string, string>.
fn string_map(d: &mut Decoder<'_>) -> Result<Vec<(String, String)>, ProtoError> {
    const VALUE: Field = field("value", "value", 2, Kind::Str);
    let tok = d.read()?;
    if tok.kind != OBJECT_OPEN {
        return Err(d.unexpected(&tok));
    }
    let mut out: Vec<(String, String)> = Vec::new();
    loop {
        let tok = d.read()?;
        match tok.kind {
            OBJECT_CLOSE => return Ok(out),
            NAME => {}
            _ => return Err(d.unexpected(&tok)),
        }
        if out.iter().any(|(k, _)| *k == tok.text) {
            return Err(d.new_error(
                tok.pos,
                &format!("duplicate map key {}", String::from_utf8_lossy(&tok.raw)),
            ));
        }
        if let Some(Val::Str(v)) = scalar(d, &VALUE)? {
            out.push((tok.text.clone(), v));
        }
    }
}

/// unmarshalStruct: an object of google.protobuf.Value members.
fn structure(d: &mut Decoder<'_>, depth: usize) -> Result<(), ProtoError> {
    if depth > RECURSION_LIMIT {
        return Err(err("exceeded max recursion depth"));
    }
    let tok = d.read()?;
    if tok.kind != OBJECT_OPEN {
        return Err(d.unexpected(&tok));
    }
    let mut keys: Vec<String> = Vec::new();
    loop {
        let tok = d.read()?;
        match tok.kind {
            OBJECT_CLOSE => return Ok(()),
            NAME => {}
            _ => return Err(d.unexpected(&tok)),
        }
        if keys.contains(&tok.text) {
            return Err(d.new_error(
                tok.pos,
                &format!("duplicate map key {}", String::from_utf8_lossy(&tok.raw)),
            ));
        }
        keys.push(tok.text.clone());
        known_value(d, depth + 1)?;
    }
}

/// unmarshalKnownValue.
fn known_value(d: &mut Decoder<'_>, depth: usize) -> Result<(), ProtoError> {
    if depth > RECURSION_LIMIT {
        return Err(err("exceeded max recursion depth"));
    }
    let tok = d.peek()?;
    match tok.kind {
        NULL | BOOL | STRING => {
            d.read()?;
            Ok(())
        }
        NUMBER => {
            let tok = d.read()?;
            let finite = std::str::from_utf8(&tok.raw)
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .is_some_and(f64::is_finite);
            if !finite {
                return Err(d.new_error(
                    tok.pos,
                    &format!(
                        "invalid google.protobuf.Value: {}",
                        String::from_utf8_lossy(&tok.raw)
                    ),
                ));
            }
            Ok(())
        }
        OBJECT_OPEN => structure(d, depth + 1),
        ARRAY_OPEN => {
            d.read()?;
            loop {
                let tok = d.peek()?;
                if tok.kind == ARRAY_CLOSE {
                    d.read()?;
                    return Ok(());
                }
                known_value(d, depth + 1)?;
            }
        }
        _ => Err(d.new_error(
            tok.pos,
            &format!(
                "invalid google.protobuf.Value: {}",
                String::from_utf8_lossy(&tok.raw)
            ),
        )),
    }
}

/// unmarshalTimestamp.
fn timestamp(d: &mut Decoder<'_>) -> Result<Val, ProtoError> {
    let tok = d.read()?;
    if tok.kind != STRING {
        return Err(d.unexpected(&tok));
    }
    let raw = String::from_utf8_lossy(&tok.raw).into_owned();
    let invalid = || format!("invalid google.protobuf.Timestamp value {raw}");
    let t = shards_dockerfile::go::parse_rfc3339(tok.text.as_bytes())
        .map_err(|_| d.new_error(tok.pos, &invalid()))?;
    let (secs, nanos) = t.unix();
    if !(-62_135_596_800..=253_402_300_799).contains(&secs) {
        return Err(d.new_error(
            tok.pos,
            &format!("google.protobuf.Timestamp value out of range: {raw}"),
        ));
    }
    let s = tok.text.as_bytes();
    let i = s.iter().rposition(|c| *c == b'.');
    let j = s.iter().rposition(|c| matches!(c, b'Z' | b'-' | b'+'));
    if let (Some(i), Some(j)) = (i, j)
        && j >= i
        && j - i > ".999999999".len()
    {
        return Err(d.new_error(tok.pos, &invalid()));
    }
    Ok(Val::Time { secs, nanos })
}

#[cfg(test)]
mod tests {
    use super::*;

    static INNER: Schema = Schema {
        name: "t.Inner",
        fields: &[field("value", "value", 1, Kind::Int64)],
        oneofs: &[],
    };
    static OUTER: Schema = Schema {
        name: "t.Outer",
        fields: &[
            field("mediaType", "media_type", 1, Kind::Str),
            field("data", "data", 2, Kind::Bytes),
            field("a", "a", 3, Kind::Message(&INNER)).oneof(0),
            field("b", "b", 4, Kind::Message(&INNER)).oneof(0),
            field("items", "items", 5, Kind::Message(&INNER)).repeated(),
        ],
        oneofs: &["t.Outer.content"],
    };

    #[test]
    fn messages_decode_and_fail_as_protojson_does() {
        let m = unmarshal(
            br#"{"media_type":"x","data":"aGk","a":{"value":"12"},"items":[{"value":1e1}]}"#,
            &OUTER,
        )
        .unwrap();
        assert_eq!(m.string(1), "x");
        assert_eq!(m.bytes(2), Some(b"hi".to_vec()));
        assert_eq!(m.msg(3).unwrap().int(1), 12);
        let e = |s: &str| unmarshal(s.as_bytes(), &OUTER).unwrap_err().to_string();
        assert_eq!(
            e(r#"{"a":{},"b":{}}"#),
            "proto: (line 1:9): error parsing \"b\", oneof t.Outer.content is already set"
        );
        assert_eq!(e(r#"{"x":1}"#), "proto: (line 1:2): unknown field \"x\"");
        assert_eq!(
            e(r#"{"data":"aGk","mediaType":"","data":"b"}"#),
            "proto: (line 1:30): duplicate field \"data\""
        );
        assert_eq!(
            e(r#"{"a":{"value":1.5}}"#),
            "proto: (line 1:15): invalid value for int64 field value: 1.5"
        );
        assert_eq!(
            unmarshal(br#"{"data":"aGk="}"#, &OUTER).unwrap().bytes(2),
            Some(b"hi".to_vec())
        );
        assert_eq!(
            e("{\"mediaType\":\"\u{1}\"}"),
            "proto: syntax error (line 1:14): invalid character '\\x01' in string"
        );
        assert_eq!(e(r#"{} x"#), "proto: syntax error (line 1:4): invalid value x");
        assert_eq!(e(r#"{"a":{}"#), "proto: unexpected EOF");
        assert!(unmarshal(br#"{"data":""}"#, &OUTER).unwrap().bytes(2).is_none());
    }
}
