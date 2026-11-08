//! PEM (RFC 7468) as Go 1.26's `encoding/pem.Decode` reads it: the first block, its
//! headers and its bytes, wiped when dropped (a private key's, for `build --ssh`). Go
//! 1.26 finds the first END line and then the last BEGIN line before it (the fix of
//! CVE-2025-61723, where each BEGIN line without an END searched the rest of the input),
//! so a read is linear in the input; this reads it so.

use zeroize::Zeroizing;

use crate::go::trim_space;

/// A PEM block (RFC 7468) as Go's `pem.Decode` finds the first: its type, its headers
/// (RFC 1421's, by name, each name's last value, as Go's map keeps them), its bytes.
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
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Go's `getLine`: up to a newline, a carriage return before it dropped, trailing spaces
/// and tabs trimmed; what follows the newline; and how many bytes the two spanned.
fn get_line(data: &[u8]) -> (&[u8], &[u8], usize) {
    let (mut i, j) = match data.iter().position(|b| *b == b'\n') {
        Some(i) => (i, i + 1),
        None => (data.len(), data.len()),
    };
    if j > i && i > 0 && data.get(i - 1) == Some(&b'\r') {
        i -= 1;
    }
    let line = data.get(..i).unwrap_or_default();
    let keep = line.len()
        - line
            .iter()
            .rev()
            .take_while(|b| matches!(b, b' ' | b'\t'))
            .count();
    (
        line.get(..keep).unwrap_or_default(),
        data.get(j..).unwrap_or_default(),
        j,
    )
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

/// Go 1.26's `encoding/pem.Decode`, its first block alone. Its indices are Go's ints:
/// one may come to -1 (an END line right after the type line), which Go's checks read.
pub fn decode(data: &[u8]) -> Option<Block> {
    const START: &[u8] = b"\n-----BEGIN ";
    const END: &[u8] = b"\n-----END ";
    const EOL: &[u8] = b"-----";
    let start1 = START.get(1..).unwrap_or_default();
    let int = |n: usize| isize::try_from(n).ok();
    let mut rest = data;
    let mut end_trailer_index: isize = 0;
    loop {
        // Past the END line a block that failed was read up to.
        let skip = usize::try_from(end_trailer_index)
            .ok()
            .filter(|&i| i <= rest.len())?;
        rest = rest.get(skip..)?;
        // The first END line, then the last BEGIN line before it, so that repeated
        // BEGIN lines without an END are each passed once.
        let found = find(rest, END)?;
        let mut end_index = int(found)?;
        end_trailer_index = end_index + int(END.len())?;
        let Some(begin_index) = rfind(rest.get(..found)?, start1) else {
            continue;
        };
        if begin_index > 0 && rest.get(begin_index - 1) != Some(&b'\n') {
            continue;
        }
        let shift = begin_index + start1.len();
        rest = rest.get(shift..)?;
        end_index -= int(shift)?;
        end_trailer_index -= int(shift)?;
        let (type_line, after, consumed) = get_line(rest);
        rest = after;
        end_index -= int(consumed)?;
        end_trailer_index -= int(consumed)?;
        let Some(kind) = type_line.strip_suffix(EOL) else {
            continue;
        };
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        loop {
            if rest.is_empty() {
                return None;
            }
            let (line, next, consumed) = get_line(rest);
            let Some(colon) = line.iter().position(|b| *b == b':') else {
                break;
            };
            let key = String::from_utf8_lossy(trim_space(line.get(..colon).unwrap_or_default())).into_owned();
            let value =
                String::from_utf8_lossy(trim_space(line.get(colon + 1..).unwrap_or_default())).into_owned();
            match index.get(&key) {
                Some(&i) => {
                    if let Some(h) = headers.get_mut(i) {
                        h.1 = value;
                    }
                }
                None => {
                    index.insert(key.clone(), headers.len());
                    headers.push((key, value));
                }
            }
            rest = next;
            end_index -= int(consumed)?;
            end_trailer_index -= int(consumed)?;
        }
        // With headers, a line ends them before the END line.
        if !headers.is_empty() && end_index < 0 {
            continue;
        }
        // After the END line's dashes, the type and five more, and nothing else.
        let Some(end_trailer) = usize::try_from(end_trailer_index)
            .ok()
            .and_then(|i| rest.get(i..))
        else {
            continue;
        };
        let trailer_len = kind.len() + EOL.len();
        let Some((end_line, rest_of_end)) = end_trailer.split_at_checked(trailer_len) else {
            continue;
        };
        if !end_line.starts_with(kind) || !end_line.ends_with(EOL) {
            continue;
        }
        if !get_line(rest_of_end).0.is_empty() {
            continue;
        }
        let mut bytes = Zeroizing::new(Vec::new());
        if let Some(end) = usize::try_from(end_index).ok().filter(|&e| e > 0) {
            // Spaces and tabs removed, as Go's removeSpacesAndTabs, and decoded as
            // StdEncoding.Decode decodes into DecodedLen bytes, which never grow.
            let body: Zeroizing<Vec<u8>> = Zeroizing::new(
                rest.get(..end)
                    .unwrap_or_default()
                    .iter()
                    .copied()
                    .filter(|b| !matches!(b, b' ' | b'\t'))
                    .collect(),
            );
            bytes = Zeroizing::new(Vec::with_capacity(body.len() / 4 * 3));
            if crate::go::base64_decode(&body, &mut bytes).is_err() {
                continue;
            }
        }
        return Some(Block {
            kind: String::from_utf8_lossy(kind).into_owned(),
            headers,
            bytes,
        });
    }
}
