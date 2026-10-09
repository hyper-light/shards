//! Go 1.26's encoding/base64 decoding (base64.go): the standard or URL alphabet, padded
//! or not, CR and LF skipped anywhere, bits after the last octet ignored (not Strict), and
//! a failure as CorruptInputError reports it, at the input octet it found.

/// The decoded octets, or the offset Go's CorruptInputError names.
pub fn decode(src: &[u8], url: bool, padded: bool) -> Result<Vec<u8>, usize> {
    let mut out = Vec::with_capacity(src.len() / 4 * 3 + 3);
    let mut si = 0;
    while si < src.len() {
        let (next, bytes, err) = quantum(src, si, url, padded);
        out.extend_from_slice(&bytes);
        if let Some(e) = err {
            return Err(e);
        }
        si = next;
    }
    Ok(out)
}

/// Go's message for a decoding failure at `offset`.
pub fn error_text(offset: usize) -> String {
    format!("illegal base64 data at input byte {offset}")
}

fn value(c: u8, url: bool) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' if !url => Some(62),
        b'/' if !url => Some(63),
        b'-' if url => Some(62),
        b'_' if url => Some(63),
        _ => None,
    }
}

/// decodeQuantum: up to four symbols from `si`; the next offset, the octets, and the
/// error.
fn quantum(src: &[u8], mut si: usize, url: bool, padded: bool) -> (usize, Vec<u8>, Option<usize>) {
    let mut dbuf = [0u8; 4];
    let mut dlen = 4;
    let mut err = None;
    let mut j = 0;
    while j < 4 {
        let Some(&c) = src.get(si) else {
            if j == 0 {
                return (si, Vec::new(), None);
            }
            if j == 1 || padded {
                return (si, Vec::new(), Some(si - j));
            }
            dlen = j;
            break;
        };
        si += 1;
        if let Some(v) = value(c, url) {
            if let Some(slot) = dbuf.get_mut(j) {
                *slot = v;
            }
            j += 1;
            continue;
        }
        if c == b'\n' || c == b'\r' {
            continue;
        }
        if !padded || c != b'=' {
            return (si, Vec::new(), Some(si - 1));
        }
        match j {
            0 | 1 => return (si, Vec::new(), Some(si - 1)),
            2 => {
                while matches!(src.get(si), Some(b'\n' | b'\r')) {
                    si += 1;
                }
                match src.get(si) {
                    None => return (si, Vec::new(), Some(src.len())),
                    Some(b'=') => si += 1,
                    Some(_) => return (si, Vec::new(), Some(si - 1)),
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
    let [a, b, c, d] = dbuf;
    let val = (u32::from(a) << 18) | (u32::from(b) << 12) | (u32::from(c) << 6) | u32::from(d);
    let bytes = [(val >> 16) as u8, (val >> 8) as u8, val as u8];
    let n = dlen.saturating_sub(1).min(3);
    (si, bytes.get(..n).unwrap_or_default().to_vec(), err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_and_fails_as_go_does() {
        assert_eq!(decode(b"aGVsbG8=", false, true), Ok(b"hello".to_vec()));
        assert_eq!(decode(b"aGVs\nbG8=", false, true), Ok(b"hello".to_vec()));
        assert_eq!(decode(b"aGVsbG8", false, false), Ok(b"hello".to_vec()));
        assert_eq!(decode(b"aGVsbG8", false, true), Err(4));
        assert_eq!(decode(b"aGVsbG9=", false, true), Ok(b"hello".to_vec()));
        assert_eq!(decode(b"aGVsbG8=x", false, true), Err(8));
        assert_eq!(decode(b"a-_b", true, true), Ok(vec![0x6b, 0xef, 0xdb]));
        assert_eq!(decode(b"a-_b", false, true), Err(1));
        assert_eq!(decode(b"=", false, true), Err(0));
    }
}
