//! Rego's values as OPA keeps them (ast/term.go): numbers keep their text, objects and
//! sets are ordered by OPA's `Compare` (ast/compare.go) and iterated in that order, as
//! OPA iterates them (`sortedKeys`). JSON is read as OPA reads input (numbers as their
//! text, a repeated key's last value kept) and written as Go's encoding/json writes it.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use num_bigint::BigUint;

use crate::number::{self, Float};

/// A number, as its text (OPA's `ast.Number`).
#[derive(Debug, Clone)]
pub struct Number(pub Rc<str>);

impl Number {
    pub fn from_i64(i: i64) -> Number {
        Number(i.to_string().into())
    }

    pub fn text(&self) -> &str {
        &self.0
    }

    /// `json.Number(s).Int64()`: strconv.ParseInt(s, 10, 64).
    pub fn as_i64(&self) -> Option<i64> {
        go_parse_int(&self.0)
    }

    /// `builtins.NumberToFloat`.
    pub fn to_float(&self) -> Result<Float, number::Error> {
        Float::parse(&self.0)
    }

    /// `builtins.FloatToNumber`.
    pub fn from_float(f: &Float) -> Number {
        Number(f.text(if f.is_int() { b'f' } else { b'g' }).into())
    }
}

/// strconv.ParseInt(s, 10, 64): an optional sign, then decimal digits.
fn go_parse_int(s: &str) -> Option<i64> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<i64>().ok()
}

/// `json.Number(s).Float64()`, for the decimal forms Rust and Go both read.
fn go_parse_float(s: &str) -> Option<f64> {
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    let ok = !body.is_empty()
        && body
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'));
    if !ok {
        return None;
    }
    s.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// OPA's `NumberCompare`, its quirks kept: a number with a `.` loses every trailing `.`
/// and `0` (Go's `strings.TrimRight(s, ".0")`), so `10.0` compares equal to `1` there.
pub fn number_compare(x: &str, y: &str) -> Ordering {
    let trim = |s: &str| -> (String, bool) {
        if s.contains('.') {
            let t = s.trim_end_matches(['.', '0']);
            if t != s {
                return (t.to_string(), t.contains('.'));
            }
        }
        (s.to_string(), false)
    };
    let (xs, x_is_f) = trim(x);
    let (ys, y_is_f) = trim(y);
    if xs == ys {
        return Ordering::Equal;
    }
    if let (Some(a), Some(b)) = (go_parse_int(x), go_parse_int(y)) {
        return a.cmp(&b);
    }
    if x_is_f
        && y_is_f
        && let (Some(a), Some(b)) = (go_parse_float(x), go_parse_float(y))
        && a == b
    {
        return Ordering::Equal;
    }
    match (exact(x), exact(y)) {
        (Some(a), Some(b)) => compare_exact(&a, &b),
        // What OPA panics on ("illegal value") never reaches here: OPA's numbers come
        // from its lexer, JSON and to_number. Order them by text, to stay total.
        _ => x.cmp(y),
    }
}

/// A number's exact value: sign, mantissa, and powers of two and five.
fn exact(s: &str) -> Option<(bool, BigUint, i64, i64)> {
    number::exact_parts(s).ok()
}

fn compare_exact(a: &(bool, BigUint, i64, i64), b: &(bool, BigUint, i64, i64)) -> Ordering {
    let zero = |x: &(bool, BigUint, i64, i64)| x.1.bits() == 0;
    let sign = |x: &(bool, BigUint, i64, i64)| -> i32 {
        if zero(x) {
            0
        } else if x.0 {
            -1
        } else {
            1
        }
    };
    let (sa, sb) = (sign(a), sign(b));
    if sa != sb || sa == 0 {
        return sa.cmp(&sb);
    }
    // Same sign: scale both to a common denominator 2^min2 × 5^min5.
    let min2 = a.2.min(b.2);
    let min5 = a.3.min(b.3);
    let scale = |x: &(bool, BigUint, i64, i64)| -> BigUint {
        let five = BigUint::from(5u32).pow(u32::try_from(x.3 - min5).unwrap_or(u32::MAX));
        (&x.1 * five) << u64::try_from(x.2 - min2).unwrap_or(0)
    };
    let mag = scale(a).cmp(&scale(b));
    if sa < 0 { mag.reverse() } else { mag }
}

impl PartialEq for Number {
    fn eq(&self, other: &Number) -> bool {
        number_compare(&self.0, &other.0) == Ordering::Equal
    }
}
impl Eq for Number {}
impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Number) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Number {
    fn cmp(&self, other: &Number) -> Ordering {
        number_compare(&self.0, &other.0)
    }
}

/// A ground value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Value {
    Null,
    Bool(bool),
    Number(Number),
    String(Rc<str>),
    Array(Rc<Vec<Value>>),
    Object(Rc<BTreeMap<Value, Value>>),
    Set(Rc<BTreeSet<Value>>),
}

impl Value {
    pub fn string(s: impl Into<Rc<str>>) -> Value {
        Value::String(s.into())
    }

    pub fn int(i: i64) -> Value {
        Value::Number(Number::from_i64(i))
    }

    pub fn array(v: Vec<Value>) -> Value {
        Value::Array(Rc::new(v))
    }

    pub fn object(m: BTreeMap<Value, Value>) -> Value {
        Value::Object(Rc::new(m))
    }

    pub fn set(s: BTreeSet<Value>) -> Value {
        Value::Set(Rc::new(s))
    }

    pub fn empty_object() -> Value {
        Value::object(BTreeMap::new())
    }

    /// OPA's type names (`type_name`, and error texts).
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
            Value::Set(_) => "set",
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// The value at `key` of an object, an array's index, or a set's member.
    pub fn get(&self, key: &Value) -> Option<&Value> {
        match self {
            Value::Object(m) => m.get(key),
            Value::Array(a) => match key {
                Value::Number(n) => n
                    .as_i64()
                    .and_then(|i| usize::try_from(i).ok())
                    .and_then(|i| a.get(i)),
                _ => None,
            },
            Value::Set(s) => s.get(key),
            _ => None,
        }
    }
}

/// An error reading JSON, in encoding/json's words where they matter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError(pub String);

/// Reads JSON as OPA reads input (`util.UnmarshalJSON`, numbers kept as text).
pub fn from_json(text: &str) -> Result<Value, JsonError> {
    let mut p = JsonParser {
        b: text.as_bytes(),
        at: 0,
    };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.at != p.b.len() {
        return Err(JsonError("invalid character after top-level value".into()));
    }
    Ok(v)
}

struct JsonParser<'a> {
    b: &'a [u8],
    at: usize,
}

/// Go's encoding/json nests at most 10000 levels.
const MAX_DEPTH: usize = 10000;

impl JsonParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.at).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, lit: &[u8]) -> bool {
        if self.b.get(self.at..self.at + lit.len()) == Some(lit) {
            self.at += lit.len();
            return true;
        }
        false
    }

    fn err<T>(&self, what: &str) -> Result<T, JsonError> {
        Err(JsonError(what.to_string()))
    }

    fn value(&mut self, depth: usize) -> Result<Value, JsonError> {
        if depth > MAX_DEPTH {
            return self.err("exceeded max depth");
        }
        match self.peek() {
            Some(b'n') if self.eat(b"null") => Ok(Value::Null),
            Some(b't') if self.eat(b"true") => Ok(Value::Bool(true)),
            Some(b'f') if self.eat(b"false") => Ok(Value::Bool(false)),
            Some(b'"') => Ok(Value::String(self.string()?.into())),
            Some(b'[') => {
                self.at += 1;
                let mut out = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.at += 1;
                    return Ok(Value::array(out));
                }
                loop {
                    self.ws();
                    out.push(self.value(depth + 1)?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            return Ok(Value::array(out));
                        }
                        _ => return self.err("invalid character in array"),
                    }
                }
            }
            Some(b'{') => {
                self.at += 1;
                let mut out = BTreeMap::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.at += 1;
                    return Ok(Value::object(out));
                }
                loop {
                    self.ws();
                    if self.peek() != Some(b'"') {
                        return self.err("invalid character looking for beginning of object key string");
                    }
                    let k = self.string()?;
                    self.ws();
                    if self.peek() != Some(b':') {
                        return self.err("invalid character after object key");
                    }
                    self.at += 1;
                    self.ws();
                    let v = self.value(depth + 1)?;
                    out.insert(Value::String(k.into()), v);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {
                            self.at += 1;
                            return Ok(Value::object(out));
                        }
                        _ => return self.err("invalid character after object key:value pair"),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => self.err("invalid character looking for beginning of value"),
        }
    }

    fn number(&mut self) -> Result<Value, JsonError> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek() {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.at += 1;
                }
            }
            _ => return self.err("invalid character in numeric literal"),
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return self.err("invalid character after decimal point in numeric literal");
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.at += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return self.err("invalid character in exponent of numeric literal");
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.at += 1;
            }
        }
        let text = std::str::from_utf8(self.b.get(start..self.at).unwrap_or_default())
            .map_err(|_| JsonError("invalid number".into()))?;
        Ok(Value::Number(Number(text.into())))
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let digits = self
            .b
            .get(self.at..self.at + 4)
            .ok_or(JsonError("invalid escape".into()))?;
        let s = std::str::from_utf8(digits).map_err(|_| JsonError("invalid escape".into()))?;
        let v = u32::from_str_radix(s, 16).map_err(|_| JsonError("invalid escape".into()))?;
        self.at += 4;
        Ok(v)
    }

    /// A string, as encoding/json unquotes it (a lone surrogate becomes U+FFFD).
    fn string(&mut self) -> Result<String, JsonError> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return self.err("unexpected end of JSON input");
            };
            match c {
                b'"' => {
                    self.at += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.at += 1;
                    let Some(e) = self.peek() else {
                        return self.err("unexpected end of JSON input");
                    };
                    self.at += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            if (0xD800..0xDC00).contains(&hi) {
                                let save = self.at;
                                if self.eat(b"\\u") {
                                    let lo = self.hex4()?;
                                    if (0xDC00..0xE000).contains(&lo) {
                                        let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
                                        out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                                        continue;
                                    }
                                }
                                self.at = save;
                                out.push('\u{FFFD}');
                            } else {
                                out.push(char::from_u32(hi).unwrap_or('\u{FFFD}'));
                            }
                        }
                        _ => return self.err("invalid character in string escape code"),
                    }
                }
                0..=0x1f => return self.err("invalid character in string literal"),
                _ => {
                    // One UTF-8 character.
                    let rest = self.b.get(self.at..).unwrap_or_default();
                    let len = utf8_len(c);
                    let chunk = rest.get(..len).unwrap_or(rest);
                    match std::str::from_utf8(chunk) {
                        Ok(s) => {
                            out.push_str(s);
                            self.at += len;
                        }
                        Err(_) => {
                            out.push('\u{FFFD}');
                            self.at += 1;
                        }
                    }
                }
            }
        }
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// Writes a string as encoding/json does, HTML characters escaped.
pub fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", u32::from(c)));
            }
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Why a value has no JSON (OPA's `ast.JSON` errors).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToJsonError(pub String);

/// Writes a value as `json.Marshal(ast.JSON(v))` does: sets as sorted arrays, objects
/// keyed by strings, in sorted order.
pub fn to_json(v: &Value) -> Result<String, ToJsonError> {
    let mut out = String::new();
    write_json(&mut out, v)?;
    Ok(out)
}

fn write_json(out: &mut String, v: &Value) -> Result<(), ToJsonError> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if !is_json_number(n.text()) {
                return Err(ToJsonError(format!(
                    "json: invalid number literal {:?}",
                    n.text()
                )));
            }
            out.push_str(n.text());
        }
        Value::String(s) => write_json_string(out, s),
        Value::Array(a) => write_seq(out, a.iter())?,
        Value::Set(s) => write_seq(out, s.iter())?,
        Value::Object(m) => {
            // Go sorts a map[string]any's keys by their bytes.
            let mut entries: Vec<(&str, &Value)> = Vec::with_capacity(m.len());
            for (k, v) in m.iter() {
                let Value::String(k) = k else {
                    return Err(ToJsonError(format!(
                        "invalid ast.Object key type: {}",
                        k.type_name()
                    )));
                };
                entries.push((k, v));
            }
            entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push('{');
            for (i, (k, v)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(out, k);
                out.push(':');
                write_json(out, v)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn write_seq<'a>(out: &mut String, items: impl Iterator<Item = &'a Value>) -> Result<(), ToJsonError> {
    out.push('[');
    for (i, v) in items.enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json(out, v)?;
    }
    out.push(']');
    Ok(())
}

/// encoding/json's check of a json.Number's text.
fn is_json_number(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    let at = |i: usize| b.get(i).copied();
    if at(i) == Some(b'-') {
        i += 1;
    }
    match at(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            while matches!(at(i), Some(b'0'..=b'9')) {
                i += 1;
            }
        }
        _ => return false,
    }
    if at(i) == Some(b'.') {
        i += 1;
        if !matches!(at(i), Some(b'0'..=b'9')) {
            return false;
        }
        while matches!(at(i), Some(b'0'..=b'9')) {
            i += 1;
        }
    }
    if matches!(at(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(at(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !matches!(at(i), Some(b'0'..=b'9')) {
            return false;
        }
        while matches!(at(i), Some(b'0'..=b'9')) {
            i += 1;
        }
    }
    i == b.len()
}
