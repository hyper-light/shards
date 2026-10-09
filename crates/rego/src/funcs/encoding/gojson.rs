//! Go 1.26's encoding/json as OPA's JSON builtins run it: the scanner (scanner.go) that
//! `json.Valid` and the Decoder check input with, its error texts; `util.UnmarshalJSON`
//! (a Decoder, then `Token` for what follows the value); and `MarshalIndent`'s
//! re-indentation (indent.go).

use super::gofmt;
use crate::value::{self, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Continue,
    BeginLiteral,
    BeginObject,
    ObjectKey,
    ObjectValue,
    EndObject,
    BeginArray,
    ArrayValue,
    EndArray,
    SkipSpace,
    End,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum St {
    BeginValue,
    BeginValueOrEmpty,
    BeginStringOrEmpty,
    BeginString,
    EndValue,
    EndTop,
    InString,
    InStringEsc,
    InStringEscU,
    InStringEscU1,
    InStringEscU12,
    InStringEscU123,
    Neg,
    One,
    Zero,
    Dot,
    Dot0,
    E,
    ESign,
    E0,
    T,
    Tr,
    Tru,
    F,
    Fa,
    Fal,
    Fals,
    N,
    Nu,
    Nul,
    Error,
}

const PARSE_OBJECT_KEY: u8 = 0;
const PARSE_OBJECT_VALUE: u8 = 1;
const PARSE_ARRAY_VALUE: u8 = 2;
const MAX_NESTING_DEPTH: usize = 10000;

#[derive(Debug)]
struct Scanner {
    step: St,
    end_top: bool,
    parse_state: Vec<u8>,
    err: Option<String>,
}

fn is_space(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'\r' || c == b'\n'
}

/// quoteChar: a byte as the scanner's errors show it.
fn quote_char(c: u8) -> String {
    match c {
        b'\'' => "'\\''".to_string(),
        b'"' => "'\"'".to_string(),
        _ => {
            let mut buf = [0; 4];
            let s = gofmt::quote_bytes(char::from(c).encode_utf8(&mut buf).as_bytes());
            let inner = s.strip_prefix('"').and_then(|t| t.strip_suffix('"')).unwrap_or(&s);
            format!("'{inner}'")
        }
    }
}

impl Scanner {
    fn new() -> Scanner {
        Scanner {
            step: St::BeginValue,
            end_top: false,
            parse_state: Vec::new(),
            err: None,
        }
    }

    fn error(&mut self, c: u8, context: &str) -> Op {
        self.step = St::Error;
        self.err = Some(format!("invalid character {} {}", quote_char(c), context));
        Op::Error
    }

    fn eof(&mut self) -> Op {
        if self.err.is_some() {
            return Op::Error;
        }
        if self.end_top {
            return Op::End;
        }
        self.step(b' ');
        if self.end_top {
            return Op::End;
        }
        if self.err.is_none() {
            self.err = Some("unexpected end of JSON input".to_string());
        }
        Op::Error
    }

    fn push(&mut self, c: u8, state: u8, success: Op) -> Op {
        self.parse_state.push(state);
        if self.parse_state.len() <= MAX_NESTING_DEPTH {
            return success;
        }
        self.error(c, "exceeded max depth")
    }

    fn pop(&mut self) {
        self.parse_state.pop();
        if self.parse_state.is_empty() {
            self.step = St::EndTop;
            self.end_top = true;
        } else {
            self.step = St::EndValue;
        }
    }

    fn hex(c: u8) -> bool {
        c.is_ascii_hexdigit()
    }

    fn step(&mut self, c: u8) -> Op {
        match self.step {
            St::BeginValueOrEmpty => {
                if is_space(c) {
                    return Op::SkipSpace;
                }
                if c == b']' {
                    return self.end_value(c);
                }
                self.begin_value(c)
            }
            St::BeginValue => self.begin_value(c),
            St::BeginStringOrEmpty => {
                if is_space(c) {
                    return Op::SkipSpace;
                }
                if c == b'}' {
                    if let Some(last) = self.parse_state.last_mut() {
                        *last = PARSE_OBJECT_VALUE;
                    }
                    return self.end_value(c);
                }
                self.begin_string(c)
            }
            St::BeginString => self.begin_string(c),
            St::EndValue => self.end_value(c),
            St::EndTop => self.end_top_step(c),
            St::InString => {
                if c == b'"' {
                    self.step = St::EndValue;
                    return Op::Continue;
                }
                if c == b'\\' {
                    self.step = St::InStringEsc;
                    return Op::Continue;
                }
                if c < 0x20 {
                    return self.error(c, "in string literal");
                }
                Op::Continue
            }
            St::InStringEsc => match c {
                b'b' | b'f' | b'n' | b'r' | b't' | b'\\' | b'/' | b'"' => {
                    self.step = St::InString;
                    Op::Continue
                }
                b'u' => {
                    self.step = St::InStringEscU;
                    Op::Continue
                }
                _ => self.error(c, "in string escape code"),
            },
            St::InStringEscU | St::InStringEscU1 | St::InStringEscU12 | St::InStringEscU123 => {
                if Self::hex(c) {
                    self.step = match self.step {
                        St::InStringEscU => St::InStringEscU1,
                        St::InStringEscU1 => St::InStringEscU12,
                        St::InStringEscU12 => St::InStringEscU123,
                        _ => St::InString,
                    };
                    return Op::Continue;
                }
                self.error(c, "in \\u hexadecimal character escape")
            }
            St::Neg => {
                if c == b'0' {
                    self.step = St::Zero;
                    return Op::Continue;
                }
                if (b'1'..=b'9').contains(&c) {
                    self.step = St::One;
                    return Op::Continue;
                }
                self.error(c, "in numeric literal")
            }
            St::One => {
                if c.is_ascii_digit() {
                    self.step = St::One;
                    return Op::Continue;
                }
                self.zero(c)
            }
            St::Zero => self.zero(c),
            St::Dot => {
                if c.is_ascii_digit() {
                    self.step = St::Dot0;
                    return Op::Continue;
                }
                self.error(c, "after decimal point in numeric literal")
            }
            St::Dot0 => {
                if c.is_ascii_digit() {
                    return Op::Continue;
                }
                if c == b'e' || c == b'E' {
                    self.step = St::E;
                    return Op::Continue;
                }
                self.end_value(c)
            }
            St::E => {
                if c == b'+' || c == b'-' {
                    self.step = St::ESign;
                    return Op::Continue;
                }
                self.e_sign(c)
            }
            St::ESign => self.e_sign(c),
            St::E0 => {
                if c.is_ascii_digit() {
                    return Op::Continue;
                }
                self.end_value(c)
            }
            St::T => self.lit(c, b'r', St::Tr, "in literal true (expecting 'r')"),
            St::Tr => self.lit(c, b'u', St::Tru, "in literal true (expecting 'u')"),
            St::Tru => self.lit(c, b'e', St::EndValue, "in literal true (expecting 'e')"),
            St::F => self.lit(c, b'a', St::Fa, "in literal false (expecting 'a')"),
            St::Fa => self.lit(c, b'l', St::Fal, "in literal false (expecting 'l')"),
            St::Fal => self.lit(c, b's', St::Fals, "in literal false (expecting 's')"),
            St::Fals => self.lit(c, b'e', St::EndValue, "in literal false (expecting 'e')"),
            St::N => self.lit(c, b'u', St::Nu, "in literal null (expecting 'u')"),
            St::Nu => self.lit(c, b'l', St::Nul, "in literal null (expecting 'l')"),
            St::Nul => self.lit(c, b'l', St::EndValue, "in literal null (expecting 'l')"),
            St::Error => Op::Error,
        }
    }

    fn lit(&mut self, c: u8, want: u8, next: St, context: &str) -> Op {
        if c == want {
            self.step = next;
            return Op::Continue;
        }
        self.error(c, context)
    }

    fn begin_value(&mut self, c: u8) -> Op {
        if is_space(c) {
            return Op::SkipSpace;
        }
        match c {
            b'{' => {
                self.step = St::BeginStringOrEmpty;
                self.push(c, PARSE_OBJECT_KEY, Op::BeginObject)
            }
            b'[' => {
                self.step = St::BeginValueOrEmpty;
                self.push(c, PARSE_ARRAY_VALUE, Op::BeginArray)
            }
            b'"' => {
                self.step = St::InString;
                Op::BeginLiteral
            }
            b'-' => {
                self.step = St::Neg;
                Op::BeginLiteral
            }
            b'0' => {
                self.step = St::Zero;
                Op::BeginLiteral
            }
            b't' => {
                self.step = St::T;
                Op::BeginLiteral
            }
            b'f' => {
                self.step = St::F;
                Op::BeginLiteral
            }
            b'n' => {
                self.step = St::N;
                Op::BeginLiteral
            }
            b'1'..=b'9' => {
                self.step = St::One;
                Op::BeginLiteral
            }
            _ => self.error(c, "looking for beginning of value"),
        }
    }

    fn begin_string(&mut self, c: u8) -> Op {
        if is_space(c) {
            return Op::SkipSpace;
        }
        if c == b'"' {
            self.step = St::InString;
            return Op::BeginLiteral;
        }
        self.error(c, "looking for beginning of object key string")
    }

    fn end_value(&mut self, c: u8) -> Op {
        let Some(&ps) = self.parse_state.last() else {
            self.step = St::EndTop;
            self.end_top = true;
            return self.end_top_step(c);
        };
        if is_space(c) {
            self.step = St::EndValue;
            return Op::SkipSpace;
        }
        match ps {
            PARSE_OBJECT_KEY => {
                if c == b':' {
                    if let Some(last) = self.parse_state.last_mut() {
                        *last = PARSE_OBJECT_VALUE;
                    }
                    self.step = St::BeginValue;
                    return Op::ObjectKey;
                }
                self.error(c, "after object key")
            }
            PARSE_OBJECT_VALUE => {
                if c == b',' {
                    if let Some(last) = self.parse_state.last_mut() {
                        *last = PARSE_OBJECT_KEY;
                    }
                    self.step = St::BeginString;
                    return Op::ObjectValue;
                }
                if c == b'}' {
                    self.pop();
                    return Op::EndObject;
                }
                self.error(c, "after object key:value pair")
            }
            _ => {
                if c == b',' {
                    self.step = St::BeginValue;
                    return Op::ArrayValue;
                }
                if c == b']' {
                    self.pop();
                    return Op::EndArray;
                }
                self.error(c, "after array element")
            }
        }
    }

    fn end_top_step(&mut self, c: u8) -> Op {
        if !is_space(c) {
            self.error(c, "after top-level value");
        }
        Op::End
    }

    fn zero(&mut self, c: u8) -> Op {
        if c == b'.' {
            self.step = St::Dot;
            return Op::Continue;
        }
        if c == b'e' || c == b'E' {
            self.step = St::E;
            return Op::Continue;
        }
        self.end_value(c)
    }

    fn e_sign(&mut self, c: u8) -> Op {
        if c.is_ascii_digit() {
            self.step = St::E0;
            return Op::Continue;
        }
        self.error(c, "in exponent of numeric literal")
    }
}

/// json.Valid's checkValid: nil, or the scanner's error.
pub fn check_valid(data: &[u8]) -> Result<(), String> {
    let mut s = Scanner::new();
    for &c in data {
        if s.step(c) == Op::Error {
            return Err(s.err.unwrap_or_default());
        }
    }
    if s.eof() == Op::Error {
        return Err(s.err.unwrap_or_default());
    }
    Ok(())
}

/// Decoder.readValue from `start`: where the value ends, or the Decoder's error.
fn read_value(data: &[u8], start: usize) -> Result<usize, String> {
    let mut s = Scanner::new();
    let mut i = start;
    while let Some(&c) = data.get(i) {
        match s.step(c) {
            Op::End => return Ok(i),
            Op::EndObject | Op::EndArray => {
                if s.end_value(b' ') == Op::End {
                    return Ok(i + 1);
                }
            }
            Op::Error => return Err(s.err.unwrap_or_default()),
            _ => {}
        }
        i += 1;
    }
    if s.step(b' ') == Op::End {
        return Ok(data.len());
    }
    let rest = data.get(start..).unwrap_or_default();
    if rest.iter().any(|&c| !is_space(c)) {
        Err("unexpected EOF".to_string())
    } else {
        Err("EOF".to_string())
    }
}

fn parse(data: &[u8]) -> Result<Value, String> {
    let text = std::str::from_utf8(data).map_err(|_| "invalid UTF-8".to_string())?;
    value::from_json(text).map_err(|e| e.0)
}

/// util.UnmarshalJSON: one value read as a json.Decoder (UseNumber) reads it, then
/// `Token` for anything after it.
pub fn unmarshal(data: &[u8]) -> Result<Value, String> {
    let end = read_value(data, 0)?;
    let v = parse(data.get(..end).unwrap_or_default())?;
    let mut pos = end;
    while data.get(pos).is_some_and(|&c| is_space(c)) {
        pos += 1;
    }
    let Some(&c) = data.get(pos) else {
        return Ok(v);
    };
    let tok = match c {
        b'[' | b'{' => char::from(c).to_string(),
        b']' | b'}' | b':' | b',' => {
            return Err(format!("invalid character {} looking for beginning of value", quote_char(c)));
        }
        _ => {
            let end2 = read_value(data, pos)?;
            let x = parse(data.get(pos..end2).unwrap_or_default())?;
            match x {
                Value::Null => return Ok(v),
                Value::Bool(b) => format!("%!s(bool={b})"),
                Value::Number(n) => n.text().to_string(),
                Value::String(s) => s.to_string(),
                _ => return Ok(v),
            }
        }
    };
    Err(format!("error: invalid character '{tok}' after top-level value"))
}

/// json.Indent's appendIndent over compact, valid JSON.
pub fn indent(src: &str, prefix: &str, indent: &str) -> String {
    let mut dst = String::with_capacity(src.len() * 2);
    let mut s = Scanner::new();
    let mut need_indent = false;
    let mut depth = 0usize;
    let newline = |dst: &mut String, depth: usize| {
        dst.push('\n');
        dst.push_str(prefix);
        for _ in 0..depth {
            dst.push_str(indent);
        }
    };
    let bytes = src.as_bytes();
    let mut i = 0;
    while let Some(&c) = bytes.get(i) {
        let op = s.step(c);
        // The whole UTF-8 character, which the scanner passes through as it passes
        // its bytes.
        let w = super::yaml_width(c);
        let ch = src.get(i..i + w).unwrap_or("");
        i += w;
        if op == Op::SkipSpace {
            continue;
        }
        if op == Op::Error {
            break;
        }
        if need_indent && op != Op::EndObject && op != Op::EndArray {
            need_indent = false;
            depth += 1;
            newline(&mut dst, depth);
        }
        if op == Op::Continue {
            dst.push_str(ch);
            continue;
        }
        match c {
            b'{' | b'[' => {
                need_indent = true;
                dst.push_str(ch);
            }
            b',' => {
                dst.push_str(ch);
                newline(&mut dst, depth);
            }
            b':' => {
                dst.push_str(ch);
                dst.push(' ');
            }
            b'}' | b']' => {
                if need_indent {
                    need_indent = false;
                } else {
                    depth = depth.saturating_sub(1);
                    newline(&mut dst, depth);
                }
                dst.push_str(ch);
            }
            _ => dst.push_str(ch),
        }
    }
    dst
}
