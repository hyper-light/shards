//! `.dockerignore` files as moby/patternmatcher's ignorefile.ReadAll reads them for
//! BuildKit's frontend: one pattern a line, a UTF-8 byte order mark dropped from the first,
//! `#` comments and blank lines skipped, each pattern trimmed and cleaned, and a leading
//! `/` made relative, `!` kept in front.

use crate::go;

/// bufio.Scanner's longest line.
const MAX_LINE: usize = 64 * 1024;

/// `ignorefile.ReadAll`: the patterns, or bufio's error.
pub fn read_all(text: &[u8]) -> Result<Vec<Vec<u8>>, Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = text;
    let mut first = true;
    while !rest.is_empty() {
        let (line, next) = match rest.iter().position(|&c| c == b'\n') {
            Some(i) => (go::head(rest, i), go::tail(rest, i + 1)),
            None => (rest, &[][..]),
        };
        if line.len() >= MAX_LINE {
            return Err(b"bufio.Scanner: token too long".to_vec());
        }
        rest = next;
        let mut line = line.strip_suffix(b"\r").unwrap_or(line);
        if first {
            line = line.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(line);
            first = false;
        }
        if line.first() == Some(&b'#') {
            continue;
        }
        let mut pattern = go::trim_space(line);
        if pattern.is_empty() {
            continue;
        }
        let invert = pattern.first() == Some(&b'!');
        if invert {
            pattern = go::trim_space(go::tail(pattern, 1));
        }
        let mut p = Vec::new();
        if !pattern.is_empty() {
            p = go::clean(pattern);
            if p.len() > 1 && p.first() == Some(&b'/') {
                p.remove(0);
            }
        }
        if invert {
            p.insert(0, b'!');
        }
        out.push(p);
    }
    Ok(out)
}
