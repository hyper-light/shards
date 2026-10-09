//! JSON as Go's encoding/json reads it into `map[string]any` and typed fields, and as
//! securesystemslib's cjson writes it canonically (OLPC canonical JSON over Go's own
//! re-encoding): numbers kept as written, for the typed fields that take them and the
//! values passed through as `json.RawMessage`; strings decoded with invalid UTF-8 replaced
//! as Go replaces it; an object's members in order, the last of a name winning as Go's
//! map keeps it.

use std::fmt::Write as _;

/// A JSON value: a number as written, an object's members in document order.
#[derive(Debug, Clone, PartialEq)]
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
}

struct Parser<'a> {
    b: &'a [u8],
    at: usize,
}

/// Go's syntax errors, as `json.SyntaxError` words them.
fn syntax(msg: impl Into<String>) -> String {
    msg.into()
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.at).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn unexpected(&self, context: &str) -> String {
        match self.peek() {
            None => syntax("unexpected end of JSON input"),
            Some(c) => syntax(format!("invalid character {} {context}", quote_char(c))),
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > 10_000 {
            return Err(syntax("exceeded max depth"));
        }
        self.ws();
        match self.peek() {
            Some(b'{') => {
                self.at += 1;
                let mut members = Vec::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.at += 1;
                    return Ok(Value::Object(members));
                }
                loop {
                    self.ws();
                    if self.peek() != Some(b'"') {
                        return Err(self.unexpected("looking for beginning of object key string"));
                    }
                    let key = self.string()?;
                    self.ws();
                    if self.peek() != Some(b':') {
                        return Err(self.unexpected("after object key"));
                    }
                    self.at += 1;
                    let v = self.value(depth + 1)?;
                    members.push((key, v));
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {
                            self.at += 1;
                            return Ok(Value::Object(members));
                        }
                        _ => return Err(self.unexpected("after object key:value pair")),
                    }
                }
            }
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.at += 1;
                    return Ok(Value::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            return Ok(Value::Array(items));
                        }
                        _ => return Err(self.unexpected("after array element")),
                    }
                }
            }
            Some(b'"') => self.string().map(Value::String),
            Some(b't') => self.literal(b"true", Value::Bool(true)),
            Some(b'f') => self.literal(b"false", Value::Bool(false)),
            Some(b'n') => self.literal(b"null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.unexpected("looking for beginning of value")),
        }
    }

    fn literal(&mut self, word: &[u8], v: Value) -> Result<Value, String> {
        for (i, &c) in word.iter().enumerate() {
            match self.b.get(self.at + i) {
                Some(&got) if got == c => {}
                None => return Err(syntax("unexpected end of JSON input")),
                Some(&got) => {
                    return Err(syntax(format!(
                        "invalid character {} in literal {} (expecting {})",
                        quote_char(got),
                        String::from_utf8_lossy(word),
                        quote_char(c)
                    )));
                }
            }
        }
        self.at += word.len();
        Ok(v)
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.at;
        let digits = |p: &mut Parser<'_>| {
            let s = p.at;
            while p.peek().is_some_and(|c| c.is_ascii_digit()) {
                p.at += 1;
            }
            p.at > s
        };
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek() {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                digits(self);
            }
            _ => return Err(self.unexpected("in numeric literal")),
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            if !digits(self) {
                return Err(self.unexpected("after decimal point in numeric literal"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if !digits(self) {
                return Err(self.unexpected("in exponent of numeric literal"));
            }
        }
        let text = self.b.get(start..self.at).unwrap_or_default();
        Ok(Value::Number(String::from_utf8_lossy(text).into_owned()))
    }

    /// A string, its escapes decoded and invalid UTF-8 replaced (encoding/json unquote).
    fn string(&mut self) -> Result<String, String> {
        self.at += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(syntax("unexpected end of JSON input"));
            };
            match c {
                b'"' => {
                    self.at += 1;
                    return Ok(String::from_utf8_lossy(&out).into_owned());
                }
                b'\\' => {
                    self.at += 1;
                    let Some(e) = self.peek() else {
                        return Err(syntax("unexpected end of JSON input"));
                    };
                    self.at += 1;
                    match e {
                        b'"' | b'\\' | b'/' => out.push(e),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let mut r = self.hex4()?;
                            if (0xD800..0xDC00).contains(&r)
                                && self.b.get(self.at) == Some(&b'\\')
                                && self.b.get(self.at + 1) == Some(&b'u')
                            {
                                let save = self.at;
                                self.at += 2;
                                let low = self.hex4()?;
                                if (0xDC00..0xE000).contains(&low) {
                                    r = 0x10000 + ((r - 0xD800) << 10) + (low - 0xDC00);
                                } else {
                                    self.at = save;
                                    r = 0xFFFD;
                                }
                            } else if (0xD800..0xE000).contains(&r) {
                                r = 0xFFFD;
                            }
                            let ch = char::from_u32(r).unwrap_or('\u{FFFD}');
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        other => {
                            return Err(syntax(format!(
                                "invalid character {} in string escape code",
                                quote_char(other)
                            )));
                        }
                    }
                }
                c if c < 0x20 => {
                    return Err(syntax(format!(
                        "invalid character {} in string literal",
                        quote_char(c)
                    )));
                }
                _ => {
                    out.push(c);
                    self.at += 1;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let mut v = 0u32;
        for _ in 0..4 {
            let Some(c) = self.peek() else {
                return Err(syntax("unexpected end of JSON input"));
            };
            let d = (c as char).to_digit(16).ok_or_else(|| {
                syntax(format!(
                    "invalid character {} in \\u hexadecimal character escape",
                    quote_char(c)
                ))
            })?;
            v = v * 16 + d;
            self.at += 1;
        }
        Ok(v)
    }
}

/// How Go's scanner names a byte in its errors.
fn quote_char(c: u8) -> String {
    match c {
        b'\'' => "'\\''".into(),
        b'"' => "'\"'".into(),
        c if c.is_ascii_graphic() || c == b' ' => format!("'{}'", c as char),
        c => format!("'\\x{c:02x}'"),
    }
}

/// json.Unmarshal's parse of a whole document: one value, then only white space.
pub fn parse(b: &[u8]) -> Result<Value, String> {
    let mut p = Parser { b, at: 0 };
    let v = p.value(0)?;
    p.ws();
    if p.at != b.len() {
        return Err(p.unexpected("after top-level value"));
    }
    Ok(v)
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

/// A canonical value: what cjson writes, from what json.Marshal writes of the typed
/// metadata. Its numbers are already what Go's encoder would write.
#[derive(Debug, Clone, PartialEq)]
pub enum Canon {
    Null,
    Bool(bool),
    Int(String),
    String(String),
    Array(Vec<Canon>),
    Object(Vec<(String, Canon)>),
}

impl Canon {
    /// `map[string]any` passed through Go: numbers by way of float64 (`Value` to
    /// `any`, then marshaled); a number cjson cannot write an error.
    pub fn from_any(v: &Value) -> Result<Canon, String> {
        Ok(match v {
            Value::Null => Canon::Null,
            Value::Bool(b) => Canon::Bool(*b),
            Value::Number(n) => Canon::Int(number_through_float(n)?),
            Value::String(s) => Canon::String(s.clone()),
            Value::Array(a) => Canon::Array(a.iter().map(Canon::from_any).collect::<Result<_, _>>()?),
            Value::Object(m) => Canon::Object(dedup(m, Canon::from_any)?),
        })
    }

    /// A `json.RawMessage` passed through Go: its numbers as written.
    pub fn from_raw(v: &Value) -> Result<Canon, String> {
        Ok(match v {
            Value::Null => Canon::Null,
            Value::Bool(b) => Canon::Bool(*b),
            Value::Number(n) => Canon::Int(int_or_panic(n)?),
            Value::String(s) => Canon::String(s.clone()),
            Value::Array(a) => Canon::Array(a.iter().map(Canon::from_raw).collect::<Result<_, _>>()?),
            Value::Object(m) => Canon::Object(dedup(m, Canon::from_raw)?),
        })
    }

    /// EncodeCanonical's text: keys in byte order, only `\` and `"` escaped.
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Canon::Null => out.extend_from_slice(b"null"),
            Canon::Bool(true) => out.extend_from_slice(b"true"),
            Canon::Bool(false) => out.extend_from_slice(b"false"),
            Canon::Int(n) => out.extend_from_slice(n.as_bytes()),
            Canon::String(s) => string(out, s),
            Canon::Array(a) => {
                out.push(b'[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    v.encode(out);
                }
                out.push(b']');
            }
            Canon::Object(m) => {
                let mut sorted: Vec<&(String, Canon)> = m.iter().collect();
                sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
                out.push(b'{');
                for (i, (k, v)) in sorted.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    string(out, k);
                    out.push(b':');
                    v.encode(out);
                }
                out.push(b'}');
            }
        }
    }
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

/// An object's members once each, the last of a name winning, as a Go map holds them.
fn dedup(
    m: &[(String, Value)],
    f: fn(&Value) -> Result<Canon, String>,
) -> Result<Vec<(String, Canon)>, String> {
    let mut out: Vec<(String, Canon)> = Vec::new();
    for (k, v) in m {
        let c = f(v)?;
        match out.iter_mut().find(|(n, _)| n == k) {
            Some(slot) => slot.1 = c,
            None => out.push((k.clone(), c)),
        }
    }
    Ok(out)
}

/// cjson's refusal of a number that is not an int64, in its words.
fn int_or_panic(n: &str) -> Result<String, String> {
    if is_int64(n) {
        Ok(n.to_string())
    } else {
        Err(format!("Can't canonicalize floating point number '{n}'"))
    }
}

fn number_through_float(n: &str) -> Result<String, String> {
    let text = go_float_text(n).ok_or_else(|| format!("json: unsupported value: {n}"))?;
    int_or_panic(&text)
}

/// `%d` of an int64, for the typed fields Go marshals as numbers.
pub fn int(v: i64) -> Canon {
    let mut s = String::new();
    let _ = write!(s, "{v}");
    Canon::Int(s)
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

    #[test]
    fn canonical_json_sorts_and_escapes_as_cjson() {
        let v = parse(br#"{"b":1,"a":"q\"\\\u00e9<","a":"x","c":[true,null]}"#).unwrap();
        let mut out = Vec::new();
        Canon::from_any(&v).unwrap().encode(&mut out);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"a":"x","b":1,"c":[true,null]}"#
        );
        let v = parse("{\"s\":\"q\\\"\\\\é<\\n\"}".as_bytes()).unwrap();
        let mut out = Vec::new();
        Canon::from_any(&v).unwrap().encode(&mut out);
        assert_eq!(out, b"{\"s\":\"q\\\"\\\\\xc3\xa9<\n\"}");
        assert!(Canon::from_any(&parse(b"1.5").unwrap()).is_err());
        assert!(Canon::from_raw(&parse(b"1e3").unwrap()).is_err());
    }
}
