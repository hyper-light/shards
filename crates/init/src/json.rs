//! JSON as RFC 8259 reads it, for what init reads of an image: its normalized Agentfile
//! (`/.agentfile.json`) and each domain's OSI config (`<dir>.d/osi.json`). Small and
//! bounded, as PID 1's reader of files an image's builder wrote: a text past `MAX_LEN`
//! bytes or nested past `MAX_DEPTH` is refused, and nothing is read but the text given.

/// The longest text read: an Agentfile's spec and a config are a few kilobytes.
pub const MAX_LEN: usize = 1 << 20;
/// The deepest nesting read.
pub const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// A number as written, read as an integer where that is asked.
    Number(String),
    String(String),
    Array(Vec<Value>),
    /// Members in the order written; a key written twice is refused.
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn array(&self) -> &[Value] {
        match self {
            Value::Array(a) => a,
            _ => &[],
        }
    }

    pub fn u64(&self) -> Option<u64> {
        match self {
            Value::Number(n) => n.parse().ok(),
            _ => None,
        }
    }

    /// An array of strings, or nothing where it is not one.
    pub fn strings(&self) -> Option<Vec<String>> {
        self.array().iter().map(|v| v.str().map(str::to_string)).collect()
    }
}

/// Parses `text`, whole: one value, whitespace around it.
pub fn parse(text: &[u8]) -> Result<Value, String> {
    if text.len() > MAX_LEN {
        return Err(format!("{} bytes of JSON, more than {MAX_LEN}", text.len()));
    }
    let mut p = Parser { text, at: 0 };
    p.space();
    let v = p.value(0)?;
    p.space();
    if p.at != text.len() {
        return Err(format!("text after the value, at byte {}", p.at));
    }
    Ok(v)
}

struct Parser<'a> {
    text: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn expect(&mut self, word: &[u8]) -> Result<(), String> {
        if self.text.get(self.at..self.at + word.len()) == Some(word) {
            self.at += word.len();
            Ok(())
        } else {
            Err(format!(
                "expected {} at byte {}",
                String::from_utf8_lossy(word),
                self.at
            ))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err(format!("nested deeper than {MAX_DEPTH}"));
        }
        match self.peek() {
            Some(b'n') => self.expect(b"null").map(|()| Value::Null),
            Some(b't') => self.expect(b"true").map(|()| Value::Bool(true)),
            Some(b'f') => self.expect(b"false").map(|()| Value::Bool(false)),
            Some(b'"') => self.string().map(Value::String),
            Some(b'[') => {
                self.at += 1;
                let mut out = Vec::new();
                self.space();
                if self.peek() == Some(b']') {
                    self.at += 1;
                    return Ok(Value::Array(out));
                }
                loop {
                    self.space();
                    out.push(self.value(depth + 1)?);
                    self.space();
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            return Ok(Value::Array(out));
                        }
                        _ => return Err(format!("expected , or ] at byte {}", self.at)),
                    }
                }
            }
            Some(b'{') => {
                self.at += 1;
                let mut out: Vec<(String, Value)> = Vec::new();
                self.space();
                if self.peek() == Some(b'}') {
                    self.at += 1;
                    return Ok(Value::Object(out));
                }
                loop {
                    self.space();
                    let key = self.string()?;
                    if out.iter().any(|(k, _)| *k == key) {
                        return Err(format!("the key {key:?} written twice"));
                    }
                    self.space();
                    self.expect(b":")?;
                    self.space();
                    let v = self.value(depth + 1)?;
                    out.push((key, v));
                    self.space();
                    match self.peek() {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {
                            self.at += 1;
                            return Ok(Value::Object(out));
                        }
                        _ => return Err(format!("expected , or }} at byte {}", self.at)),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => {
                let start = self.at;
                if self.peek() == Some(b'-') {
                    self.at += 1;
                }
                let digits = |p: &mut Self| {
                    let s = p.at;
                    while matches!(p.peek(), Some(b'0'..=b'9')) {
                        p.at += 1;
                    }
                    p.at > s
                };
                let int = self.at;
                if !digits(self) {
                    return Err(format!("a number without digits at byte {start}"));
                }
                // RFC 8259 §6: no leading zero.
                if self.text.get(int) == Some(&b'0') && self.at - int > 1 {
                    return Err(format!("a number with a leading zero at byte {start}"));
                }
                if self.peek() == Some(b'.') {
                    self.at += 1;
                    if !digits(self) {
                        return Err(format!("a fraction without digits at byte {start}"));
                    }
                }
                if matches!(self.peek(), Some(b'e' | b'E')) {
                    self.at += 1;
                    if matches!(self.peek(), Some(b'+' | b'-')) {
                        self.at += 1;
                    }
                    if !digits(self) {
                        return Err(format!("an exponent without digits at byte {start}"));
                    }
                }
                let n = self.text.get(start..self.at).unwrap_or_default();
                Ok(Value::Number(String::from_utf8_lossy(n).into_owned()))
            }
            _ => Err(format!("no JSON value at byte {}", self.at)),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self
            .text
            .get(self.at..self.at + 4)
            .ok_or("a \\u escape cut short")?;
        let s = std::str::from_utf8(h).map_err(|_| "a \\u escape that is no hex".to_string())?;
        let v = u32::from_str_radix(s, 16).map_err(|_| "a \\u escape that is no hex".to_string())?;
        self.at += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, String> {
        if self.peek() != Some(b'"') {
            return Err(format!("expected a string at byte {}", self.at));
        }
        self.at += 1;
        let mut out = Vec::new();
        loop {
            let Some(b) = self.peek() else {
                return Err("a string never closed".into());
            };
            self.at += 1;
            match b {
                b'"' => break,
                b'\\' => {
                    let e = self.peek().ok_or("an escape cut short")?;
                    self.at += 1;
                    let c = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&hi) {
                                self.expect(b"\\u")?;
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return Err("a lone surrogate".into());
                                }
                                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                            } else {
                                hi
                            };
                            char::from_u32(code).ok_or("a \\u escape that names no character")?
                        }
                        other => return Err(format!("the escape \\{}", other as char)),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                0..=0x1f => return Err("a control character in a string".into()),
                _ => out.push(b),
            }
        }
        String::from_utf8(out).map_err(|_| "a string that is no UTF-8".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_rfc_8259_writes_and_no_more() {
        let v = parse(br#" {"name":"main","run":{"command":["bin/a","-x"],"env":[]},"n":-1.5e3,"t":true,"z":null,"u":"\u00e9\ud83d\ude00\n"} "#).unwrap();
        assert_eq!(v.get("name").and_then(Value::str), Some("main"));
        assert_eq!(
            v.get("run")
                .and_then(|r| r.get("command"))
                .and_then(Value::strings),
            Some(vec!["bin/a".to_string(), "-x".to_string()])
        );
        assert_eq!(v.get("u").and_then(Value::str), Some("é😀\n"));
        for bad in [
            &b"{\"a\":1,\"a\":2}"[..],
            b"[1,]",
            b"{\"a\" 1}",
            b"\"\\ud800\"",
            b"\"a\nb\"",
            b"01x",
            b"012",
            b"-01",
            b"[] []",
            b"\"never",
        ] {
            assert!(parse(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
        let deep = "[".repeat(MAX_DEPTH + 2) + &"]".repeat(MAX_DEPTH + 2);
        assert!(parse(deep.as_bytes()).is_err());
    }
}
