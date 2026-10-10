//! JSON as Go 1.26's encoding/json reads it for Rekor's entry bodies: a `json.Decoder`'s
//! first value (go-openapi's JSONConsumer) or `json.Unmarshal`'s whole document, each
//! syntax error in the scanner's words; values kept as Go's `any` holds them with
//! `UseNumber` (numbers as written, an object's members in order, invalid UTF-8 in
//! strings replaced), and struct fields matched as the decoder matches them.

/// A JSON value. As deep as Go reads one, it is made, copied and let go without
/// recursion, on any thread's stack.
#[derive(Debug, PartialEq, Eq)]
pub enum JValue {
    Null,
    Bool(bool),
    Number(String),
    Str(String),
    Array(Vec<JValue>),
    Object(Vec<(String, JValue)>),
}

impl JValue {
    /// The values directly inside it, moved out onto `out`.
    fn take_inner(&mut self, out: &mut Vec<JValue>) {
        match self {
            JValue::Array(items) => out.append(items),
            JValue::Object(members) => out.extend(members.drain(..).map(|(_, v)| v)),
            _ => {}
        }
    }

    /// A copy of a value with nothing inside it; none for a container.
    fn copy_scalar(&self) -> Option<JValue> {
        Some(match self {
            JValue::Null => JValue::Null,
            JValue::Bool(b) => JValue::Bool(*b),
            JValue::Number(n) => JValue::Number(n.clone()),
            JValue::Str(s) => JValue::Str(s.clone()),
            JValue::Array(_) | JValue::Object(_) => return None,
        })
    }
}

impl Drop for JValue {
    /// The values inside it let go one at a time, each emptied first.
    fn drop(&mut self) {
        let mut inner = Vec::new();
        self.take_inner(&mut inner);
        while let Some(mut v) = inner.pop() {
            v.take_inner(&mut inner);
        }
    }
}

/// A container being copied: what is left of it to copy, and its copy so far.
enum Copying<'v> {
    Array(std::slice::Iter<'v, JValue>, Vec<JValue>),
    Object(
        std::slice::Iter<'v, (String, JValue)>,
        Vec<(String, JValue)>,
        String,
    ),
}

impl Clone for JValue {
    /// Copied container by container, those being copied held on a stack of the heap's.
    fn clone(&self) -> JValue {
        let mut open: Vec<Copying<'_>> = Vec::new();
        let mut next = self;
        loop {
            let mut done = match next {
                JValue::Array(items) => {
                    open.push(Copying::Array(items.iter(), Vec::with_capacity(items.len())));
                    None
                }
                JValue::Object(members) => {
                    open.push(Copying::Object(
                        members.iter(),
                        Vec::with_capacity(members.len()),
                        String::new(),
                    ));
                    None
                }
                scalar => scalar.copy_scalar(),
            };
            // Each copy done goes into the container being copied; the next value in that
            // is copied next, or the container is done.
            loop {
                if let Some(v) = done.take() {
                    match open.last_mut() {
                        None => return v,
                        Some(Copying::Array(_, out)) => out.push(v),
                        Some(Copying::Object(_, out, key)) => out.push((std::mem::take(key), v)),
                    }
                }
                let advanced = match open.last_mut() {
                    None => return JValue::Null,
                    Some(Copying::Array(rest, _)) => rest.next(),
                    Some(Copying::Object(rest, _, key)) => rest.next().map(|(k, v)| {
                        key.clone_from(k);
                        v
                    }),
                };
                if let Some(v) = advanced {
                    next = v;
                    break;
                }
                done = match open.pop() {
                    Some(Copying::Array(_, out)) => Some(JValue::Array(out)),
                    Some(Copying::Object(_, out, _)) => Some(JValue::Object(out)),
                    None => return JValue::Null,
                };
            }
        }
    }
}

impl JValue {
    /// Go's name for the value in an UnmarshalTypeError.
    pub fn kind(&self) -> &'static str {
        match self {
            JValue::Null => "null",
            JValue::Bool(_) => "bool",
            JValue::Number(_) => "number",
            JValue::Str(_) => "string",
            JValue::Array(_) => "array",
            JValue::Object(_) => "object",
        }
    }

    /// The member `name` as a `map[string]any` keeps it: the last of that name.
    pub fn get(&self, name: &str) -> Option<&JValue> {
        match self {
            JValue::Object(m) => m.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            JValue::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Where a parse stopped short.
enum Fail {
    Syntax(String),
    /// The input ended inside a value.
    Eof,
}

/// How deep encoding/json's scanner nests containers (maxNestingDepth): the next is
/// refused.
const MAX_DEPTH: usize = 10_000;

/// A container being read: its values so far, an object's key for the value next, and
/// where its span is.
enum Open {
    Array(Vec<JValue>, Option<usize>),
    Object(Vec<(String, JValue)>, String, Option<usize>),
}

struct Parser<'a> {
    b: &'a [u8],
    at: usize,
    /// Where each value lies, in the order the values begin, when asked for.
    spans: Option<Vec<(usize, usize)>>,
}

/// strconv.Quote of a byte taken as a rune, its quotes dropped (Go's quoteChar).
pub fn quote_char(c: u8) -> String {
    match c {
        b'\'' => "'\\''".into(),
        b'"' => "'\"'".into(),
        _ => {
            let q = go_quote_rune(char::from(c));
            format!("'{q}'")
        }
    }
}

/// strconv.Quote of a single rune, without its quotes.
fn go_quote_rune(c: char) -> String {
    match c {
        '\u{7}' => "\\a".into(),
        '\u{8}' => "\\b".into(),
        '\u{c}' => "\\f".into(),
        '\n' => "\\n".into(),
        '\r' => "\\r".into(),
        '\t' => "\\t".into(),
        '\u{b}' => "\\v".into(),
        '\\' => "\\\\".into(),
        '"' => "\\\"".into(),
        c if (c as u32) < 0x20 || c as u32 == 0x7f => format!("\\x{:02x}", c as u32),
        c if (0x80..0xa1).contains(&(c as u32)) || c as u32 == 0xad => format!("\\u{:04x}", c as u32),
        c => c.to_string(),
    }
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

    fn unexpected(&self, context: &str) -> Fail {
        match self.peek() {
            None => Fail::Eof,
            Some(c) => Fail::Syntax(format!("invalid character {} {context}", quote_char(c))),
        }
    }

    /// A value, as the scanner reads one: the containers it is inside held on a stack of
    /// the heap's, so no depth Go reads (its 10,000) needs a deeper stack of the thread's.
    fn value(&mut self) -> Result<JValue, Fail> {
        let mut open: Vec<Open> = Vec::new();
        'value: loop {
            self.ws();
            let span = self.spans.as_mut().map(|s| {
                s.push((self.at, self.at));
                s.len() - 1
            });
            let mut v = match self.peek() {
                Some(c @ (b'{' | b'[')) => {
                    if open.len() >= MAX_DEPTH {
                        return Err(Fail::Syntax(format!(
                            "invalid character {} exceeded max depth",
                            quote_char(c)
                        )));
                    }
                    self.at += 1;
                    self.ws();
                    if c == b'{' {
                        if self.peek() == Some(b'}') {
                            self.at += 1;
                            JValue::Object(Vec::new())
                        } else {
                            let key = self.key()?;
                            open.push(Open::Object(Vec::new(), key, span));
                            continue 'value;
                        }
                    } else if self.peek() == Some(b']') {
                        self.at += 1;
                        JValue::Array(Vec::new())
                    } else {
                        open.push(Open::Array(Vec::new(), span));
                        continue 'value;
                    }
                }
                Some(b'"') => self.string().map(JValue::Str)?,
                Some(b't') => self.literal(b"true", JValue::Bool(true))?,
                Some(b'f') => self.literal(b"false", JValue::Bool(false))?,
                Some(b'n') => self.literal(b"null", JValue::Null)?,
                Some(b'-' | b'0'..=b'9') => self.number()?,
                _ => return Err(self.unexpected("looking for beginning of value")),
            };
            self.close(span);
            // The value done: into the container it is in, and each container it ends.
            loop {
                match open.last_mut() {
                    None => return Ok(v),
                    Some(Open::Array(items, _)) => {
                        items.push(v);
                        self.ws();
                        match self.peek() {
                            Some(b',') => {
                                self.at += 1;
                                continue 'value;
                            }
                            Some(b']') => self.at += 1,
                            _ => return Err(self.unexpected("after array element")),
                        }
                    }
                    Some(Open::Object(members, key, _)) => {
                        members.push((std::mem::take(key), v));
                        self.ws();
                        match self.peek() {
                            Some(b',') => {
                                self.at += 1;
                                *key = self.key()?;
                                continue 'value;
                            }
                            Some(b'}') => self.at += 1,
                            _ => return Err(self.unexpected("after object key:value pair")),
                        }
                    }
                }
                // The container ended (the one just read: there is one).
                v = match open.pop() {
                    Some(Open::Array(items, span)) => {
                        self.close(span);
                        JValue::Array(items)
                    }
                    Some(Open::Object(members, _, span)) => {
                        self.close(span);
                        JValue::Object(members)
                    }
                    None => JValue::Null,
                };
            }
        }
    }

    /// An object's key: the string, then its `:`.
    fn key(&mut self) -> Result<String, Fail> {
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
        Ok(key)
    }

    /// The span begun at `span` ends here.
    fn close(&mut self, span: Option<usize>) {
        let end = self.at;
        if let Some(s) = span.and_then(|i| self.spans.as_mut()?.get_mut(i)) {
            s.1 = end;
        }
    }

    fn literal(&mut self, word: &[u8], v: JValue) -> Result<JValue, Fail> {
        for (i, &c) in word.iter().enumerate() {
            match self.b.get(self.at + i) {
                Some(&got) if got == c => {}
                None => return Err(Fail::Eof),
                Some(&got) => {
                    return Err(Fail::Syntax(format!(
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

    fn number(&mut self) -> Result<JValue, Fail> {
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
        Ok(JValue::Number(String::from_utf8_lossy(text).into_owned()))
    }

    /// A string, its escapes decoded and invalid UTF-8 replaced (encoding/json unquote).
    fn string(&mut self) -> Result<String, Fail> {
        self.at += 1;
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(Fail::Eof);
            };
            match c {
                b'"' => {
                    self.at += 1;
                    return Ok(String::from_utf8_lossy(&out).into_owned());
                }
                b'\\' => {
                    self.at += 1;
                    let Some(e) = self.peek() else {
                        return Err(Fail::Eof);
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
                            return Err(Fail::Syntax(format!(
                                "invalid character {} in string escape code",
                                quote_char(other)
                            )));
                        }
                    }
                }
                c if c < 0x20 => {
                    return Err(Fail::Syntax(format!(
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

    fn hex4(&mut self) -> Result<u32, Fail> {
        let mut v = 0u32;
        for _ in 0..4 {
            let Some(c) = self.peek() else {
                return Err(Fail::Eof);
            };
            let d = char::from(c).to_digit(16).ok_or_else(|| {
                Fail::Syntax(format!(
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

/// json.Unmarshal's reading of a document: one value, then only white space.
pub fn unmarshal(b: &[u8]) -> Result<JValue, String> {
    let mut p = Parser {
        b,
        at: 0,
        spans: None,
    };
    let v = match p.value() {
        Ok(v) => v,
        Err(Fail::Syntax(s)) => return Err(s),
        Err(Fail::Eof) => return Err("unexpected end of JSON input".into()),
    };
    p.ws();
    if p.at != b.len() {
        return match p.unexpected("after top-level value") {
            Fail::Syntax(s) => Err(s),
            Fail::Eof => Err("unexpected end of JSON input".into()),
        };
    }
    Ok(v)
}

/// [`unmarshal`], and the bytes of each value (a json.RawMessage's) in the order the
/// values begin: the order [`pre_order`] walks them in.
pub fn unmarshal_raw(b: &[u8]) -> Result<(JValue, Vec<(usize, usize)>), String> {
    let mut p = Parser {
        b,
        at: 0,
        spans: Some(Vec::new()),
    };
    let v = match p.value() {
        Ok(v) => v,
        Err(Fail::Syntax(s)) => return Err(s),
        Err(Fail::Eof) => return Err("unexpected end of JSON input".into()),
    };
    p.ws();
    if p.at != b.len() {
        return match p.unexpected("after top-level value") {
            Fail::Syntax(s) => Err(s),
            Fail::Eof => Err("unexpected end of JSON input".into()),
        };
    }
    Ok((v, p.spans.unwrap_or_default()))
}

/// `v` and the values inside it, each before those inside it, members and elements in
/// document order.
pub fn pre_order<'v>(v: &'v JValue, out: &mut Vec<&'v JValue>) {
    let mut next = vec![v];
    while let Some(v) = next.pop() {
        out.push(v);
        match v {
            JValue::Array(items) => next.extend(items.iter().rev()),
            JValue::Object(members) => next.extend(members.iter().rev().map(|(_, x)| x)),
            _ => {}
        }
    }
}

/// json.Decoder.Decode's reading: the first value, whatever follows it; `EOF` where
/// there is none and `unexpected EOF` where it is cut short.
pub fn decode_first(b: &[u8]) -> Result<JValue, String> {
    let mut p = Parser {
        b,
        at: 0,
        spans: None,
    };
    p.ws();
    if p.at == b.len() {
        return Err("EOF".into());
    }
    match p.value() {
        Ok(v) => Ok(v),
        Err(Fail::Syntax(s)) => Err(s),
        Err(Fail::Eof) => Err("unexpected EOF".into()),
    }
}

/// [`decode_first`], and the bytes of each value in the order the values begin (as
/// [`unmarshal_raw`]): what an Unmarshaler is given.
pub fn decode_first_raw(b: &[u8]) -> Result<(JValue, Vec<(usize, usize)>), String> {
    let mut p = Parser {
        b,
        at: 0,
        spans: Some(Vec::new()),
    };
    p.ws();
    if p.at == b.len() {
        return Err("EOF".into());
    }
    match p.value() {
        Ok(v) => Ok((v, p.spans.unwrap_or_default())),
        Err(Fail::Syntax(s)) => Err(s),
        Err(Fail::Eof) => Err("unexpected EOF".into()),
    }
}

/// foldName: ASCII letters upper-cased, and the two runes whose case folding reaches
/// ASCII (U+017F to S, U+212A to K).
fn fold(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            'a'..='z' => c.to_ascii_uppercase(),
            '\u{17f}' => 'S',
            '\u{212a}' => 'K',
            c => c,
        })
        .collect()
}

/// The members of `obj` that set each of `fields` (a struct's JSON names), in document
/// order: a member matches a field by its exact name, else by its folded one.
pub fn struct_members<'v>(obj: &'v [(String, JValue)], fields: &[&str]) -> Vec<(usize, &'v JValue)> {
    let mut out = Vec::new();
    for (k, v) in obj {
        let exact = fields.iter().position(|f| f == k);
        let found = exact.or_else(|| {
            let fk = fold(k);
            fields.iter().position(|f| fold(f) == fk)
        });
        if let Some(i) = found {
            out.push((i, v));
        }
    }
    out
}

/// An UnmarshalTypeError's words.
pub fn type_error(value: &str, strukt: &str, field: &str, ty: &str) -> String {
    if strukt.is_empty() && field.is_empty() {
        format!("json: cannot unmarshal {value} into Go value of type {ty}")
    } else {
        format!("json: cannot unmarshal {value} into Go struct field {strukt}.{field} of type {ty}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_read_as_go_s() {
        assert_eq!(decode_first(b"  ").unwrap_err(), "EOF");
        assert_eq!(decode_first(b"{\"a\":").unwrap_err(), "unexpected EOF");
        assert_eq!(unmarshal(b"{\"a\":").unwrap_err(), "unexpected end of JSON input");
        assert_eq!(decode_first(b"12x").unwrap(), JValue::Number("12".into()));
        assert_eq!(
            unmarshal(b"12x").unwrap_err(),
            "invalid character 'x' after top-level value"
        );
        assert_eq!(
            unmarshal(b"\"a\nb\"").unwrap_err(),
            "invalid character '\\n' in string literal"
        );
        assert_eq!(quote_char(0xe9), "'é'");
        assert_eq!(quote_char(0x85), "'\\u0085'");
    }
}
