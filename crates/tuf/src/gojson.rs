//! JSON as Go's encoding/json reads it into `map[string]any` and typed fields, and as
//! securesystemslib's cjson writes it canonically (OLPC canonical JSON over Go's own
//! re-encoding): numbers kept as written, for the typed fields that take them and the
//! values passed through as `json.RawMessage`; strings decoded with invalid UTF-8 replaced
//! as Go replaces it; an object's members in order, the last of a name winning as Go's
//! map keeps it.
//!
//! Nothing here recurses. The parse, a value's clone and drop, its canonical form and that
//! form's text each keep their stack on the heap, so a document nested as deep as Go's
//! scanner allows is read and written on any thread: recursion a level at a time overflowed
//! a 2 MiB thread's stack well within Go's depth, and a stack overflow ends the process.

use std::fmt::Write as _;

/// Go's scanner refuses JSON nested deeper than this (encoding/json scanner.go,
/// `maxNestingDepth`).
const MAX_DEPTH: usize = 10_000;

/// A JSON value: a number as written, an object's members in document order.
#[derive(Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    /// The member `name`'s value: the last of that name, as Go's map keeps it.
    pub fn get(&self, name: &str) -> Option<&Value> {
        match self {
            Value::Object(m) => m.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Go's name for its kind in a type error: `json: cannot unmarshal KIND into …`.
    pub fn kind(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    /// The first number, in document order, that Go's decoder into `any` refuses: one past
    /// a float64's range, which it reads as infinite (one too small it reads as 0).
    pub fn first_overflow(&self) -> Option<&str> {
        let mut stack = vec![self];
        while let Some(v) = stack.pop() {
            match v {
                Value::Number(n) if !n.parse::<f64>().is_ok_and(f64::is_finite) => return Some(n),
                Value::Array(a) => stack.extend(a.iter().rev()),
                Value::Object(m) => stack.extend(m.iter().rev().map(|(_, v)| v)),
                _ => {}
            }
        }
        None
    }
}

impl Clone for Value {
    /// Cloned a level at a time, as it drops.
    fn clone(&self) -> Value {
        rebuild(
            self,
            |v| match v {
                Value::Bool(b) => Value::Bool(*b),
                Value::Number(n) => Value::Number(n.clone()),
                Value::String(s) => Value::String(s.clone()),
                _ => Value::Null,
            },
            Value::Array,
            Value::Object,
        )
    }
}

/// Dropped without recursion: what each container holds is moved onto a heap stack and
/// dropped a level at a time.
impl Drop for Value {
    fn drop(&mut self) {
        let mut held = Vec::new();
        take_values(self, &mut held);
        while let Some(mut v) = held.pop() {
            take_values(&mut v, &mut held);
        }
    }
}

fn take_values(v: &mut Value, into: &mut Vec<Value>) {
    match v {
        Value::Array(items) => into.append(items),
        Value::Object(members) => into.extend(members.drain(..).map(|(_, v)| v)),
        _ => {}
    }
}

/// `v` rebuilt from its leaves up without recursion: each scalar by `leaf`, each array
/// and object from what its contents became.
fn rebuild<T>(
    v: &Value,
    mut leaf: impl FnMut(&Value) -> T,
    mut array: impl FnMut(Vec<T>) -> T,
    mut object: impl FnMut(Vec<(String, T)>) -> T,
) -> T {
    /// A container being rebuilt: what is left of it, what is done, and the key of the
    /// member being rebuilt.
    enum Frame<'a, T> {
        Array(std::slice::Iter<'a, Value>, Vec<T>),
        Object(std::slice::Iter<'a, (String, Value)>, Vec<(String, T)>, String),
    }
    fn open<T>(v: &Value) -> Option<Frame<'_, T>> {
        match v {
            Value::Array(a) => Some(Frame::Array(a.iter(), Vec::with_capacity(a.len()))),
            Value::Object(m) => Some(Frame::Object(
                m.iter(),
                Vec::with_capacity(m.len()),
                String::new(),
            )),
            _ => None,
        }
    }
    let mut stack = match open(v) {
        Some(f) => vec![f],
        None => return leaf(v),
    };
    loop {
        let next = match stack.last_mut() {
            Some(Frame::Array(items, _)) => items.next(),
            Some(Frame::Object(members, _, key)) => members.next().map(|(k, v)| {
                key.clone_from(k);
                v
            }),
            // Not reached: the outermost container is returned once done.
            None => return leaf(v),
        };
        let done = match next {
            Some(child) => match open(child) {
                Some(f) => {
                    stack.push(f);
                    continue;
                }
                None => leaf(child),
            },
            None => match stack.pop() {
                Some(Frame::Array(_, items)) => array(items),
                Some(Frame::Object(_, members, _)) => object(members),
                None => return leaf(v),
            },
        };
        match stack.last_mut() {
            None => return done,
            Some(Frame::Array(_, items)) => items.push(done),
            Some(Frame::Object(_, members, key)) => members.push((std::mem::take(key), done)),
        }
    }
}

struct Parser<'a> {
    b: &'a [u8],
    at: usize,
}

/// A container being read, and what it holds so far.
enum Open {
    Array(Vec<Value>),
    /// The members so far, and the key whose value comes next.
    Object(Vec<(String, Value)>, String),
}

/// The message of Go's `SyntaxError` for running out of input.
const END: &str = "unexpected end of JSON input";

/// `SyntaxError`'s message for byte `c` where the scanner wanted something else.
fn fail(c: u8, context: &str) -> String {
    format!("invalid character {} {context}", quote_char(c))
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.at).copied()
    }

    /// The next byte within a literal, a number or an escape, which it does not consume: at
    /// the end of the input, a space, as Go's scanner feeds one to the state it is in
    /// (`scanner.eof`).
    fn inner(&self) -> u8 {
        self.peek().unwrap_or(b' ')
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn unexpected(&self, context: &str) -> String {
        match self.peek() {
            None => END.into(),
            Some(c) => fail(c, context),
        }
    }

    /// A member's key and the `:` after it.
    fn key(&mut self) -> Result<String, String> {
        if self.peek() != Some(b'"') {
            return Err(self.unexpected("looking for beginning of object key string"));
        }
        let key = self.string()?;
        self.ws();
        if self.peek() != Some(b':') {
            return Err(self.unexpected("after object key"));
        }
        self.at += 1;
        Ok(key)
    }

    fn literal(&mut self, word: &[u8], v: Value) -> Result<Value, String> {
        for (i, &want) in word.iter().enumerate() {
            let got = self.b.get(self.at + i).copied().unwrap_or(b' ');
            if got != want {
                return Err(fail(
                    got,
                    &format!(
                        "in literal {} (expecting {})",
                        String::from_utf8_lossy(word),
                        quote_char(want)
                    ),
                ));
            }
        }
        self.at += word.len();
        Ok(v)
    }

    fn digits(&mut self) -> bool {
        let s = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.at += 1;
        }
        self.at > s
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.inner() {
            b'0' => self.at += 1,
            b'1'..=b'9' => {
                self.digits();
            }
            c => return Err(fail(c, "in numeric literal")),
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            if !self.digits() {
                return Err(fail(self.inner(), "after decimal point in numeric literal"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if !self.digits() {
                return Err(fail(self.inner(), "in exponent of numeric literal"));
            }
        }
        let text = self.b.get(start..self.at).unwrap_or_default();
        Ok(Value::Number(String::from_utf8_lossy(text).into_owned()))
    }

    /// A string, decoded as encoding/json's unquote decodes it: escapes, and each byte of
    /// invalid UTF-8 replaced by U+FFFD as `utf8.DecodeRune` reads it.
    fn string(&mut self) -> Result<String, String> {
        self.at += 1;
        let mut out = String::new();
        loop {
            // A run of bytes that need no decoding but UTF-8's.
            let start = self.at;
            while self.peek().is_some_and(|c| c >= 0x20 && c != b'"' && c != b'\\') {
                self.at += 1;
            }
            let run = self.b.get(start..self.at).unwrap_or_default();
            match std::str::from_utf8(run) {
                Ok(s) => out.push_str(s),
                Err(_) => {
                    let mut rest = run;
                    while !rest.is_empty() {
                        let (r, w) = decode_rune(rest);
                        out.push(r);
                        rest = rest.get(w..).unwrap_or_default();
                    }
                }
            }
            let Some(c) = self.peek() else {
                return Err(END.into());
            };
            self.at += 1;
            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let e = self.inner();
                    self.at += 1;
                    match e {
                        b'"' | b'\\' | b'/' => out.push(char::from(e)),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let r = self.hex4()?;
                            let r = if (0xD800..0xDC00).contains(&r) {
                                // A pair, if a low surrogate's escape follows.
                                match self.low_surrogate() {
                                    Some(low) => {
                                        self.at += 6;
                                        0x10000 + ((r - 0xD800) << 10) + (low - 0xDC00)
                                    }
                                    None => 0xFFFD,
                                }
                            } else {
                                r
                            };
                            out.push(char::from_u32(r).unwrap_or('\u{FFFD}'));
                        }
                        other => return Err(fail(other, "in string escape code")),
                    }
                }
                c => return Err(fail(c, "in string literal")),
            }
        }
    }

    /// The low surrogate a `\uXXXX` escape at the cursor holds, if one does.
    fn low_surrogate(&self) -> Option<u32> {
        let e = self.b.get(self.at..self.at + 6)?;
        let (prefix, hex) = e.split_at_checked(2)?;
        if prefix != b"\\u" {
            return None;
        }
        let low = hex
            .iter()
            .try_fold(0u32, |r, &d| Some(r * 16 + char::from(d).to_digit(16)?))?;
        (0xDC00..0xE000).contains(&low).then_some(low)
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.inner();
            let d = char::from(c)
                .to_digit(16)
                .ok_or_else(|| fail(c, "in \\u hexadecimal character escape"))?;
            v = v * 16 + d;
            self.at += 1;
        }
        Ok(v)
    }
}

/// `utf8.DecodeRune`: the rune at the start of `b` and its width, an invalid or incomplete
/// sequence U+FFFD one byte wide.
fn decode_rune(b: &[u8]) -> (char, usize) {
    let width = match b.first() {
        None => return ('\u{FFFD}', 0),
        Some(&c) if c < 0x80 => return (char::from(c), 1),
        Some(0xC2..=0xDF) => 2,
        Some(0xE0..=0xEF) => 3,
        Some(0xF0..=0xF4) => 4,
        Some(_) => return ('\u{FFFD}', 1),
    };
    // Go and Rust validate alike: no overlong forms, no surrogates, nothing past U+10FFFF.
    match b
        .get(..width)
        .and_then(|s| std::str::from_utf8(s).ok())
        .and_then(|s| s.chars().next())
    {
        Some(r) => (r, width),
        None => ('\u{FFFD}', 1),
    }
}

/// How Go's scanner names a byte in its errors (`quoteChar`): the byte as the rune of that
/// number, quoted as `strconv.Quote` quotes it.
fn quote_char(c: u8) -> String {
    match c {
        b'\'' => "'\\''".into(),
        b'"' => "'\"'".into(),
        c => {
            let mut buf = [0u8; 4];
            let q = shards_dockerfile::go::quote(char::from(c).encode_utf8(&mut buf).as_bytes());
            format!("'{}'", q.get(1..q.len().saturating_sub(1)).unwrap_or_default())
        }
    }
}

/// json.Unmarshal's parse of a whole document: one value, then only white space; for
/// what is no JSON, the scanner's message for the first byte it refuses.
pub fn parse(b: &[u8]) -> Result<Value, String> {
    let mut p = Parser { b, at: 0 };
    let mut stack: Vec<Open> = Vec::new();
    let value = 'outer: loop {
        p.ws();
        let mut done = match p.peek() {
            Some(c @ (b'[' | b'{')) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(fail(c, "exceeded max depth"));
                }
                p.at += 1;
                p.ws();
                if c == b'[' {
                    if p.peek() == Some(b']') {
                        p.at += 1;
                        Value::Array(Vec::new())
                    } else {
                        stack.push(Open::Array(Vec::new()));
                        continue;
                    }
                } else if p.peek() == Some(b'}') {
                    p.at += 1;
                    Value::Object(Vec::new())
                } else {
                    let key = p.key()?;
                    stack.push(Open::Object(Vec::new(), key));
                    continue;
                }
            }
            Some(b'"') => Value::String(p.string()?),
            Some(b't') => p.literal(b"true", Value::Bool(true))?,
            Some(b'f') => p.literal(b"false", Value::Bool(false))?,
            Some(b'n') => p.literal(b"null", Value::Null)?,
            Some(b'-' | b'0'..=b'9') => p.number()?,
            _ => return Err(p.unexpected("looking for beginning of value")),
        };
        // Add the value to its container, closing what ends. A value outside any
        // container is the document's.
        loop {
            let Some(top) = stack.last_mut() else {
                break 'outer done;
            };
            match top {
                Open::Array(items) => items.push(done),
                Open::Object(members, key) => members.push((std::mem::take(key), done)),
            }
            p.ws();
            match (p.peek(), &mut *top) {
                (Some(b','), Open::Array(_)) => {
                    p.at += 1;
                    break;
                }
                (Some(b','), Open::Object(_, key)) => {
                    p.at += 1;
                    p.ws();
                    *key = p.key()?;
                    break;
                }
                (Some(b']'), Open::Array(_)) | (Some(b'}'), Open::Object(..)) => {
                    p.at += 1;
                    done = match stack.pop() {
                        Some(Open::Array(items)) => Value::Array(items),
                        Some(Open::Object(members, _)) => Value::Object(members),
                        None => return Err(END.into()),
                    };
                }
                (_, Open::Array(_)) => return Err(p.unexpected("after array element")),
                (_, Open::Object(..)) => return Err(p.unexpected("after object key:value pair")),
            }
        }
    };
    p.ws();
    if p.at != b.len() {
        return Err(p.unexpected("after top-level value"));
    }
    Ok(value)
}

/// A number as Go's `float64` holds it and json.Marshal writes it back: shortest
/// round-trip digits, in `%f` form unless the exponent is below -6 or at least 21.
pub fn go_float_text(text: &str) -> Option<String> {
    let f: f64 = text.parse().ok()?;
    if !f.is_finite() {
        return None;
    }
    let abs = f.abs();
    if abs != 0.0 && !(1e-6..1e21).contains(&abs) {
        // Rust's `{:e}` is shortest round-trip, as strconv's 'e' with -1 precision is.
        // strconv writes the exponent signed, of at least two digits; encoding/json then
        // drops a negative one's leading zero (e-07 to e-7).
        let s = format!("{f:e}");
        let (mant, exp) = s.split_once('e')?;
        let exp: i32 = exp.parse().ok()?;
        return Some(if exp < 0 {
            format!("{mant}e-{}", exp.unsigned_abs())
        } else {
            format!("{mant}e+{exp:02}")
        });
    }
    Some(format!("{f}"))
}

/// Whether `text` is an integer as json.Number's Int64 parses one.
pub fn is_int64(text: &str) -> bool {
    text.parse::<i64>().is_ok()
}

/// A canonical value: what cjson reads back from what json.Marshal writes of the typed
/// metadata, its numbers as `json.Number` holds them. An object may hold a name more than
/// once; the last is the one a Go map keeps, and the one written.
#[derive(Debug, PartialEq)]
pub enum Canon {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<Canon>),
    Object(Vec<(String, Canon)>),
}

/// Dropped without recursion, as [`Value`] is.
impl Drop for Canon {
    fn drop(&mut self) {
        let mut held = Vec::new();
        take_canons(self, &mut held);
        while let Some(mut v) = held.pop() {
            take_canons(&mut v, &mut held);
        }
    }
}

fn take_canons(v: &mut Canon, into: &mut Vec<Canon>) {
    match v {
        Canon::Array(items) => into.append(items),
        Canon::Object(members) => into.extend(members.drain(..).map(|(_, v)| v)),
        _ => {}
    }
}

impl Canon {
    /// `map[string]any` passed through Go: numbers by way of float64 (`Value` to `any`,
    /// then marshaled).
    pub fn from_any(v: &Value) -> Canon {
        rebuild(
            v,
            |v| match v {
                Value::Number(n) => Canon::Number(go_float_text(n).unwrap_or_else(|| n.clone())),
                v => Canon::scalar(v),
            },
            Canon::Array,
            Canon::Object,
        )
    }

    /// A `json.RawMessage` passed through Go: its numbers as written.
    pub fn from_raw(v: &Value) -> Canon {
        rebuild(
            v,
            |v| match v {
                Value::Number(n) => Canon::Number(n.clone()),
                v => Canon::scalar(v),
            },
            Canon::Array,
            Canon::Object,
        )
    }

    fn scalar(v: &Value) -> Canon {
        match v {
            Value::Bool(b) => Canon::Bool(*b),
            Value::String(s) => Canon::String(s.clone()),
            _ => Canon::Null,
        }
    }

    /// EncodeCanonical's text: keys in byte order, only `\` and `"` escaped; a number that
    /// is no int64 refused in cjson's words, the first it meets in that order.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), String> {
        enum Frame<'a> {
            Array(std::slice::Iter<'a, Canon>),
            Object(std::vec::IntoIter<(&'a str, &'a Canon)>),
        }
        let mut stack: Vec<Frame<'_>> = Vec::new();
        let mut next = Some(self);
        loop {
            match next.take() {
                None => {}
                Some(Canon::Null) => out.extend_from_slice(b"null"),
                Some(Canon::Bool(true)) => out.extend_from_slice(b"true"),
                Some(Canon::Bool(false)) => out.extend_from_slice(b"false"),
                Some(Canon::Number(n)) => {
                    if !is_int64(n) {
                        return Err(format!("Can't canonicalize floating point number '{n}'"));
                    }
                    out.extend_from_slice(n.as_bytes());
                }
                Some(Canon::String(s)) => string(out, s),
                Some(Canon::Array(a)) => {
                    out.push(b'[');
                    stack.push(Frame::Array(a.iter()));
                }
                Some(Canon::Object(m)) => {
                    out.push(b'{');
                    stack.push(Frame::Object(sorted_members(m).into_iter()));
                }
            }
            // The next value: the innermost container's next, or its end.
            let Some(top) = stack.last_mut() else {
                return Ok(());
            };
            let first = matches!(out.last(), Some(b'[' | b'{'));
            match top {
                Frame::Array(items) => match items.next() {
                    Some(v) => {
                        if !first {
                            out.push(b',');
                        }
                        next = Some(v);
                    }
                    None => {
                        out.push(b']');
                        stack.pop();
                    }
                },
                Frame::Object(members) => match members.next() {
                    Some((k, v)) => {
                        if !first {
                            out.push(b',');
                        }
                        string(out, k);
                        out.push(b':');
                        next = Some(v);
                    }
                    None => {
                        out.push(b'}');
                        stack.pop();
                    }
                },
            }
        }
    }
}

/// An object's members once each, in byte order of their names, the last of a name
/// winning, as a Go map holds them and cjson sorts them.
fn sorted_members(m: &[(String, Canon)]) -> Vec<(&str, &Canon)> {
    let mut sorted: Vec<(&str, &Canon)> = m.iter().map(|(k, v)| (k.as_str(), v)).collect();
    // Stable: of members of one name, the last stays last.
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut out: Vec<(&str, &Canon)> = Vec::with_capacity(sorted.len());
    for (k, v) in sorted {
        match out.last_mut() {
            Some(last) if last.0 == k => last.1 = v,
            _ => out.push((k, v)),
        }
    }
    out
}

fn string(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for c in s.bytes() {
        if c == b'\\' || c == b'"' {
            out.push(b'\\');
        }
        out.push(c);
    }
    out.push(b'"');
}

/// `%d` of an int64, for the typed fields Go marshals as numbers.
pub fn int(v: i64) -> Canon {
    let mut s = String::new();
    let _ = write!(s, "{v}");
    Canon::Number(s)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn floats_print_as_go_prints_them() {
        for (input, want) in [
            ("5", "5"),
            ("1e3", "1000"),
            ("1.5", "1.5"),
            ("1e21", "1e+21"),
            ("123456789012345678901", "123456789012345680000"),
            ("0.000001", "0.000001"),
            ("0.0000001", "1e-7"),
            ("1.5e-10", "1.5e-10"),
            ("-0", "-0"),
        ] {
            assert_eq!(go_float_text(input).unwrap(), want, "{input}");
        }
    }

    fn canonical(v: Canon) -> Result<String, String> {
        let mut out = Vec::new();
        v.encode(&mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    #[test]
    fn canonical_json_sorts_and_escapes_as_cjson() {
        // `U` stands for the escape's `\u`.
        let v = r#"{"b":1,"a":"q\"\\U00e9<","a":"x","c":[true,null]}"#.replace('U', "\\u");
        let v = parse(v.as_bytes()).unwrap();
        assert_eq!(
            canonical(Canon::from_any(&v)).unwrap(),
            r#"{"a":"x","b":1,"c":[true,null]}"#
        );
        let v = parse("{\"s\":\"q\\\"\\\\é<\\n\"}".as_bytes()).unwrap();
        let mut out = Vec::new();
        Canon::from_any(&v).encode(&mut out).unwrap();
        assert_eq!(out, b"{\"s\":\"q\\\"\\\\\xc3\xa9<\n\"}");
        assert!(canonical(Canon::from_any(&parse(b"1.5").unwrap())).is_err());
        assert!(canonical(Canon::from_raw(&parse(b"1e3").unwrap())).is_err());
        assert_eq!(
            canonical(Canon::from_any(&parse(b"1e3").unwrap())).unwrap(),
            "1000"
        );
    }

    /// Of numbers cjson cannot write, the one refused is the first in its order, keys
    /// sorted (measured: cjson.EncodeCanonical as buildx v0.37.1 vendors it).
    #[test]
    fn canonical_json_refuses_the_number_cjson_meets_first() {
        for (json, want) in [
            (r#"{"b":1.5,"a":2.5}"#, "2.5"),
            (r#"{"a":{"z":1.5},"b":2.5}"#, "1.5"),
            (r#"{"b":[1.5],"a":{"c":3.5}}"#, "3.5"),
            // A name's earlier value is not the map's, and not written.
            (r#"{"a":1.5,"a":1,"b":2.5}"#, "2.5"),
        ] {
            assert_eq!(
                canonical(Canon::from_raw(&parse(json.as_bytes()).unwrap())),
                Err(format!("Can't canonicalize floating point number '{want}'")),
                "{json}"
            );
        }
    }

    /// The scanner's words for what it refuses (measured: encoding/json, Go 1.26.1): the
    /// end of the input within a literal is a space, as Go's scanner feeds one; bytes are
    /// named as strconv.Quote names the rune of their number.
    #[test]
    fn syntax_errors_are_the_scanners() {
        for (json, want) in [
            (
                &b"tru"[..],
                "invalid character ' ' in literal true (expecting 'e')",
            ),
            (b"-", "invalid character ' ' in numeric literal"),
            (
                b"1.",
                "invalid character ' ' after decimal point in numeric literal",
            ),
            (b"1e", "invalid character ' ' in exponent of numeric literal"),
            (b"\"\\", "invalid character ' ' in string escape code"),
            (
                b"\"\\u12",
                "invalid character ' ' in \\u hexadecimal character escape",
            ),
            (b"\"ab", "unexpected end of JSON input"),
            (b"\\", "invalid character '\\\\' looking for beginning of value"),
            (b"\xe9", "invalid character 'é' looking for beginning of value"),
            (
                b"\x85",
                "invalid character '\\u0085' looking for beginning of value",
            ),
            (b"\"\n\"", "invalid character '\\n' in string literal"),
            (b"\"\t\"", "invalid character '\\t' in string literal"),
            (
                b"\x01",
                "invalid character '\\x01' looking for beginning of value",
            ),
            (b"'", "invalid character '\\'' looking for beginning of value"),
            (b"\"\"\"", "invalid character '\"' after top-level value"),
            (b"[1,]", "invalid character ']' looking for beginning of value"),
            (
                b"{\"a\":1,}",
                "invalid character '}' looking for beginning of object key string",
            ),
            (b"{\"a\" 1}", "invalid character '1' after object key"),
            (b"[1 2]", "invalid character '2' after array element"),
            (
                b"{\"a\":1 2}",
                "invalid character '2' after object key:value pair",
            ),
            (b"[", "unexpected end of JSON input"),
        ] {
            assert_eq!(
                parse(json).map(|_| ()),
                Err(want.to_string()),
                "{}",
                String::from_utf8_lossy(json)
            );
        }
    }

    /// Invalid UTF-8 in a string is replaced a byte at a time, as `utf8.DecodeRune` reads
    /// it (measured: Go 1.26.1), not by maximal subpart: a truncated four-byte sequence is
    /// three replacements.
    #[test]
    fn invalid_utf8_is_replaced_as_go_replaces_it() {
        for (json, want) in [
            (&b"\"\xf0\x9f\x98\""[..], "\u{FFFD}\u{FFFD}\u{FFFD}"),
            (b"\"\xe0\x80\"", "\u{FFFD}\u{FFFD}"),
            (b"\"\xc0\xaf\"", "\u{FFFD}\u{FFFD}"),
            (b"\"\xed\xa0\x80\"", "\u{FFFD}\u{FFFD}\u{FFFD}"),
            (b"\"a\xffb\"", "a\u{FFFD}b"),
            ("\"é😀\"".as_bytes(), "é😀"),
        ] {
            assert_eq!(
                parse(json).unwrap(),
                Value::String(want.to_string()),
                "{}",
                String::from_utf8_lossy(json)
            );
        }
        // Escapes (`U` stands for `\u`): a pair, a high surrogate with no low one after it,
        // and a lone low one.
        for (json, want) in [
            (r#""Ud83dUde00Ud83dU0041Ude00""#, "😀\u{FFFD}A\u{FFFD}"),
            (r#""Ud83dUd83dUde00""#, "\u{FFFD}😀"),
            (r#""Ud83d""#, "\u{FFFD}"),
            (r#""U00e9U0000""#, "é\u{0}"),
        ] {
            let json = json.replace('U', "\\u");
            let json = json.as_bytes();
            assert_eq!(
                parse(json).unwrap(),
                Value::String(want.to_string()),
                "{}",
                String::from_utf8_lossy(json)
            );
        }
    }
}
