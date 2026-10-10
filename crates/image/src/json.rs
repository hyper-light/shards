//! JSON as Go's `encoding/json` reads and writes it.
//!
//! Reading: the text must be valid JSON as Go's scanner checks it, nesting at most 10,000
//! deep, and strings decode as Go decodes them, an invalid byte or a lone surrogate becoming
//! U+FFFD. The scan is iterative, its stack on the heap, so no nesting exhausts a thread's
//! stack. BuildKit reads a JSON array of strings this way (`parser/line_parsers.go`,
//! `parseJSON`), and an image's config.
//!
//! Writing: strings escaped as `encodeState.string` escapes them with HTML escaping on, as
//! `json.Marshal` does, so documents BuildKit writes come out byte for byte.

use std::fmt::Write as _;

use crate::go;

/// A JSON value. Strings are Go strings, bytes; numbers keep their text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(Vec<u8>),
    /// The string decoded, and its text as written where that differs (an escape, or a
    /// byte that is not UTF-8): Go reads a time from the text as written.
    String(Vec<u8>, Option<Vec<u8>>),
    Array(Vec<Value>),
    /// Members in order, duplicates kept, as Go's decoder meets them.
    Object(Vec<(Vec<u8>, Value)>),
}

/// Dropped without recursion. A value as deep as Go's scanner allows, its containers
/// dropped one inside another, took more stack than a thread may have (measured: over
/// 512 KiB in a release build, over 2 MiB in a debug one), and a stack overflow ends the
/// process: what each holds is moved onto a heap stack and dropped a level at a time.
impl Drop for Value {
    fn drop(&mut self) {
        let mut held = Vec::new();
        take_contents(self, &mut held);
        while let Some(mut v) = held.pop() {
            take_contents(&mut v, &mut held);
        }
    }
}

fn take_contents(v: &mut Value, into: &mut Vec<Value>) {
    match v {
        Value::Array(elements) => into.append(elements),
        Value::Object(members) => into.extend(members.drain(..).map(|(_, v)| v)),
        _ => {}
    }
}

/// Whether a number in `v` is past a float64's range, which Go's decoder into `any`
/// refuses: it reads every number as a float64, and one past the largest is infinite. One
/// too small is 0, which it takes.
fn overflows(v: &Value) -> bool {
    let mut stack = vec![v];
    while let Some(v) = stack.pop() {
        match v {
            Value::Number(n) => {
                if std::str::from_utf8(n)
                    .ok()
                    .and_then(|n| n.parse::<f64>().ok())
                    .is_some_and(f64::is_infinite)
                {
                    return true;
                }
            }
            Value::Array(elements) => stack.extend(elements),
            Value::Object(members) => stack.extend(members.iter().map(|(_, v)| v)),
            _ => {}
        }
    }
    false
}

/// What a JSON array of strings is, if `text` is one.
#[derive(Debug)]
pub enum Array {
    /// Valid JSON, an array whose elements are all strings.
    Strings(Vec<Vec<u8>>),
    /// Valid JSON, an array with an element that is no string.
    NotStrings,
    /// Not valid JSON, or not an array.
    Not,
}

/// `parseJSON`'s reading of `text`: `json.Unmarshal` into `[]any`, whose failure, a number
/// past a float64's range included, makes it no JSON array, then each element a string.
pub fn array(text: &[u8]) -> Array {
    let Ok(mut v) = parse(text) else {
        return Array::Not;
    };
    if overflows(&v) {
        return Array::Not;
    }
    let Value::Array(elements) = &mut v else {
        return Array::Not;
    };
    let mut strings = Vec::with_capacity(elements.len());
    for e in elements.iter_mut() {
        match e {
            Value::String(s, _) => strings.push(std::mem::take(s)),
            _ => return Array::NotStrings,
        }
    }
    Array::Strings(strings)
}

/// Go's scanner refuses JSON nested deeper than this (`encoding/json` scanner.go,
/// `maxNestingDepth`).
const MAX_DEPTH: usize = 10_000;

/// A container being read, and what it holds so far.
enum Open {
    Array(Vec<Value>),
    /// The members so far, and the key whose value comes next.
    Object(Vec<(Vec<u8>, Value)>, Option<Vec<u8>>),
}

/// The message of Go's `SyntaxError` for running out of input.
const END: &[u8] = b"unexpected end of JSON input";

/// encoding/json's Indent of compact `text` (prefix none, indent two spaces), as
/// `MarshalIndent` writes a value: each member and element on a line of its own, `": "`
/// after a key, an empty object or array as it is.
pub fn indent(text: &[u8]) -> Vec<u8> {
    indent_with(text, b"  ")
}

/// [`indent`] with `unit` for each level (`MarshalIndent(v, "", unit)`).
pub fn indent_with(text: &[u8], unit: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 2);
    let (mut depth, mut in_string, mut escaped, mut need_indent) = (0usize, false, false, false);
    let newline = |out: &mut Vec<u8>, depth: usize| {
        out.push(b'\n');
        for _ in 0..depth {
            out.extend_from_slice(unit);
        }
    };
    for &c in text {
        if in_string {
            out.push(c);
            match c {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        if need_indent && c != b']' && c != b'}' {
            need_indent = false;
            depth += 1;
            newline(&mut out, depth);
        }
        match c {
            b'"' => {
                in_string = true;
                out.push(c);
            }
            b'{' | b'[' => {
                need_indent = true;
                out.push(c);
            }
            b',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            b':' => out.extend_from_slice(b": "),
            b'}' | b']' => {
                if need_indent {
                    need_indent = false;
                } else {
                    depth = depth.saturating_sub(1);
                    newline(&mut out, depth);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// encoding/json's Compact: `text`, one JSON value as Go's scanner reads one, with the
/// whitespace between its tokens taken out and nothing else changed; else the scanner's
/// message for what it refuses. docker/cli sends a seccomp profile's file so.
pub fn compact(text: &[u8]) -> Result<Vec<u8>, String> {
    parse(text).map_err(|m| String::from_utf8_lossy(&m).into_owned())?;
    let mut out = Vec::with_capacity(text.len());
    let (mut in_string, mut escaped) = (false, false);
    for &b in text {
        if in_string {
            out.push(b);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
        } else if !matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
            out.push(b);
            in_string = b == b'"';
        }
    }
    Ok(out)
}

/// `text` as one JSON value with only whitespace around it, or Go's `SyntaxError` message
/// for the first thing its scanner refuses.
pub fn parse(text: &[u8]) -> Result<Value, Vec<u8>> {
    let mut s = Scan { b: text, at: 0 };
    let mut stack: Vec<Open> = Vec::new();
    let value = 'outer: loop {
        s.space();
        // A value, or the end of the container just opened.
        let c = s.peek().ok_or(END)?;
        let mut done = match c {
            b'[' | b'{' => {
                if stack.len() >= MAX_DEPTH {
                    return Err(fail(c, "exceeded max depth"));
                }
                s.at += 1;
                s.space();
                let next = s.peek().ok_or(END)?;
                if c == b'[' {
                    if next == b']' {
                        s.at += 1;
                        Value::Array(Vec::new())
                    } else {
                        stack.push(Open::Array(Vec::new()));
                        continue;
                    }
                } else if next == b'}' {
                    s.at += 1;
                    Value::Object(Vec::new())
                } else {
                    let key = s.key()?;
                    stack.push(Open::Object(Vec::new(), Some(key)));
                    continue;
                }
            }
            b'"' => {
                let start = s.at;
                let text = s.string()?;
                let raw = go::span(s.b, start + 1, s.at - 1);
                let raw = (raw != text.as_slice()).then(|| raw.to_vec());
                Value::String(text, raw)
            }
            _ => s.scalar()?,
        };
        // Add the value to its container, closing what ends. A value outside any
        // container is the document's.
        loop {
            let Some(top) = stack.last_mut() else {
                break 'outer done;
            };
            match top {
                Open::Array(elements) => elements.push(done),
                Open::Object(members, key) => members.push((key.take().unwrap_or_default(), done)),
            }
            s.space();
            let c = s.peek().ok_or(END)?;
            match (c, &mut *top) {
                (b',', Open::Array(_)) => {
                    s.at += 1;
                    break;
                }
                (b',', Open::Object(_, key)) => {
                    s.at += 1;
                    s.space();
                    *key = Some(s.key()?);
                    break;
                }
                (b']', Open::Array(_)) | (b'}', Open::Object(..)) => {
                    s.at += 1;
                    done = match stack.pop() {
                        Some(Open::Array(elements)) => Value::Array(elements),
                        Some(Open::Object(members, _)) => Value::Object(members),
                        None => return Err(END.to_vec()),
                    };
                }
                (_, Open::Array(_)) => return Err(fail(c, "after array element")),
                (_, Open::Object(..)) => return Err(fail(c, "after object key:value pair")),
            }
        }
    };
    s.space();
    match s.peek() {
        None => Ok(value),
        Some(c) => Err(fail(c, "after top-level value")),
    }
}

/// `SyntaxError`'s message for byte `c` where the scanner wanted something else:
/// `"invalid character " + quoteChar(c) + " " + context`.
fn fail(c: u8, context: &str) -> Vec<u8> {
    let quoted = match c {
        b'\'' => r"'\''".to_string(),
        b'"' => "'\"'".to_string(),
        // string(c) is the rune c: a byte past ASCII is its Latin-1 character.
        _ => {
            let mut rune = Vec::new();
            go::push(&mut rune, u32::from(c));
            let q = go::quote(&rune);
            format!("'{}'", q.get(1..q.len().saturating_sub(1)).unwrap_or_default())
        }
    };
    format!("invalid character {quoted} {context}").into_bytes()
}

struct Scan<'a> {
    b: &'a [u8],
    at: usize,
}

impl Scan<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.at).copied()
    }

    /// The next byte, which must be there.
    fn next(&mut self) -> Result<u8, Vec<u8>> {
        let c = self.peek().ok_or(END)?;
        self.at += 1;
        Ok(c)
    }

    /// The next byte within a literal, a number or an escape: at the end of the input,
    /// a space, as Go's scanner feeds one to the state it is in (`scanner.eof`).
    fn inner(&mut self) -> u8 {
        match self.peek() {
            Some(c) => {
                self.at += 1;
                c
            }
            None => b' ',
        }
    }

    /// JSON's whitespace, the only kind Go's scanner skips.
    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    /// A member's key and the `:` after it.
    fn key(&mut self) -> Result<Vec<u8>, Vec<u8>> {
        let c = self.peek().ok_or(END)?;
        if c != b'"' {
            return Err(fail(c, "looking for beginning of object key string"));
        }
        let key = self.string()?;
        self.space();
        let c = self.next()?;
        if c != b':' {
            return Err(fail(c, "after object key"));
        }
        Ok(key)
    }

    /// `true`, `false`, `null` or a number.
    fn scalar(&mut self) -> Result<Value, Vec<u8>> {
        let c = self.next()?;
        let (rest, value): (&[u8], Value) = match c {
            b't' => (b"rue", Value::Bool(true)),
            b'f' => (b"alse", Value::Bool(false)),
            b'n' => (b"ull", Value::Null),
            b'-' | b'0'..=b'9' => {
                self.at -= 1;
                return self.number();
            }
            _ => return Err(fail(c, "looking for beginning of value")),
        };
        let word = match c {
            b't' => "true",
            b'f' => "false",
            _ => "null",
        };
        for &want in rest {
            let got = self.inner();
            if got != want {
                return Err(fail(
                    got,
                    &format!("in literal {word} (expecting '{}')", char::from(want)),
                ));
            }
        }
        Ok(value)
    }

    /// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`, ending where Go's scanner ends it.
    fn number(&mut self) -> Result<Value, Vec<u8>> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.inner() {
            b'0' => {}
            b'1'..=b'9' => self.digits(),
            c => return Err(fail(c, "in numeric literal")),
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            match self.inner() {
                b'0'..=b'9' => self.digits(),
                c => return Err(fail(c, "after decimal point in numeric literal")),
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            let mut c = self.inner();
            if matches!(c, b'+' | b'-') {
                c = self.inner();
            }
            if !c.is_ascii_digit() {
                return Err(fail(c, "in exponent of numeric literal"));
            }
            self.digits();
        }
        Ok(Value::Number(go::span(self.b, start, self.at).to_vec()))
    }

    fn digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
    }

    /// A string, decoded as Go's `unquoteBytes` decodes it.
    fn string(&mut self) -> Result<Vec<u8>, Vec<u8>> {
        self.at += 1;
        let mut out = Vec::new();
        loop {
            let c = self.peek().ok_or(END)?;
            match c {
                b'"' => {
                    self.at += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.at += 1;
                    let e = self.inner();
                    let simple = match e {
                        b'"' => b'"',
                        b'\\' => b'\\',
                        b'/' => b'/',
                        b'b' => 0x08,
                        b'f' => 0x0c,
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'u' => {
                            let r = self.hex4()?;
                            if (0xD800..0xDC00).contains(&r) {
                                // A pair, if a low surrogate's escape follows.
                                let low = go::tail(self.b, self.at)
                                    .strip_prefix(b"\\u")
                                    .and_then(hex4)
                                    .filter(|l| (0xDC00..0xE000).contains(l));
                                match low {
                                    Some(l) => {
                                        self.at += 6;
                                        go::push(&mut out, 0x10000 + ((r - 0xD800) << 10) + (l - 0xDC00));
                                    }
                                    None => go::push(&mut out, go::RUNE_ERROR),
                                }
                            } else if (0xDC00..0xE000).contains(&r) {
                                go::push(&mut out, go::RUNE_ERROR);
                            } else {
                                go::push(&mut out, r);
                            }
                            continue;
                        }
                        _ => return Err(fail(e, "in string escape code")),
                    };
                    out.push(simple);
                }
                0..=0x1f => return Err(fail(c, "in string literal")),
                0x80.. => {
                    let (r, w) = go::decode(go::tail(self.b, self.at));
                    go::push(&mut out, r);
                    self.at += w;
                }
                // A run of bytes that stand for themselves, taken whole: what Go's scanner
                // steps through one at a time, read at once.
                _ => {
                    let rest = go::tail(self.b, self.at);
                    let run = rest
                        .iter()
                        .position(|&b| b == b'"' || b == b'\\' || !(0x20..0x80).contains(&b))
                        .unwrap_or(rest.len());
                    out.extend_from_slice(rest.get(..run).unwrap_or_default());
                    self.at += run;
                }
            }
        }
    }

    /// The four hex digits of a `\u` escape.
    fn hex4(&mut self) -> Result<u32, Vec<u8>> {
        let mut r = 0;
        for _ in 0..4 {
            let c = self.inner();
            let d = char::from(c)
                .to_digit(16)
                .ok_or_else(|| fail(c, "in \\u hexadecimal character escape"))?;
            r = r * 16 + d;
        }
        Ok(r)
    }
}

/// Four hex digits at the start of `b`, as a number.
fn hex4(b: &[u8]) -> Option<u32> {
    b.get(..4)?
        .iter()
        .try_fold(0u32, |r, &d| Some(r * 16 + char::from(d).to_digit(16)?))
}

/// Appends `s` as a JSON string, as `json.Marshal` writes it: `<`, `>` and `&` escaped for
/// HTML, U+2028 and U+2029 escaped, an invalid byte as `\ufffd`.
pub fn write_string(out: &mut String, s: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push('"');
    let mut at = 0;
    while at < s.len() {
        let (r, w) = go::decode(go::tail(s, at));
        at += w.max(1);
        match r {
            0x22 => out.push_str("\\\""),
            0x5c => out.push_str("\\\\"),
            0x08 => out.push_str("\\b"),
            0x0c => out.push_str("\\f"),
            0x0a => out.push_str("\\n"),
            0x0d => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            0..=0x1f | 0x3c | 0x3e | 0x26 => {
                out.push_str("\\u00");
                for nibble in [r >> 4, r & 0xf] {
                    out.push(char::from(HEX.get(nibble as usize).copied().unwrap_or(b'0')));
                }
            }
            0x2028 | 0x2029 => {
                let _ = write!(out, "\\u{r:04x}");
            }
            // An invalid byte decodes to U+FFFD one byte wide, written as Go writes it.
            go::RUNE_ERROR if w == 1 => out.push_str("\\ufffd"),
            _ => out.push(char::from_u32(r).unwrap_or('\u{FFFD}')),
        }
    }
    out.push('"');
}

/// Appends `items` as a JSON array of strings.
pub fn write_strings(out: &mut String, items: &[Vec<u8>]) {
    out.push('[');
    for (i, s) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_string(out, s);
    }
    out.push(']');
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Nesting is Go's to its limit, and scanned without recursion.
    #[test]
    fn nesting_is_scanned_without_recursion() {
        let deep = |n: usize| [b"[".repeat(n), b"]".repeat(n)].concat();
        assert!(matches!(array(&deep(MAX_DEPTH)), Array::NotStrings));
        assert!(matches!(array(&deep(MAX_DEPTH + 1)), Array::Not));
        assert!(matches!(array(&deep(1)), Array::Strings(v) if v.is_empty()));
        assert!(matches!(array(b" [\"a\", \"b\"] "), Array::Strings(v) if v.len() == 2));
        assert!(matches!(array(b"[\"a\", {\"k\": [1]}]"), Array::NotStrings));
        assert!(matches!(array(b"[\"a\" \"b\"]"), Array::Not));
    }

    /// A number past a float64's range, anywhere in it, makes an array no JSON one, as
    /// `json.Unmarshal` into `[]any` fails on it (measured, go1.27.1); one too small is 0.
    #[test]
    fn a_number_past_a_float64_is_no_json() {
        for text in [
            &br#"["a", 1e400]"#[..],
            br#"["a", [1e309]]"#,
            br#"["a", {"k": -1e400}]"#,
            br#"["a", 1.7976931348623159e308]"#,
        ] {
            assert!(
                matches!(array(text), Array::Not),
                "{}",
                String::from_utf8_lossy(text)
            );
        }
        for text in [
            &br#"["a", 1e-400]"#[..],
            br#"["a", 1.7976931348623157e308]"#,
            br#"["a", 2e-324]"#,
        ] {
            assert!(
                matches!(array(text), Array::NotStrings),
                "{}",
                String::from_utf8_lossy(text)
            );
        }
    }

    /// A value as deep as Go allows drops on a thread of little stack: its drop is no
    /// deeper than its parse.
    #[test]
    fn the_deepest_value_drops_on_a_small_stack() {
        let dropped = std::thread::Builder::new()
            .stack_size(64 << 10)
            .spawn(|| {
                for (open, close) in [(b'[', b']'), (b'{', b'}')] {
                    let text = if open == b'[' {
                        [vec![open; MAX_DEPTH], vec![close; MAX_DEPTH]].concat()
                    } else {
                        [
                            br#"{"k":"#.repeat(MAX_DEPTH - 1),
                            b"{}".to_vec(),
                            b"}".repeat(MAX_DEPTH - 1),
                        ]
                        .concat()
                    };
                    drop(parse(&text).unwrap());
                }
            })
            .unwrap()
            .join();
        assert!(dropped.is_ok());
    }

    /// Values parse whole, members in order with duplicates kept, and nothing malformed.
    /// json.Compact's (Go 1.24): whitespace between tokens goes, a string's stays, and what
    /// is no JSON is refused in the scanner's words.
    #[test]
    fn json_compacts_as_go_compacts_it() {
        assert_eq!(
            compact(b" { \"a\" : [ 1 , \" b \\\" c \" ] }\n").unwrap(),
            br#"{"a":[1," b \" c "]}"#
        );
        assert_eq!(
            compact(b"{\"defaultAction\": ").unwrap_err(),
            "unexpected end of JSON input"
        );
        assert_eq!(
            compact(b"{} x").unwrap_err(),
            "invalid character 'x' after top-level value"
        );
    }

    #[test]
    fn values_parse_as_go_scans_them() {
        let v = parse(br#" {"a": [1, -2.5e3, true, null], "b": {}, "a": "\u00e9\ud800x"} "#).unwrap();
        assert_eq!(
            v,
            Value::Object(vec![
                (
                    b"a".to_vec(),
                    Value::Array(vec![
                        Value::Number(b"1".to_vec()),
                        Value::Number(b"-2.5e3".to_vec()),
                        Value::Bool(true),
                        Value::Null,
                    ])
                ),
                (b"b".to_vec(), Value::Object(vec![])),
                (
                    b"a".to_vec(),
                    Value::String("é\u{FFFD}x".as_bytes().to_vec(), Some(br"\u00e9\ud800x".to_vec()))
                ),
            ])
        );
        // Go's own messages; tests/oracle.rs holds the rest, as image configs.
        let cases: &[(&[u8], &[u8])] = &[
            (&b"{\"a\" 1}"[..], &b"invalid character '1' after object key"[..]),
            (
                &b"{\"a\":1,}"[..],
                &b"invalid character '}' looking for beginning of object key string"[..],
            ),
            (
                &b"[1,]"[..],
                &b"invalid character ']' looking for beginning of value"[..],
            ),
            (&b"[01]"[..], &b"invalid character '1' after array element"[..]),
            (
                &b"{1:2}"[..],
                &b"invalid character '1' looking for beginning of object key string"[..],
            ),
            (
                &b"\"a\x0ab\""[..],
                &b"invalid character '\\n' in string literal"[..],
            ),
        ];
        for (bad, want) in cases {
            assert_eq!(
                String::from_utf8_lossy(&parse(bad).unwrap_err()),
                String::from_utf8_lossy(want),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
        let deep = [b"[".repeat(MAX_DEPTH + 1), b"]".repeat(MAX_DEPTH + 1)].concat();
        assert_eq!(
            parse(&deep).unwrap_err(),
            b"invalid character '[' exceeded max depth"
        );
        assert_eq!(parse(b"\"x\""), Ok(Value::String(b"x".to_vec(), None)));
        // UTF-8 kept as it is; each byte that begins no rune Go's utf8.DecodeRune reads
        // made U+FFFD, a cut sequence one for each of its bytes, the text kept as written.
        assert_eq!(
            parse("\"é\"".as_bytes()),
            Ok(Value::String("é".as_bytes().to_vec(), None))
        );
        assert_eq!(
            parse(b"\"a\xffb\xf0\x9f\x98\""),
            Ok(Value::String(
                "a\u{FFFD}b\u{FFFD}\u{FFFD}\u{FFFD}".as_bytes().to_vec(),
                Some(b"a\xffb\xf0\x9f\x98".to_vec())
            ))
        );
    }

    /// Strings are written as `json.Marshal` writes them.
    #[test]
    fn strings_are_written_as_go_writes_them() {
        let mut out = String::new();
        write_string(&mut out, b"a\"\\<>&\x01\x7f\n\t\xff\xc3\xa9\xe2\x80\xa8");
        assert_eq!(
            out,
            "\"a\\\"\\\\\\u003c\\u003e\\u0026\\u0001\x7f\\n\\t\\ufffdé\\u2028\""
        );
    }
}
