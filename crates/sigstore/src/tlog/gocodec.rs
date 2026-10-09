//! Go 1.26's encoding/base64 decoding (decodeQuantum: line breaks skipped, padding as
//! the encoding asks, the offset of the first corrupt byte) and encoding/hex, with their
//! errors in Go's words.

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// An encoding: its alphabet and whether it pads.
#[derive(Debug, Clone, Copy)]
pub struct Encoding {
    url: bool,
    padded: bool,
}

pub const STD_ENCODING: Encoding = Encoding {
    url: false,
    padded: true,
};
pub const URL_ENCODING: Encoding = Encoding {
    url: true,
    padded: true,
};
pub const RAW_STD_ENCODING: Encoding = Encoding {
    url: false,
    padded: false,
};
pub const RAW_URL_ENCODING: Encoding = Encoding {
    url: true,
    padded: false,
};

impl Encoding {
    fn value(&self, c: u8) -> Option<u8> {
        let alphabet = if self.url { URL } else { STD };
        alphabet
            .iter()
            .position(|x| *x == c)
            .and_then(|p| u8::try_from(p).ok())
    }

    /// Decode: the octets, or the offset CorruptInputError reports.
    pub fn decode(&self, src: &[u8]) -> Result<Vec<u8>, usize> {
        let mut out = Vec::with_capacity(src.len() / 4 * 3 + 3);
        let mut si = 0;
        while si < src.len() {
            let mut dbuf = [0u8; 4];
            let mut dlen = 4;
            let mut j = 0;
            let mut err: Option<usize> = None;
            while j < 4 {
                let Some(&c) = src.get(si) else {
                    if j == 0 {
                        return Ok(out);
                    }
                    if j == 1 || self.padded {
                        return Err(si - j);
                    }
                    dlen = j;
                    break;
                };
                si += 1;
                if let Some(v) = self.value(c) {
                    if let Some(slot) = dbuf.get_mut(j) {
                        *slot = v;
                    }
                    j += 1;
                    continue;
                }
                if c == b'\n' || c == b'\r' {
                    continue;
                }
                if !self.padded || c != b'=' {
                    return Err(si - 1);
                }
                match j {
                    0 | 1 => return Err(si - 1),
                    2 => {
                        while matches!(src.get(si), Some(b'\n' | b'\r')) {
                            si += 1;
                        }
                        match src.get(si) {
                            None => return Err(src.len()),
                            Some(&b'=') => si += 1,
                            Some(_) => return Err(si - 1),
                        }
                    }
                    _ => {}
                }
                while matches!(src.get(si), Some(b'\n' | b'\r')) {
                    si += 1;
                }
                if si < src.len() {
                    err = Some(si);
                }
                dlen = j;
                break;
            }
            let val = (u32::from(dbuf[0]) << 18)
                | (u32::from(dbuf[1]) << 12)
                | (u32::from(dbuf[2]) << 6)
                | u32::from(dbuf[3]);
            let bytes = [(val >> 16) as u8, (val >> 8) as u8, val as u8];
            out.extend_from_slice(bytes.get(..dlen.saturating_sub(1)).unwrap_or_default());
            if let Some(e) = err {
                return Err(e);
            }
        }
        Ok(out)
    }
}

/// CorruptInputError's words.
pub fn corrupt(at: usize) -> String {
    format!("illegal base64 data at input byte {at}")
}

/// StdEncoding.EncodeToString.
pub fn std_encode(b: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// strconv.IsPrint for a rune below 0x100.
fn latin1_printable(c: u8) -> bool {
    (0x20..0x7f).contains(&c) || (c >= 0xa1 && c != 0xad)
}

/// InvalidByteError's words: `%#U` of the byte as a rune.
fn invalid_byte(c: u8) -> String {
    if latin1_printable(c) {
        format!("encoding/hex: invalid byte: U+{:04X} '{}'", c, char::from(c))
    } else {
        format!("encoding/hex: invalid byte: U+{c:04X}")
    }
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// hex.DecodeString.
pub fn hex_decode(s: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() / 2);
    let (pairs, rest) = s.as_chunks::<2>();
    for &[p, q] in pairs {
        let a = nibble(p).ok_or_else(|| invalid_byte(p))?;
        let b = nibble(q).ok_or_else(|| invalid_byte(q))?;
        out.push((a << 4) | b);
    }
    if let [last] = rest {
        if nibble(*last).is_none() {
            return Err(invalid_byte(*last));
        }
        return Err("encoding/hex: odd length hex string".into());
    }
    Ok(out)
}

/// hex.EncodeToString.
pub fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_fails_where_go_s_does() {
        assert_eq!(STD_ENCODING.decode(b"YWJj").unwrap(), b"abc");
        assert_eq!(STD_ENCODING.decode(b"YW\nJj").unwrap(), b"abc");
        assert_eq!(STD_ENCODING.decode(b"YQ==").unwrap(), b"a");
        assert_eq!(STD_ENCODING.decode(b"YQ="), Err(3));
        assert_eq!(STD_ENCODING.decode(b"YQ"), Err(0));
        assert_eq!(STD_ENCODING.decode(b"Y"), Err(0));
        assert_eq!(STD_ENCODING.decode(b"YQ==YQ=="), Err(4));
        assert_eq!(STD_ENCODING.decode(b"Y!=="), Err(1));
        assert_eq!(RAW_STD_ENCODING.decode(b"YQ").unwrap(), b"a");
        assert_eq!(
            hex_decode(b"0g").unwrap_err(),
            "encoding/hex: invalid byte: U+0067 'g'"
        );
        assert_eq!(
            hex_decode(b"0").unwrap_err(),
            "encoding/hex: odd length hex string"
        );
        assert_eq!(
            hex_decode(b"z").unwrap_err(),
            "encoding/hex: invalid byte: U+007A 'z'"
        );
    }
}
