//! What Go's standard library does to bytes where shards must do the same to read as Go
//! reads: `bytes.TrimSpace`, and `encoding/base64`'s `StdEncoding.Decode` (Go 1.26), its
//! errors' offsets included.

/// Go's asciiSpace: the bytes `unicode.IsSpace` takes below 0x80.
fn ascii_space(c: u8) -> bool {
    matches!(c, b'\t' | b'\n' | 0x0b | 0x0c | b'\r' | b' ')
}

/// The first rune of `b` and its width, or none where `b` does not begin with valid UTF-8
/// (utf8.DecodeRune's RuneError, which is no space).
fn first_rune(b: &[u8]) -> Option<(char, usize)> {
    (1..=b.len().min(4)).find_map(|n| {
        std::str::from_utf8(b.get(..n)?)
            .ok()
            .and_then(|s| s.chars().next().map(|c| (c, n)))
    })
}

/// The last rune of `b` and its width, as utf8.DecodeLastRune finds it.
fn last_rune(b: &[u8]) -> Option<(char, usize)> {
    (1..=b.len().min(4)).find_map(|n| {
        std::str::from_utf8(b.get(b.len() - n..)?).ok().and_then(|s| {
            s.chars()
                .next_back()
                .filter(|c| c.len_utf8() == n)
                .map(|c| (c, n))
        })
    })
}

/// How many bytes of white space `b` begins with: an ASCII space's one, or a rune that
/// `unicode.IsSpace` takes (the Unicode White_Space property, which `char::is_whitespace`
/// tests).
fn leading_space(b: &[u8]) -> Option<usize> {
    match *b.first()? {
        c if c < 0x80 => ascii_space(c).then_some(1),
        _ => first_rune(b).filter(|(c, _)| c.is_whitespace()).map(|(_, n)| n),
    }
}

fn trailing_space(b: &[u8]) -> Option<usize> {
    match *b.last()? {
        c if c < 0x80 => ascii_space(c).then_some(1),
        _ => last_rune(b).filter(|(c, _)| c.is_whitespace()).map(|(_, n)| n),
    }
}

/// `bytes.TrimSpace`: leading and trailing white space, runes decoded where they are
/// valid UTF-8 and an invalid byte ending the trim.
pub(crate) fn trim_space(mut b: &[u8]) -> &[u8] {
    while let Some(n) = leading_space(b) {
        b = b.get(n..).unwrap_or_default();
    }
    while let Some(n) = trailing_space(b) {
        b = b.get(..b.len() - n).unwrap_or_default();
    }
    b
}

/// No symbol, in [`DECODE`].
const NONE: u8 = 0xff;

/// StdEncoding's decodeMap: a symbol's six bits, NONE for any other byte ('=' too).
const DECODE: [u8; 256] = {
    let symbols = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut map = [NONE; 256];
    let mut i = 0;
    while i < symbols.len() {
        // Evaluated when compiled: an index out of bounds fails the build.
        #[allow(clippy::indexing_slicing)]
        {
            map[symbols[i] as usize] = i as u8;
        }
        i += 1;
    }
    map
};

fn symbol(c: u8) -> Option<u8> {
    DECODE.get(usize::from(c)).copied().filter(|&v| v != NONE)
}

/// Whether `c` is one of StdEncoding's 64 symbols.
pub(crate) fn is_symbol(c: u8) -> bool {
    symbol(c).is_some()
}

/// `base64.StdEncoding.Decode` of `src` onto `out`: four symbols at once while all four
/// are symbols (Go's assemble32 and assemble64), and elsewhere a quantum as
/// decodeQuantum reads it (line breaks skipped, padding ending the data, trailing bits
/// allowed); the bytes of the quanta before an error kept, and the error as
/// CorruptInputError's offset.
pub(crate) fn base64_decode(src: &[u8], out: &mut Vec<u8>) -> Result<(), usize> {
    let six = |x: u8| u64::from(DECODE.get(usize::from(x)).copied().unwrap_or(NONE));
    let mut si = 0;
    while si < src.len() {
        if let Some(q) = src.get(si..).and_then(<[u8]>::first_chunk::<8>) {
            let v = q.map(six);
            if v.iter().fold(0, |a, x| a | x) != u64::from(NONE) {
                let val = v.iter().fold(0u64, |a, x| (a << 6) | x);
                out.extend_from_slice(val.to_be_bytes().get(2..).unwrap_or_default());
                si += 8;
                continue;
            }
        }
        if let Some(q) = src.get(si..).and_then(<[u8]>::first_chunk::<4>) {
            let v = q.map(six);
            if v.iter().fold(0, |a, x| a | x) != u64::from(NONE) {
                let val = v.iter().fold(0u64, |a, x| (a << 6) | x);
                out.extend_from_slice(val.to_be_bytes().get(5..).unwrap_or_default());
                si += 4;
                continue;
            }
        }
        let mut dbuf = [0u8; 4];
        let mut dlen = 4;
        let mut garbage = None;
        let mut j = 0;
        while j < 4 {
            let Some(&c) = src.get(si) else {
                if j == 0 {
                    return Ok(());
                }
                // A quantum cut short: StdEncoding pads, so any is an error.
                return Err(si - j);
            };
            si += 1;
            if let Some(v) = symbol(c) {
                if let Some(d) = dbuf.get_mut(j) {
                    *d = v;
                }
                j += 1;
                continue;
            }
            if c == b'\n' || c == b'\r' {
                continue;
            }
            if c != b'=' {
                return Err(si - 1);
            }
            match j {
                0 | 1 => return Err(si - 1),
                2 => {
                    // "==": the second after line breaks.
                    while matches!(src.get(si), Some(b'\n' | b'\r')) {
                        si += 1;
                    }
                    match src.get(si) {
                        None => return Err(src.len()),
                        Some(b'=') => si += 1,
                        Some(_) => return Err(si - 1),
                    }
                }
                _ => {}
            }
            while matches!(src.get(si), Some(b'\n' | b'\r')) {
                si += 1;
            }
            if si < src.len() {
                garbage = Some(si);
            }
            dlen = j;
            break;
        }
        let val = (u32::from(dbuf[0]) << 18)
            | (u32::from(dbuf[1]) << 12)
            | (u32::from(dbuf[2]) << 6)
            | u32::from(dbuf[3]);
        let bytes = val.to_be_bytes();
        out.extend_from_slice(bytes.get(1..dlen).unwrap_or_default());
        if let Some(at) = garbage {
            return Err(at);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use base64::Engine as _;

    use super::*;

    fn oracle() -> serde_json::Value {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/stdlib.json");
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    /// Every input of stdlib.json decoded as Go 1.26's StdEncoding.Decode decodes it: the
    /// bytes it writes, and its error's offset (`scripts/gitsign/generate-stdlib`).
    #[test]
    fn decodes_as_go_decodes() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut failures = Vec::new();
        for c in oracle()["decode"].as_array().unwrap() {
            let input = b64.decode(c["input"].as_str().unwrap()).unwrap();
            let mut out = Vec::new();
            let error = base64_decode(&input, &mut out)
                .err()
                .map(|at| format!("illegal base64 data at input byte {at}"));
            let want = (
                c["out"].as_str().unwrap().to_string(),
                c["error"].as_str().map(str::to_string),
            );
            let got = (b64.encode(&out), error);
            if got != want {
                failures.push(format!("{input:?}: got {got:?}, want {want:?}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// Every input of stdlib.json trimmed as Go 1.26's bytes.TrimSpace trims it.
    #[test]
    fn trims_as_go_trims() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let mut failures = Vec::new();
        for c in oracle()["trim"].as_array().unwrap() {
            let input = b64.decode(c["input"].as_str().unwrap()).unwrap();
            let want = b64.decode(c["out"].as_str().unwrap()).unwrap();
            if trim_space(&input) != want.as_slice() {
                failures.push(format!("{input:?}: got {:?}, want {want:?}", trim_space(&input)));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
