//! PEM (RFC 7468) as Go 1.26's `encoding/pem.Decode` reads it: the first block, its
//! headers and its bytes, wiped when dropped (a private key's, for `build --ssh`).

use zeroize::Zeroizing;

/// A PEM block (RFC 7468) as Go's `pem.Decode` finds the first: its type, its headers
/// (RFC 1421's, the last of a name kept), its bytes.
#[derive(Debug)]
pub struct Block {
    pub kind: String,
    pub headers: Vec<(String, String)>,
    pub bytes: Zeroizing<Vec<u8>>,
}

impl Block {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Go's `getLine`: up to a newline, trailing spaces, tabs and carriage returns trimmed;
/// and what follows it.
fn get_line(data: &[u8]) -> (&[u8], &[u8]) {
    let (line, rest) = match data.iter().position(|b| *b == b'\n') {
        Some(i) => (
            data.get(..i).unwrap_or_default(),
            data.get(i + 1..).unwrap_or_default(),
        ),
        None => (data, &[][..]),
    };
    let keep = line.len()
        - line
            .iter()
            .rev()
            .take_while(|b| matches!(b, b' ' | b'\t' | b'\r'))
            .count();
    (line.get(..keep).unwrap_or_default(), rest)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn trim(s: &[u8]) -> &[u8] {
    s.trim_ascii()
}

/// Go 1.26's `encoding/pem.Decode`, its first block alone.
pub fn decode(data: &[u8]) -> Option<Block> {
    use base64::Engine as _;
    const START: &[u8] = b"\n-----BEGIN ";
    const END: &[u8] = b"\n-----END ";
    const EOL: &[u8] = b"-----";
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::RequireCanonical),
    );
    let start1 = START.get(1..).unwrap_or_default();
    let end1 = END.get(1..).unwrap_or_default();
    let mut rest = data;
    loop {
        if rest.starts_with(start1) {
            rest = rest.get(start1.len()..).unwrap_or_default();
        } else {
            let i = find(rest, START)?;
            rest = rest.get(i + START.len()..).unwrap_or_default();
        }
        let (type_line, after) = get_line(rest);
        rest = after;
        let Some(kind) = type_line.strip_suffix(EOL) else {
            continue;
        };
        let mut headers = Vec::new();
        loop {
            if rest.is_empty() {
                return None;
            }
            let (line, next) = get_line(rest);
            let Some(colon) = line.iter().position(|b| *b == b':') else {
                break;
            };
            let (k, v) = (
                line.get(..colon).unwrap_or_default(),
                line.get(colon + 1..).unwrap_or_default(),
            );
            headers.push((
                String::from_utf8_lossy(trim(k)).into_owned(),
                String::from_utf8_lossy(trim(v)).into_owned(),
            ));
            rest = next;
        }
        let (end_index, trailer_index) = if headers.is_empty() && rest.starts_with(end1) {
            (0, end1.len())
        } else {
            match find(rest, END) {
                Some(i) => (i, i + END.len()),
                None => continue,
            }
        };
        let trailer = rest.get(trailer_index..).unwrap_or_default();
        let trailer_len = kind.len() + EOL.len();
        let Some((end_line, rest_of_end)) = trailer.split_at_checked(trailer_len) else {
            continue;
        };
        if !end_line.starts_with(kind) || !end_line.ends_with(EOL) {
            continue;
        }
        if !get_line(rest_of_end).0.is_empty() {
            continue;
        }
        // Spaces and tabs removed, as Go's removeSpacesAndTabs; and line breaks, which
        // Go's base64 decoder skips.
        let body: Zeroizing<Vec<u8>> = Zeroizing::new(
            rest.get(..end_index)
                .unwrap_or_default()
                .iter()
                .copied()
                .filter(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
                .collect(),
        );
        let Ok(bytes) = engine.decode(&*body) else {
            continue;
        };
        return Some(Block {
            kind: String::from_utf8_lossy(kind).into_owned(),
            headers,
            bytes: Zeroizing::new(bytes),
        });
    }
}
