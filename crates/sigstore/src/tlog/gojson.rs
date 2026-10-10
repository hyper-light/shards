//! JSON as Go 1.26's encoding/json reads it for Rekor's entry bodies: a `json.Decoder`'s
//! first value (go-openapi's JSONConsumer) or `json.Unmarshal`'s whole document, each
//! syntax error in the scanner's words; values kept as Go's `any` holds them with
//! `UseNumber` (numbers as written, an object's members in order, invalid UTF-8 in
//! strings replaced), and struct fields matched as the decoder matches them.

/// A JSON value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JValue {
    Null,
    Bool(bool),
    Number(String),
    Str(String),
    Array(Vec<JValue>),
    Object(Vec<(String, JValue)>),
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

    fn value(&mut self, depth: usize) -> Result<JValue, Fail> {
        self.ws();
        let Some(spans) = self.spans.as_mut() else {
            return self.bare(depth);
        };
        let i = spans.len();
        spans.push((self.at, self.at));
        let v = self.bare(depth)?;
        let end = self.at;
        if let Some(s) = self.spans.as_mut().and_then(|s| s.get_mut(i)) {
            s.1 = end;
        }
        Ok(v)
    }

    /// A value, white space before it skipped.
    fn bare(&mut self, depth: usize) -> Result<JValue, Fail> {
        match self.peek() {
            Some(b'{') => {
                if depth >= 10_000 {
                    return Err(Fail::Syntax("invalid character '{' exceeded max depth".into()));
                }
                self.at += 1;
                let mut members = Vec::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.at += 1;
                    return Ok(JValue::Object(members));
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
                            return Ok(JValue::Object(members));
                        }
                        _ => return Err(self.unexpected("after object key:value pair")),
                    }
                }
            }
            Some(b'[') => {
                if depth >= 10_000 {
                    return Err(Fail::Syntax("invalid character '[' exceeded max depth".into()));
                }
                self.at += 1;
                let mut items = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.at += 1;
                    return Ok(JValue::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            return Ok(JValue::Array(items));
                        }
                        _ => return Err(self.unexpected("after array element")),
                    }
                }
            }
            Some(b'"') => self.string().map(JValue::Str),
            Some(b't') => self.literal(b"true", JValue::Bool(true)),
            Some(b'f') => self.literal(b"false", JValue::Bool(false)),
            Some(b'n') => self.literal(b"null", JValue::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.unexpected("looking for beginning of value")),
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
    let v = match p.value(0) {
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
    let v = match p.value(0) {
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
    out.push(v);
    match v {
        JValue::Array(items) => items.iter().for_each(|x| pre_order(x, out)),
        JValue::Object(members) => members.iter().for_each(|(_, x)| pre_order(x, out)),
        _ => {}
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
    match p.value(0) {
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
    match p.value(0) {
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
