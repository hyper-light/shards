//! A JSON array of strings, as BuildKit reads one with Go's `encoding/json`
//! (`parser/line_parsers.go`, `parseJSON`): the text must be valid JSON as Go's scanner
//! checks it, nesting at most 10,000 deep, and strings decode as Go decodes them, an
//! invalid byte or a lone surrogate becoming U+FFFD. The scan is iterative, its stack on
//! the heap, so no nesting exhausts a thread's stack.

use crate::go;

/// What a JSON array of strings is, if `text` is one.
pub(crate) enum Array {
    /// Valid JSON, an array whose elements are all strings.
    Strings(Vec<Vec<u8>>),
    /// Valid JSON, an array with an element that is no string.
    NotStrings,
    /// Not valid JSON, or not an array.
    Not,
}

/// Go's scanner refuses JSON nested deeper than this (`encoding/json` scanner.go,
/// `maxNestingDepth`).
const MAX_DEPTH: usize = 10_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Container {
    Array,
    Object,
}

/// Where the scan is within the innermost container.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// A value, or `]` closing an array just opened.
    ValueOrEnd,
    /// A value.
    Value,
    /// `,` or the container's end.
    CommaOrEnd,
    /// A key (`"..."`), or `}` closing an object just opened.
    KeyOrEnd,
    /// A key.
    Key,
    /// `:` after a key.
    Colon,
}

pub(crate) fn array(text: &[u8]) -> Array {
    let mut s = Scan { b: text, at: 0 };
    s.space();
    if s.peek() != Some(b'[') {
        return Array::Not;
    }
    let mut stack: Vec<Container> = Vec::new();
    let mut expect = Expect::Value;
    // The top-level array's elements: each a string, or not.
    let mut strings = Vec::new();
    let mut all_strings = true;
    loop {
        s.space();
        let Some(c) = s.peek() else {
            return Array::Not;
        };
        let top = stack.last().copied();
        match expect {
            Expect::Value | Expect::ValueOrEnd => {
                if expect == Expect::ValueOrEnd && c == b']' {
                    s.at += 1;
                    stack.pop();
                    expect = Expect::CommaOrEnd;
                } else {
                    let at_top = stack.len() == 1;
                    match c {
                        b'[' | b'{' => {
                            if stack.len() >= MAX_DEPTH {
                                return Array::Not;
                            }
                            s.at += 1;
                            if at_top {
                                all_strings = false;
                            }
                            if c == b'[' {
                                stack.push(Container::Array);
                                expect = Expect::ValueOrEnd;
                            } else {
                                stack.push(Container::Object);
                                expect = Expect::KeyOrEnd;
                            }
                            continue;
                        }
                        b'"' => {
                            let Some(v) = s.string() else {
                                return Array::Not;
                            };
                            if at_top {
                                strings.push(v);
                            }
                        }
                        _ => {
                            if !s.scalar() {
                                return Array::Not;
                            }
                            if at_top {
                                all_strings = false;
                            }
                        }
                    }
                    expect = Expect::CommaOrEnd;
                }
            }
            Expect::CommaOrEnd => match (c, top) {
                (b',', Some(Container::Array)) => {
                    s.at += 1;
                    expect = Expect::Value;
                }
                (b',', Some(Container::Object)) => {
                    s.at += 1;
                    expect = Expect::Key;
                }
                (b']', Some(Container::Array)) | (b'}', Some(Container::Object)) => {
                    s.at += 1;
                    stack.pop();
                }
                _ => return Array::Not,
            },
            Expect::Key | Expect::KeyOrEnd => {
                if expect == Expect::KeyOrEnd && c == b'}' {
                    s.at += 1;
                    stack.pop();
                    expect = Expect::CommaOrEnd;
                } else if c == b'"' && s.string().is_some() {
                    expect = Expect::Colon;
                } else {
                    return Array::Not;
                }
            }
            Expect::Colon => {
                if c != b':' {
                    return Array::Not;
                }
                s.at += 1;
                expect = Expect::Value;
            }
        }
        if stack.is_empty() {
            break;
        }
    }
    s.space();
    if s.at != text.len() {
        return Array::Not;
    }
    if all_strings {
        Array::Strings(strings)
    } else {
        Array::NotStrings
    }
}

struct Scan<'a> {
    b: &'a [u8],
    at: usize,
}

impl Scan<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.at).copied()
    }

    /// JSON's whitespace, the only kind Go's scanner skips.
    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    /// `true`, `false`, `null` or a number; whether it was one.
    fn scalar(&mut self) -> bool {
        for word in [b"true".as_slice(), b"false", b"null"] {
            if go::tail(self.b, self.at).starts_with(word) {
                self.at += word.len();
                return true;
            }
        }
        self.number()
    }

    /// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`
    fn number(&mut self) -> bool {
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek() {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => self.digits(),
            _ => return false,
        }
        if self.peek() == Some(b'.') {
            self.at += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return false;
            }
            self.digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return false;
            }
            self.digits();
        }
        true
    }

    fn digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
    }

    /// A string, decoded as Go's `unquoteBytes` decodes it.
    fn string(&mut self) -> Option<Vec<u8>> {
        self.at += 1;
        let mut out = Vec::new();
        loop {
            let c = self.peek()?;
            match c {
                b'"' => {
                    self.at += 1;
                    return Some(out);
                }
                b'\\' => {
                    self.at += 1;
                    let simple = match self.peek()? {
                        b'"' => Some(b'"'),
                        b'\\' => Some(b'\\'),
                        b'/' => Some(b'/'),
                        b'b' => Some(0x08),
                        b'f' => Some(0x0c),
                        b'n' => Some(b'\n'),
                        b'r' => Some(b'\r'),
                        b't' => Some(b'\t'),
                        b'u' => None,
                        _ => return None,
                    };
                    if let Some(byte) = simple {
                        out.push(byte);
                        self.at += 1;
                        continue;
                    }
                    self.at += 1;
                    let r = hex4(go::tail(self.b, self.at))?;
                    self.at += 4;
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
                }
                0..=0x1f => return None,
                0x80.. => {
                    let (r, w) = go::decode(go::tail(self.b, self.at));
                    go::push(&mut out, r);
                    self.at += w;
                }
                _ => {
                    out.push(c);
                    self.at += 1;
                }
            }
        }
    }
}

/// Four hex digits at the start of `b`, as a number.
fn hex4(b: &[u8]) -> Option<u32> {
    b.get(..4)?
        .iter()
        .try_fold(0u32, |r, &d| Some(r * 16 + char::from(d).to_digit(16)?))
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
}
