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
    // |a| / |b| = (ma / mb) × 2^d2 × 5^d5, the power of five put where it multiplies.
    let d2 = i128::from(a.2) - i128::from(b.2);
    let d5 = i128::from(a.3) - i128::from(b.3);
    // Two i64s differ by less than 2^64.
    let n = u64::try_from(d5.unsigned_abs()).unwrap_or(u64::MAX);
    let mag = if d5 < 0 {
        scaled_cmp(&b.1, n, -d2, &a.1).reverse()
    } else {
        scaled_cmp(&a.1, n, d2, &b.1)
    };
    if sa < 0 { mag.reverse() } else { mag }
}

/// `p × 5^n × 2^s` against `q`, both mantissas nonzero, in work the mantissas bound.
///
/// While `n <= bits(q) / 2`, the product is made exactly: then it has at most
/// `bits(p) + 1.17 bits(q)` bits. Past that, `5^n > 4^n >= 2^(bits(q) + 1) > q`, so 5^n
/// divides no multiple of q by a power of two and the two sides are never equal; bounds
/// on 5^n decide which side is larger, made more precise until they do. Each doubling of
/// their precision halves how near the sides may be and still be told apart, and the
/// sides differ, so the loop ends; a pair of mantissas made to put the sides within
/// 2^-k of each other costs precision of about k bits.
fn scaled_cmp(p: &BigUint, n: u64, s: i128, q: &BigUint) -> Ordering {
    if let Some(small) = u32::try_from(n).ok().filter(|n| u64::from(*n) <= q.bits() / 2) {
        return shifted_cmp(&(p * BigUint::from(5u32).pow(small)), s, q);
    }
    // First, bounds within 2^-(bits(p) + bits(q)) of 5^n: each squaring doubles a bound's
    // error, so they start bitlen(n) + 2 bits more precise than that.
    let mut prec = p.bits() + q.bits() + u64::from(u64::BITS - n.leading_zeros()) + 2;
    loop {
        let ((lo, at_lo), (hi, at_hi)) = number::pow5_wide(n, prec);
        if shifted_cmp(&(p * &lo), s + at_lo, q) != Ordering::Less {
            return Ordering::Greater;
        }
        if shifted_cmp(&(p * &hi), s + at_hi, q) != Ordering::Greater {
            return Ordering::Less;
        }
        prec = prec.saturating_mul(2);
    }
}

/// `x × 2^s` against `y`, y nonzero: by their lengths, else exactly, the shift then
/// shorter than either.
fn shifted_cmp(x: &BigUint, s: i128, y: &BigUint) -> Ordering {
    if x.bits() == 0 {
        return Ordering::Less;
    }
    let (lx, ly) = (i128::from(x.bits()) + s, i128::from(y.bits()));
    if lx != ly {
        return lx.cmp(&ly);
    }
    let shift = u64::try_from(s.unsigned_abs()).unwrap_or(u64::MAX);
    if s >= 0 {
        (x << shift).cmp(y)
    } else {
        x.cmp(&(y << shift))
    }
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

/// A ground value. Values order by kind (null, booleans, numbers, strings, arrays,
/// objects, sets), then collections member by member, then by length.
#[derive(Debug, Clone)]
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
    fn kind(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Number(_) => 2,
            Value::String(_) => 3,
            Value::Array(_) => 4,
            Value::Object(_) => 5,
            Value::Set(_) => 6,
        }
    }
}

/// Values compare a pair of members at a time, collections that are one and the same
/// at once: a value nests deeper than a thread has stack for a frame a level.
impl Ord for Value {
    fn cmp(&self, other: &Value) -> Ordering {
        enum Step<'a> {
            Pair(&'a Value, &'a Value),
            Len(usize, usize),
        }
        // Scalars, the most compared, without the walk's stack.
        match (self, other) {
            (Value::Bool(x), Value::Bool(y)) => return x.cmp(y),
            (Value::Number(x), Value::Number(y)) => return x.cmp(y),
            (Value::String(x), Value::String(y)) => return x.cmp(y),
            _ => {}
        }
        let mut todo = vec![Step::Pair(self, other)];
        while let Some(step) = todo.pop() {
            let (a, b) = match step {
                Step::Pair(a, b) => (a, b),
                Step::Len(x, y) => match x.cmp(&y) {
                    Ordering::Equal => continue,
                    o => return o,
                },
            };
            let o = match (a, b) {
                (Value::Null, Value::Null) => Ordering::Equal,
                (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
                (Value::Number(x), Value::Number(y)) => x.cmp(y),
                (Value::String(x), Value::String(y)) => x.cmp(y),
                (Value::Array(x), Value::Array(y)) => {
                    if !Rc::ptr_eq(x, y) {
                        todo.push(Step::Len(x.len(), y.len()));
                        todo.extend(x.iter().zip(y.iter()).rev().map(|(p, q)| Step::Pair(p, q)));
                    }
                    continue;
                }
                (Value::Object(x), Value::Object(y)) => {
                    if !Rc::ptr_eq(x, y) {
                        todo.push(Step::Len(x.len(), y.len()));
                        for ((k1, v1), (k2, v2)) in x.iter().zip(y.iter()).rev() {
                            todo.push(Step::Pair(v1, v2));
                            todo.push(Step::Pair(k1, k2));
                        }
                    }
                    continue;
                }
                (Value::Set(x), Value::Set(y)) => {
                    if !Rc::ptr_eq(x, y) {
                        todo.push(Step::Len(x.len(), y.len()));
                        todo.extend(x.iter().zip(y.iter()).rev().map(|(p, q)| Step::Pair(p, q)));
                    }
                    continue;
                }
                (a, b) => a.kind().cmp(&b.kind()),
            };
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Value) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Value {}

/// A value nests as deep as json.patch puts one in another, past any frame a level a
/// thread's stack holds: its last owner drops its members one after another.
impl Drop for Value {
    fn drop(&mut self) {
        let mut rest = Vec::new();
        take_members(self, &mut rest);
        while let Some(mut v) = rest.pop() {
            take_members(&mut v, &mut rest);
        }
    }
}

/// Moves a container's members to `out` when this is its last owner.
fn take_members(v: &mut Value, out: &mut Vec<Value>) {
    match v {
        Value::Array(a) => {
            if let Some(a) = Rc::get_mut(a) {
                out.append(a);
            }
        }
        Value::Object(o) => {
            if let Some(o) = Rc::get_mut(o) {
                for (k, v) in std::mem::take(o) {
                    out.push(k);
                    out.push(v);
                }
            }
        }
        Value::Set(s) => {
            if let Some(s) = Rc::get_mut(s) {
                out.extend(std::mem::take(s));
            }
        }
        _ => {}
    }
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
    let v = p.value()?;
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

    /// A value, its containers kept open on a stack of its own (as Go's decoder keeps its
    /// own): the deepest document reads without a frame a level.
    fn value(&mut self) -> Result<Value, JsonError> {
        enum Open {
            Array(Vec<Value>),
            /// The members so far, and the key the next value is for.
            Object(BTreeMap<Value, Value>, String),
        }
        let mut open: Vec<Open> = Vec::new();
        loop {
            if open.len() > MAX_DEPTH {
                return self.err("exceeded max depth");
            }
            let mut v = match self.peek() {
                Some(b'n') if self.eat(b"null") => Value::Null,
                Some(b't') if self.eat(b"true") => Value::Bool(true),
                Some(b'f') if self.eat(b"false") => Value::Bool(false),
                Some(b'"') => Value::String(self.string()?.into()),
                Some(b'[') => {
                    self.at += 1;
                    self.ws();
                    if self.peek() == Some(b']') {
                        self.at += 1;
                        Value::array(Vec::new())
                    } else {
                        open.push(Open::Array(Vec::new()));
                        self.ws();
                        continue;
                    }
                }
                Some(b'{') => {
                    self.at += 1;
                    self.ws();
                    if self.peek() == Some(b'}') {
                        self.at += 1;
                        Value::object(BTreeMap::new())
                    } else {
                        let k = self.key()?;
                        open.push(Open::Object(BTreeMap::new(), k));
                        continue;
                    }
                }
                Some(b'-' | b'0'..=b'9') => self.number()?,
                _ => return self.err("invalid character looking for beginning of value"),
            };
            // The value read goes into the container open last, which may end with it,
            // and so on out: then the next value is read.
            loop {
                match open.last_mut() {
                    None => return Ok(v),
                    Some(Open::Array(items)) => {
                        items.push(v);
                        self.ws();
                        match self.peek() {
                            Some(b',') => {
                                self.at += 1;
                                self.ws();
                                break;
                            }
                            Some(b']') => {
                                self.at += 1;
                                let items = match open.pop() {
                                    Some(Open::Array(items)) => items,
                                    _ => Vec::new(),
                                };
                                v = Value::array(items);
                            }
                            _ => return self.err("invalid character in array"),
                        }
                    }
                    Some(Open::Object(members, key)) => {
                        members.insert(Value::String(std::mem::take(key).into()), v);
                        self.ws();
                        match self.peek() {
                            Some(b',') => {
                                self.at += 1;
                                let k = self.key()?;
                                if let Some(Open::Object(_, key)) = open.last_mut() {
                                    *key = k;
                                }
                                break;
                            }
                            Some(b'}') => {
                                self.at += 1;
                                let members = match open.pop() {
                                    Some(Open::Object(members, _)) => members,
                                    _ => BTreeMap::new(),
                                };
                                v = Value::object(members);
                            }
                            _ => return self.err("invalid character after object key:value pair"),
                        }
                    }
                }
            }
        }
    }

    /// An object's member's key and its colon, the value next.
    fn key(&mut self) -> Result<String, JsonError> {
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
        Ok(k)
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

/// The JSON of a value, written a member at a time: a value nests deeper than a thread
/// has stack for a frame a level.
fn write_json(out: &mut String, v: &Value) -> Result<(), ToJsonError> {
    enum P<'a> {
        Text(&'static str),
        Key(&'a str),
        Val(&'a Value),
    }
    let mut todo = vec![P::Val(v)];
    while let Some(p) = todo.pop() {
        let v = match p {
            P::Text(t) => {
                out.push_str(t);
                continue;
            }
            P::Key(k) => {
                write_json_string(out, k);
                out.push(':');
                continue;
            }
            P::Val(v) => v,
        };
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
            Value::Array(_) | Value::Set(_) => {
                let items: Vec<&Value> = match v {
                    Value::Array(a) => a.iter().collect(),
                    Value::Set(s) => s.iter().collect(),
                    _ => Vec::new(),
                };
                out.push('[');
                todo.push(P::Text("]"));
                for (i, x) in items.into_iter().enumerate().rev() {
                    todo.push(P::Val(x));
                    if i > 0 {
                        todo.push(P::Text(","));
                    }
                }
            }
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
                todo.push(P::Text("}"));
                for (i, (k, x)) in entries.into_iter().enumerate().rev() {
                    todo.push(P::Val(x));
                    todo.push(P::Key(k));
                    if i > 0 {
                        todo.push(P::Text(","));
                    }
                }
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use std::cmp::Ordering::{self, Equal, Greater, Less};
    use std::sync::mpsc;
    use std::time::Duration;

    use num_bigint::BigUint;

    use super::{compare_exact, exact, number_compare};

    /// `f`'s answer, or a failure once `secs` pass: the comparisons here take
    /// microseconds, and scaling by their exponents, hours.
    fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(Duration::from_secs(secs))
            .expect("the comparison is still working")
    }

    fn compares(a: &'static str, b: &'static str) -> Ordering {
        within(60, move || number_compare(a, b))
    }

    /// The comparison as it was: both scaled to 2^min2 × 5^min5, for exponents close
    /// enough to afford it.
    fn scaled_both(a: &str, b: &str) -> Ordering {
        let (a, b) = (exact(a).unwrap(), exact(b).unwrap());
        let sign = |x: &(bool, BigUint, i64, i64)| match (x.1.bits() == 0, x.0) {
            (true, _) => 0,
            (false, true) => -1,
            (false, false) => 1,
        };
        let (sa, sb) = (sign(&a), sign(&b));
        if sa != sb || sa == 0 {
            return sa.cmp(&sb);
        }
        let (min2, min5) = (a.2.min(b.2), a.3.min(b.3));
        let scale = |x: &(bool, BigUint, i64, i64)| {
            (&x.1 * BigUint::from(5u32).pow(u32::try_from(x.3 - min5).unwrap()))
                << u64::try_from(x.2 - min2).unwrap()
        };
        let mag = scale(&a).cmp(&scale(&b));
        if sa < 0 { mag.reverse() } else { mag }
    }

    /// CI's fuzzing (run 43250570050's oom-36ae0bf3, and oom-b6644dc2 before it):
    /// `lt` of numbers an exponent of billions apart.
    #[test]
    fn the_fuzzers_comparisons_answer_without_scaling_by_their_exponents() {
        assert_eq!(compares("33", "3e33333333332"), Less);
        assert_eq!(compares("3e33333333332", "33"), Greater);
        assert_eq!(compares("1202022222200000E0297138122009577", "1"), Greater);
    }

    #[test]
    fn numbers_compare_exactly_at_every_distance() {
        let cases = [
            // Equal magnitudes, written apart.
            ("1.5e999999999", "15e999999998", Equal),
            ("1e-999999999", "0.1e-999999998", Equal),
            ("4e9223372036854775807", "40e9223372036854775806", Equal),
            // Adjacent magnitudes: apart in the last of their digits.
            ("9.999999999999999999e999999999", "1e1000000000", Less),
            ("1e1000000000", "1.000000000000000000001e1000000000", Less),
            ("99999999999999999999e-1000000000", "1e-999999980", Less),
            // The exponents' extremes.
            ("1e9223372036854775807", "1e9223372036854775806", Greater),
            ("1e-9223372036854775808", "1e9223372036854775807", Less),
            ("2e-9223372036854775808", "1e-9223372036854775808", Greater),
            // Mixed signs, and negatives, the larger in magnitude the smaller.
            ("-1e999999999", "1e-999999999", Less),
            ("-1e999999999", "-1e999999998", Less),
            ("-1e-999999999", "-1e-999999998", Greater),
            ("-3e33333333332", "33", Less),
            // Zero, whatever its exponent.
            ("0e99999999999", "0", Equal),
            ("-0e99999999999", "0e-99999999999", Equal),
            ("0e99999999999", "1e-99999999999", Less),
            ("0e99999999999", "-1e-99999999999", Greater),
            // Powers of ten against powers of two (to_number's hex floats) close by, at
            // convergents of log2(10): 10^-E and 2^-P within 2^-64 of each other at the last.
            ("1e-100000000000", "0x1p-332192809488", Less),
            ("1e-100000000000", "0x1p-332192809489", Greater),
            ("1e-21306", "0x1p-70777", Greater),
            ("1e-76573", "0x1p-254370", Less),
            ("1e-97879", "0x1p-325147", Greater),
            ("1e-579001193", "0x1p-1923400330", Less),
            ("1e-1329339201633350533", "0x1p-4415969241540963378", Greater),
            ("1e1329339201633350533", "0x1p4415969241540963378", Less),
            ("-1e-1329339201633350533", "-0x1p-4415969241540963378", Less),
        ];
        for (a, b, want) in cases {
            assert_eq!(compares(a, b), want, "{a} against {b}");
            assert_eq!(compares(b, a), want.reverse(), "{b} against {a}");
        }
    }

    /// Every order the scaled comparison gave where it could, kept: random mantissas
    /// written in decimal and hexadecimal, with exponents a few thousand apart, so
    /// that both the exact products and the bounds on powers of five decide. (Before
    /// it, number_compare's OPA shortcuts, unchanged, can call two numbers equal.)
    #[test]
    fn orders_are_those_the_exact_scaling_gave() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move |n: u64| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
        };
        let mut text = || {
            let sign = if next(3) == 0 { "-" } else { "" };
            let digits: String = (0..1 + next(30))
                .map(|_| char::from(b'0' + next(10) as u8))
                .collect();
            let exp = next(4000) as i64 - 2000;
            match next(4) {
                0 => format!("{sign}{digits}e{exp}"),
                1 => format!("{sign}0.{digits}e{exp}"),
                2 => format!("{sign}0x{digits}p{}", exp * 3),
                _ => format!("{sign}{digits}"),
            }
        };
        for _ in 0..20000 {
            let (a, b) = (text(), text());
            let got = compare_exact(&exact(&a).unwrap(), &exact(&b).unwrap());
            assert_eq!(got, scaled_both(&a, &b), "{a} against {b}");
        }
    }
}
