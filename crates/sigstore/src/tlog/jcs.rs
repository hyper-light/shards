//! cyberphone's jsoncanonicalizer (RFC 8785, the Go reference implementation as rekor and
//! sigstore-go vendor it): Transform's parse, its members sorted by their UTF-16 code
//! units, strings re-escaped and numbers written as ES6 writes them (NumberToJSON), each
//! failure in its words.

struct Canon<'a> {
    b: &'a [u8],
    at: usize,
    err: Option<String>,
}

const ASCII_ESCAPES: [u8; 7] = *b"\\\"bfnrt";
const BINARY_ESCAPES: [u8; 7] = [b'\\', b'"', 8, 12, b'\n', b'\r', b'\t'];

fn white(c: u8) -> bool {
    matches!(c, 0x20 | 0x0a | 0x0d | 0x09)
}

/// Go's `string(byte)`: the byte as a rune, in UTF-8.
fn byte_rune(c: u8) -> String {
    char::from(c).to_string()
}

impl Canon<'_> {
    fn set_error(&mut self, msg: String) {
        if self.err.is_none() {
            self.err = Some(msg);
        }
    }

    fn next_char(&mut self) -> u8 {
        if let Some(&c) = self.b.get(self.at) {
            if c > 0x7f {
                self.set_error("Unexpected non-ASCII character".into());
            }
            self.at += 1;
            return c;
        }
        self.set_error("Unexpected EOF reached".into());
        b'"'
    }

    fn scan(&mut self) -> u8 {
        loop {
            let c = self.next_char();
            if !white(c) {
                return c;
            }
        }
    }

    fn scan_for(&mut self, expected: u8) {
        let c = self.scan();
        if c != expected {
            self.set_error(format!(
                "Expected '{}' but got '{}'",
                byte_rune(expected),
                byte_rune(c)
            ));
        }
    }

    fn u_escape(&mut self) -> u32 {
        let start = self.at;
        for _ in 0..4 {
            self.next_char();
        }
        if self.err.is_some() {
            return 0;
        }
        let text = self.b.get(start..self.at).unwrap_or_default();
        match u32::from_str_radix(&String::from_utf8_lossy(text), 16) {
            Ok(v) if text.iter().all(u8::is_ascii_hexdigit) => v,
            _ => {
                self.set_error(format!(
                    "strconv.ParseUint: parsing {}: invalid syntax",
                    shards_dockerfile::go::quote(text)
                ));
                0
            }
        }
    }

    fn peek_non_white(&mut self) -> u8 {
        let save = self.at;
        let c = self.scan();
        self.at = save;
        c
    }

    fn decorate(raw: &[u8]) -> Vec<u8> {
        let mut out = vec![b'"'];
        'core: for &c in raw {
            for (i, esc) in BINARY_ESCAPES.iter().enumerate() {
                if *esc == c {
                    out.push(b'\\');
                    out.push(ASCII_ESCAPES.get(i).copied().unwrap_or(c));
                    continue 'core;
                }
            }
            if c < 0x20 {
                out.extend_from_slice(format!("\\u{c:04x}").as_bytes());
            } else {
                out.push(c);
            }
        }
        out.push(b'"');
        out
    }

    fn quoted(&mut self) -> Vec<u8> {
        let mut raw: Vec<u8> = Vec::new();
        'core: while self.err.is_none() {
            let c = if let Some(&c) = self.b.get(self.at) {
                self.at += 1;
                c
            } else {
                self.next_char();
                break;
            };
            if c == b'"' {
                break;
            }
            if c < b' ' {
                self.set_error("Unterminated string literal".into());
            } else if c == b'\\' {
                let c = self.next_char();
                if c == b'u' {
                    let first = self.u_escape();
                    if (0xd800..0xe000).contains(&first) {
                        if self.next_char() != b'\\' || self.next_char() != b'u' {
                            self.set_error("Missing surrogate".into());
                        } else {
                            let second = self.u_escape();
                            let r = if (0xd800..0xdc00).contains(&first) && (0xdc00..0xe000).contains(&second)
                            {
                                0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
                            } else {
                                0xfffd
                            };
                            push_rune(&mut raw, r);
                        }
                    } else {
                        push_rune(&mut raw, first);
                    }
                } else if c == b'/' {
                    raw.push(b'/');
                } else {
                    for (i, esc) in ASCII_ESCAPES.iter().enumerate() {
                        if *esc == c {
                            raw.push(BINARY_ESCAPES.get(i).copied().unwrap_or(c));
                            continue 'core;
                        }
                    }
                    self.set_error(format!("Unexpected escape: \\{}", byte_rune(c)));
                }
            } else {
                raw.push(c);
            }
        }
        raw
    }

    fn simple(&mut self) -> Vec<u8> {
        let mut token: Vec<u8> = Vec::new();
        self.at = self.at.saturating_sub(1);
        while self.err.is_none() {
            let c = self.peek_non_white();
            if matches!(c, b',' | b']' | b'}') {
                break;
            }
            let c = self.next_char();
            if white(c) {
                break;
            }
            token.push(c);
        }
        if token.is_empty() {
            self.set_error("Missing argument".into());
        }
        if matches!(token.as_slice(), b"true" | b"false" | b"null") {
            return token;
        }
        let text = String::from_utf8_lossy(&token).into_owned();
        let f = match go_parse_float(&text) {
            Ok(f) => f,
            Err(e) => {
                self.set_error(e);
                0.0
            }
        };
        match number_to_json(f) {
            Ok(s) => s.into_bytes(),
            Err(e) => {
                self.set_error(e);
                b"null".to_vec()
            }
        }
    }

    fn element(&mut self) -> Vec<u8> {
        match self.scan() {
            b'{' => self.object(),
            b'"' => {
                let raw = self.quoted();
                Self::decorate(&raw)
            }
            b'[' => self.array(),
            _ => self.simple(),
        }
    }

    fn array(&mut self) -> Vec<u8> {
        let mut out = vec![b'['];
        let mut next = false;
        while self.err.is_none() && self.peek_non_white() != b']' {
            if next {
                self.scan_for(b',');
                out.push(b',');
            } else {
                next = true;
            }
            let e = self.element();
            out.extend(e);
        }
        self.scan();
        out.push(b']');
        out
    }

    fn object(&mut self) -> Vec<u8> {
        // (raw name, sort key, value) in sorted order.
        let mut list: Vec<(Vec<u8>, Vec<u16>, Vec<u8>)> = Vec::new();
        let mut next = false;
        'core: while self.err.is_none() && self.peek_non_white() != b'}' {
            if next {
                self.scan_for(b',');
            }
            next = true;
            self.scan_for(b'"');
            let raw = self.quoted();
            if self.err.is_some() {
                break;
            }
            let key: Vec<u16> = String::from_utf8_lossy(&raw).encode_utf16().collect();
            self.scan_for(b':');
            let value = self.element();
            for i in 0..list.len() {
                let Some(old) = list.get(i).map(|e| e.1.clone()) else {
                    break;
                };
                let n = key.len().min(old.len());
                let mut precedes = None;
                for q in 0..n {
                    let (a, b) = (key.get(q).copied().unwrap_or(0), old.get(q).copied().unwrap_or(0));
                    if a < b {
                        precedes = Some(true);
                        break;
                    }
                    if a > b {
                        precedes = Some(false);
                        break;
                    }
                }
                let precedes = precedes.unwrap_or_else(|| {
                    if key.len() < old.len() {
                        return true;
                    }
                    if key.len() == old.len() {
                        let name = list
                            .get(i)
                            .map(|e| String::from_utf8_lossy(&e.0).into_owned())
                            .unwrap_or_default();
                        self.set_error(format!("Duplicate key: {name}"));
                    }
                    false
                });
                if precedes {
                    list.insert(i, (raw, key, value));
                    continue 'core;
                }
            }
            list.push((raw, key, value));
        }
        self.scan();
        let mut out = vec![b'{'];
        for (i, (name, _, value)) in list.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.extend(Self::decorate(name));
            out.push(b':');
            out.extend_from_slice(value);
        }
        out.push(b'}');
        out
    }
}

/// WriteRune of a code point (invalid ones as U+FFFD).
fn push_rune(out: &mut Vec<u8>, r: u32) {
    let ch = char::from_u32(r).unwrap_or('\u{fffd}');
    let mut buf = [0u8; 4];
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
}

/// strconv.ParseFloat(s, 64) of a JSON number token.
fn go_parse_float(s: &str) -> Result<f64, String> {
    let json_number = {
        let b = s.as_bytes();
        let mut i = 0;
        if b.first() == Some(&b'-') {
            i += 1;
        }
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        let mut ok = i > start;
        if b.get(i) == Some(&b'.') {
            i += 1;
            let s2 = i;
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            ok &= i > s2;
        }
        if matches!(b.get(i), Some(b'e' | b'E')) {
            i += 1;
            if matches!(b.get(i), Some(b'+' | b'-')) {
                i += 1;
            }
            let s3 = i;
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            ok &= i > s3;
        }
        ok && i == b.len()
    };
    let q = shards_dockerfile::go::quote(s.as_bytes());
    if !json_number {
        return Err(format!("strconv.ParseFloat: parsing {q}: invalid syntax"));
    }
    let f: f64 = s
        .parse()
        .map_err(|_| format!("strconv.ParseFloat: parsing {q}: invalid syntax"))?;
    if f.is_infinite() {
        return Err(format!("strconv.ParseFloat: parsing {q}: value out of range"));
    }
    Ok(f)
}

/// NumberToJSON: ES6's number serialization.
pub fn number_to_json(f: f64) -> Result<String, String> {
    if !f.is_finite() {
        return Err(format!("Invalid JSON number: {:x}", f.to_bits()));
    }
    if f == 0.0 {
        return Ok("0".into());
    }
    let (sign, f) = if f < 0.0 { ("-", -f) } else { ("", f) };
    let body = if (1e-6..1e21).contains(&f) {
        format!("{f}")
    } else {
        // %e with the shortest digits: Go writes the exponent with a sign and at least
        // two digits, NumberToJSON drops a leading zero.
        let s = format!("{f:e}");
        match s.split_once('e') {
            Some((m, e)) => {
                let (es, ed) = match e.strip_prefix('-') {
                    Some(d) => ('-', d),
                    None => ('+', e),
                };
                format!("{m}e{es}{ed}")
            }
            None => s,
        }
    };
    Ok(format!("{sign}{body}"))
}

/// Transform.
pub fn transform(json: &[u8]) -> Result<Vec<u8>, String> {
    let mut c = Canon {
        b: json,
        at: 0,
        err: None,
    };
    let out = if c.peek_non_white() == b'[' {
        c.scan();
        c.array()
    } else {
        c.scan_for(b'{');
        c.object()
    };
    while let Some(&ch) = c.b.get(c.at) {
        if !white(ch) {
            c.set_error("Improperly terminated JSON object".into());
            break;
        }
        c.at += 1;
    }
    match c.err {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_json_is_rfc_8785_s() {
        assert_eq!(
            transform(br#"{"b":1, "a":[true, 1e21, 0.000001, 1e-7, 9007199254740993], "\u00e9":"\n"}"#)
                .unwrap(),
            "{\"a\":[true,1e+21,0.000001,1e-7,9007199254740992],\"b\":1,\"é\":\"\\n\"}".as_bytes()
        );
        assert_eq!(transform(b"{\"a\":1,\"a\":2}").unwrap_err(), "Duplicate key: a");
    }
}
