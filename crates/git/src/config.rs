//! git-config's syntax (Documentation/git-config.adoc, "Syntax"), as `.gitmodules` is
//! written in it: `[section "subsection"]` headers, `name = value` lines with their
//! quoting, escapes and continuations, and `#`/`;` comments.

/// Each variable as `(section, subsection, name, value)`, in the file's order: section
/// and name lowercased, as git compares them; a bare name's value is `true`.
pub type Entries = Vec<(String, Option<Vec<u8>>, String, Vec<u8>)>;

/// The variables of `text`; `Err` says where it is malformed, as git refuses such a file.
pub fn parse(text: &[u8]) -> Result<Entries, String> {
    let mut out = Vec::new();
    let mut section: Option<(String, Option<Vec<u8>>)> = None;
    let mut i = 0usize;
    let mut line = 1usize;
    let bad = |line: usize| format!("bad config line {line}");
    while i < text.len() {
        let c = text.get(i).copied().unwrap_or(b'\n');
        match c {
            b'\n' => {
                line += 1;
                i += 1;
            }
            b' ' | b'\t' | b'\r' => i += 1,
            b'#' | b';' => {
                while text.get(i).is_some_and(|&c| c != b'\n') {
                    i += 1;
                }
            }
            b'[' => {
                i += 1;
                let start = i;
                while text
                    .get(i)
                    .is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.')
                {
                    i += 1;
                }
                let name =
                    String::from_utf8_lossy(text.get(start..i).unwrap_or_default()).to_ascii_lowercase();
                if name.is_empty() {
                    return Err(bad(line));
                }
                let mut sub = None;
                if text.get(i) == Some(&b' ') {
                    while text.get(i) == Some(&b' ') {
                        i += 1;
                    }
                    if text.get(i) != Some(&b'"') {
                        return Err(bad(line));
                    }
                    i += 1;
                    let mut s = Vec::new();
                    loop {
                        match text.get(i).copied() {
                            Some(b'"') => break,
                            // A backslash keeps the character after it, whatever it is.
                            Some(b'\\') => {
                                s.push(*text.get(i + 1).ok_or_else(|| bad(line))?);
                                i += 2;
                            }
                            Some(b'\n') | None => return Err(bad(line)),
                            Some(c) => {
                                s.push(c);
                                i += 1;
                            }
                        }
                    }
                    i += 1;
                    sub = Some(s);
                }
                if text.get(i) != Some(&b']') {
                    return Err(bad(line));
                }
                i += 1;
                section = Some((name, sub));
            }
            c if c.is_ascii_alphabetic() => {
                let (sec, sub) = section.clone().ok_or_else(|| bad(line))?;
                let start = i;
                while text
                    .get(i)
                    .is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'-')
                {
                    i += 1;
                }
                let name =
                    String::from_utf8_lossy(text.get(start..i).unwrap_or_default()).to_ascii_lowercase();
                while matches!(text.get(i), Some(b' ' | b'\t')) {
                    i += 1;
                }
                let value = match text.get(i) {
                    Some(b'=') => {
                        i += 1;
                        let (v, next, lines) = value(text, i).ok_or_else(|| bad(line))?;
                        i = next;
                        line += lines;
                        v
                    }
                    Some(b'\n' | b'\r' | b'#' | b';') | None => b"true".to_vec(),
                    Some(_) => return Err(bad(line)),
                };
                out.push((sec, sub, name, value));
            }
            _ => return Err(bad(line)),
        }
    }
    Ok(out)
}

/// A value from `at` to its line's end: its quotes taken off, its escapes read, spaces
/// outside quotes kept only between words; with where it ends and the lines it continued
/// over.
fn value(text: &[u8], mut at: usize) -> Option<(Vec<u8>, usize, usize)> {
    let mut out = Vec::new();
    let mut quoted = false;
    // Spaces seen outside quotes and not yet known to be inside the value.
    let mut pending = 0usize;
    let mut lines = 0usize;
    loop {
        let c = text.get(at).copied();
        match c {
            None | Some(b'\n') if !quoted => return Some((out, at, lines)),
            None | Some(b'\n') => return None,
            Some(b'#' | b';') if !quoted => {
                while text.get(at).is_some_and(|&c| c != b'\n') {
                    at += 1;
                }
                return Some((out, at, lines));
            }
            Some(b' ' | b'\t' | b'\r') if !quoted => {
                if !out.is_empty() {
                    pending += 1;
                }
                at += 1;
            }
            Some(b'"') => {
                out.extend(std::iter::repeat_n(b' ', std::mem::take(&mut pending)));
                quoted = !quoted;
                at += 1;
            }
            Some(b'\\') => {
                out.extend(std::iter::repeat_n(b' ', std::mem::take(&mut pending)));
                let e = text.get(at + 1).copied()?;
                match e {
                    b'\n' => lines += 1,
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'b' => {
                        out.pop();
                    }
                    b'"' | b'\\' => out.push(e),
                    _ => return None,
                }
                at += 2;
            }
            Some(c) => {
                out.extend(std::iter::repeat_n(b' ', std::mem::take(&mut pending)));
                out.push(c);
                at += 1;
            }
        }
    }
}

/// A `.gitmodules` entry: the path a submodule is checked out at, and its URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submodule {
    pub name: Vec<u8>,
    pub path: Option<Vec<u8>>,
    pub url: Option<Vec<u8>>,
}

/// The submodules `.gitmodules` names, the last value of each variable winning, as git
/// reads them (submodule-config.c).
pub fn submodules(text: &[u8]) -> Result<Vec<Submodule>, String> {
    let mut out: Vec<Submodule> = Vec::new();
    for (section, sub, name, value) in parse(text)? {
        let Some(sub) = sub.filter(|_| section == "submodule") else {
            continue;
        };
        let i = match out.iter().position(|s| s.name == sub) {
            Some(i) => i,
            None => {
                out.push(Submodule {
                    name: sub,
                    path: None,
                    url: None,
                });
                out.len() - 1
            }
        };
        let Some(entry) = out.get_mut(i) else { continue };
        match name.as_str() {
            "path" => entry.path = Some(value),
            "url" => entry.url = Some(value),
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_read_as_git_config_reads_it() {
        let text = b"# a comment\n\
            [submodule \"mods/sub\"]\n\
            \tpath = mods/sub\n\
            \turl = ../sub.git ; trailing\n\
            [Submodule \"q\\\"x\"]\n\
            \tPATH = \"a  b\" \n\
            \turl = one \\\n  two\n\
            [core]\n\
            \tbare\n";
        let entries = parse(text).unwrap();
        assert_eq!(entries.last().unwrap().3, b"true");
        let subs = submodules(text).unwrap();
        assert_eq!(subs.len(), 2);
        assert_eq!(subs[0].path.as_deref(), Some(&b"mods/sub"[..]));
        assert_eq!(subs[0].url.as_deref(), Some(&b"../sub.git"[..]));
        assert_eq!(subs[1].name, b"q\"x");
        assert_eq!(subs[1].path.as_deref(), Some(&b"a  b"[..]));
        assert_eq!(subs[1].url.as_deref(), Some(&b"one   two"[..]));
        assert!(parse(b"[unclosed\n").is_err());
        assert!(parse(b"key = before any section\n").is_err());
        assert!(parse(b"[s]\nk = \"open\n").is_err());
    }
}
