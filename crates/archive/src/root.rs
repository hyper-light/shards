//! Go's os.Root (go1.26.1 src/os/root.go, root_openat.go): every operation of an unpack
//! walks its path from the destination one component at a time, never through `..` past
//! it and never through an absolute symlink, following relative symlinks only as long as
//! they stay inside. go-archive v0.3.3 unpacks through it, which is what keeps an
//! archive's names and links from writing outside the destination.

use std::io;

use crate::error::{Error, Kind};
use crate::gopath::{self, Os};

/// What an operation on the final component, or the opening of an intermediate one, did:
/// finished, or found a symlink whose target the walk splices in (Go's errSymlink).
#[derive(Debug)]
pub(crate) enum Step<T> {
    Done(T),
    Link(Vec<u8>),
}

/// Why a walk failed.
#[derive(Debug)]
pub(crate) enum WalkError {
    /// errPathEscapes: `..` past the root, or an absolute symlink.
    Escapes,
    Empty,
    Os(io::Error),
}

impl From<io::Error> for WalkError {
    fn from(e: io::Error) -> WalkError {
        WalkError::Os(e)
    }
}

pub(crate) const ESCAPES: &str = "path escapes from parent";

impl WalkError {
    /// Go's PathError of the operation that walked.
    pub(crate) fn error(self, op: &str, name: &[u8]) -> Error {
        let name = String::from_utf8_lossy(name);
        match self {
            WalkError::Escapes => Error::new(Kind::Breakout, format!("{op} {name}: {ESCAPES}")),
            WalkError::Empty => Error::other(format!("{op} {name}: empty path")),
            WalkError::Os(e) => Error::path(op, name.as_bytes(), &e),
        }
    }

    /// Go's LinkError of the operation: `op old new: cause`.
    pub(crate) fn link_error(self, op: &str, old: &[u8], new: &[u8]) -> Error {
        let (kind, cause) = match self {
            WalkError::Escapes => (Kind::Breakout, ESCAPES.to_string()),
            WalkError::Empty => (Kind::Other, "empty path".to_string()),
            WalkError::Os(e) => (
                if e.kind() == io::ErrorKind::NotFound {
                    Kind::NotFound
                } else {
                    Kind::Other
                },
                crate::error::errno_text(&e),
            ),
        };
        Error::new(
            kind,
            format!(
                "{op} {} {}: {cause}",
                String::from_utf8_lossy(old),
                String::from_utf8_lossy(new)
            ),
        )
    }

    pub(crate) fn is_not_found(&self) -> bool {
        matches!(self, WalkError::Os(e) if e.kind() == io::ErrorKind::NotFound)
    }

    pub(crate) fn escapes(&self) -> bool {
        matches!(self, WalkError::Escapes)
    }
}

/// splitPathInRoot: `s`'s components between `prefix` and `suffix`, `.` dropped but at
/// the end, and the separators that end `s`. On Windows the whole is first cleaned
/// lexically and must stay local (rootCleanPath).
pub(crate) fn split_path(
    os: Os,
    s: &[u8],
    prefix: &[Vec<u8>],
    suffix: &[Vec<u8>],
) -> Result<(Vec<Vec<u8>>, Vec<u8>), WalkError> {
    if s.is_empty() {
        return Err(WalkError::Empty);
    }
    if s.first().is_some_and(|&c| os.is_sep(c)) {
        return Err(WalkError::Escapes);
    }
    let cleaned;
    let (s, prefix, suffix): (&[u8], &[Vec<u8>], &[Vec<u8>]) = if os == Os::Windows {
        if s.contains(&b'?') {
            return Err(WalkError::Os(io::Error::from(io::ErrorKind::InvalidInput)));
        }
        let mut parts: Vec<&[u8]> = prefix.iter().map(Vec::as_slice).collect();
        parts.push(s);
        parts.extend(suffix.iter().map(Vec::as_slice));
        let joined = parts.join(&b'\\');
        cleaned = gopath::clean(os, &joined);
        if !gopath::is_local(os, &cleaned) {
            return Err(WalkError::Escapes);
        }
        (&cleaned, &[], &[])
    } else {
        (s, prefix, suffix)
    };
    let mut parts: Vec<Vec<u8>> = prefix.to_vec();
    let (mut i, mut j) = (0, 1);
    let suffix_sep = loop {
        if j < s.len() && !s.get(j).is_some_and(|&c| os.is_sep(c)) {
            j += 1;
            continue;
        }
        parts.push(s.get(i..j).unwrap_or_default().to_vec());
        let part_end = j;
        while j < s.len() && s.get(j).is_some_and(|&c| os.is_sep(c)) {
            j += 1;
        }
        if j == s.len() {
            break s.get(part_end..).unwrap_or_default().to_vec();
        }
        if parts.last().is_some_and(|p| p == b".") {
            parts.pop();
        }
        i = j;
    };
    if !suffix.is_empty() && parts.last().is_some_and(|p| p == b".") {
        parts.pop();
    }
    parts.extend(suffix.iter().cloned());
    Ok((parts, suffix_sep))
}

/// rootMaxSymlinks.
const MAX_SYMLINKS: usize = 8;

/// doInRoot: walks `name` from the root, opening each directory with `open_dir`, then
/// runs `f` on the last component's parent and name.
pub(crate) fn walk<H, T>(
    os: Os,
    root: impl Fn() -> H,
    name: &[u8],
    open_dir: impl Fn(&H, &[u8]) -> io::Result<Step<H>>,
    mut f: impl FnMut(&H, &[u8]) -> io::Result<Step<T>>,
) -> Result<T, WalkError> {
    let (mut parts, mut suffix_sep) = split_path(os, name, &[], &[])?;
    let mut dir = root();
    let (mut i, mut steps, mut restarts, mut symlinks) = (0usize, 0usize, 0usize, 0usize);
    loop {
        steps += 1;
        if steps > 255 && restarts > 8 {
            return Err(WalkError::Os(io::Error::from_raw_os_error(name_too_long())));
        }
        let Some(part) = parts.get(i).cloned() else {
            return Err(WalkError::Empty);
        };
        if part == b".." {
            restarts += 1;
            let mut end = i + 1;
            while parts.get(end).is_some_and(|p| p == b"..") {
                end += 1;
            }
            let count = end - i;
            if count > i {
                return Err(WalkError::Escapes);
            }
            parts.drain(i - count..end);
            if parts.is_empty() {
                parts.push(b".".to_vec());
            }
            i = 0;
            dir = root();
            continue;
        }
        let last = i + 1 == parts.len();
        let step = if last {
            match f(&dir, &[part.as_slice(), &suffix_sep].concat())? {
                Step::Done(t) => return Ok(t),
                Step::Link(l) => l,
            }
        } else {
            match open_dir(&dir, &part)? {
                Step::Done(d) => {
                    dir = d;
                    i += 1;
                    continue;
                }
                Step::Link(l) => l,
            }
        };
        symlinks += 1;
        if symlinks > MAX_SYMLINKS {
            return Err(WalkError::Os(io::Error::from_raw_os_error(too_many_links())));
        }
        let before = parts.get(..i).unwrap_or_default().to_vec();
        let after = parts.get(i + 1..).unwrap_or_default().to_vec();
        let (new_parts, new_sep) = split_path(os, &step, &before, &after)?;
        if last {
            suffix_sep = new_sep;
        }
        if new_parts.len() < i || new_parts.get(..i) != Some(before.as_slice()) {
            i = 0;
            dir = root();
        }
        parts = new_parts;
    }
}

#[cfg(unix)]
fn name_too_long() -> i32 {
    libc::ENAMETOOLONG
}

#[cfg(unix)]
fn too_many_links() -> i32 {
    libc::ELOOP
}

/// ERROR_FILENAME_EXCED_RANGE, ERROR_CANT_RESOLVE_FILENAME.
#[cfg(windows)]
fn name_too_long() -> i32 {
    206
}

#[cfg(windows)]
fn too_many_links() -> i32 {
    1921
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(s: &str) -> Vec<String> {
        let (p, sep) = split_path(Os::Unix, s.as_bytes(), &[], &[]).unwrap();
        let mut out: Vec<String> = p.into_iter().map(|p| String::from_utf8(p).unwrap()).collect();
        out.push(String::from_utf8(sep).unwrap());
        out
    }

    // splitPathInRoot's cases from go1.26.1 src/os/root_test.go (TestRootSplitPath).
    #[test]
    fn splits_as_go() {
        assert_eq!(parts("a"), ["a", ""]);
        assert_eq!(parts("a/b"), ["a", "b", ""]);
        assert_eq!(parts("a/b/"), ["a", "b", "/"]);
        assert_eq!(parts("a//b//"), ["a", "b", "//"]);
        assert_eq!(parts("./a/./b/."), ["a", "b", ".", ""]);
        assert_eq!(parts("a/../b"), ["a", "..", "b", ""]);
        assert!(matches!(
            split_path(Os::Unix, b"/a", &[], &[]),
            Err(WalkError::Escapes)
        ));
        assert!(matches!(
            split_path(Os::Unix, b"", &[], &[]),
            Err(WalkError::Empty)
        ));
        let (p, _) = split_path(Os::Windows, b"a\\..\\b", &[b"x".to_vec()], &[]).unwrap();
        assert_eq!(p, [b"x".to_vec(), b"b".to_vec()]);
        assert!(matches!(
            split_path(Os::Windows, b"..\\..\\b", &[b"x".to_vec()], &[]),
            Err(WalkError::Escapes)
        ));
    }

    /// A tree of directories and symlinks, walked as the OS walks one.
    fn fake_walk(name: &str) -> Result<String, String> {
        let links: &[(&str, &str)] = &[
            ("in", "d"),
            ("up", "../x"),
            ("abs", "/etc"),
            ("deep", "d/../d"),
            ("loop", "loop"),
        ];
        let lookup = |dir: &String, part: &[u8]| -> io::Result<Step<String>> {
            let part = String::from_utf8_lossy(part).into_owned();
            let path = if dir.is_empty() {
                part.clone()
            } else {
                format!("{dir}/{part}")
            };
            match links.iter().find(|(l, _)| *l == path) {
                Some((_, t)) => Ok(Step::Link(t.as_bytes().to_vec())),
                None => Ok(Step::Done(path)),
            }
        };
        walk(Os::Unix, String::new, name.as_bytes(), lookup, |dir, last| {
            let last = String::from_utf8_lossy(last).into_owned();
            match links.iter().find(|(l, _)| *l == last && dir.is_empty()) {
                Some((_, t)) => Ok(Step::Link(t.as_bytes().to_vec())),
                None => Ok(Step::Done(if dir.is_empty() {
                    last
                } else {
                    format!("{dir}/{last}")
                })),
            }
        })
        .map_err(|e| e.error("op", name.as_bytes()).to_string())
    }

    #[test]
    fn walks_stay_inside() {
        assert_eq!(fake_walk("in/f").unwrap(), "d/f");
        assert_eq!(fake_walk("deep/f").unwrap(), "d/f");
        assert_eq!(fake_walk("a/../f").unwrap(), "f");
        assert_eq!(
            fake_walk("up/f").unwrap_err(),
            "op up/f: path escapes from parent"
        );
        assert_eq!(
            fake_walk("abs/f").unwrap_err(),
            "op abs/f: path escapes from parent"
        );
        assert_eq!(
            fake_walk("a/../../f").unwrap_err(),
            "op a/../../f: path escapes from parent"
        );
        assert!(fake_walk("loop/f").is_err());
        // The last component's symlink is the operation's to follow or not.
        assert_eq!(fake_walk("in").unwrap(), "d");
    }
}
