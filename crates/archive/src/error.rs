//! Errors worded as Go and go-archive word them, since Docker shows them as they are.

use std::fmt;
use std::io;

/// What went wrong, for callers that act on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Kind {
    /// An archive entry tried to reach outside the destination (go-archive's breakoutErr).
    Breakout,
    /// A file is missing (os.IsNotExist).
    NotFound,
    /// copy.go's ErrNotDirectory: a destination's parent is not a directory.
    NotDirectory,
    /// copy.go's ErrDirNotExists: a file copied to a missing path that asserts a directory.
    DirNotExists,
    /// copy.go's ErrCannotCopyDir: a directory copied onto an existing file.
    CannotCopyDir,
    /// The archive is not one Go's archive/tar reads.
    Header,
    Other,
}

/// An error, with Go's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    kind: Kind,
    msg: String,
}

impl Error {
    pub(crate) fn new(kind: Kind, msg: impl Into<String>) -> Error {
        Error {
            kind,
            msg: msg.into(),
        }
    }

    pub(crate) fn other(msg: impl Into<String>) -> Error {
        Error::new(Kind::Other, msg)
    }

    pub(crate) fn breakout(msg: impl Into<String>) -> Error {
        Error::new(Kind::Breakout, msg)
    }

    /// Go's os.PathError: `op path: err`.
    pub(crate) fn path(op: &str, path: &[u8], err: &io::Error) -> Error {
        Error::new(
            kind_of(err),
            format!("{op} {}: {}", String::from_utf8_lossy(path), errno_text(err)),
        )
    }

    /// An I/O error as Go prints it.
    pub(crate) fn io(err: &io::Error) -> Error {
        Error::new(kind_of(err), errno_text(err))
    }

    /// The same error, its text behind `prefix: `, as Go's `fmt.Errorf("...: %w")`.
    pub(crate) fn wrap(self, prefix: &str) -> Error {
        Error {
            kind: self.kind,
            msg: format!("{prefix}: {}", self.msg),
        }
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn is_not_found(&self) -> bool {
        self.kind == Kind::NotFound
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::io(&err)
    }
}

fn kind_of(err: &io::Error) -> Kind {
    match err.kind() {
        io::ErrorKind::NotFound => Kind::NotFound,
        _ => Kind::Other,
    }
}

/// An error's text as Go's syscall.Errno and io errors print it (golang.org/x/sys
/// zerrors_linux.go and zerrors_darwin.go's errors tables), else Rust's own without its
/// `(os error N)`.
pub(crate) fn errno_text(err: &io::Error) -> String {
    if err.kind() == io::ErrorKind::UnexpectedEof {
        return "unexpected EOF".into();
    }
    if let Some(text) = err.raw_os_error().and_then(go_errno) {
        return text.into();
    }
    if err.raw_os_error().is_none() {
        return err.to_string();
    }
    let s = err.to_string();
    match s.rfind(" (os error ") {
        Some(i) => s.get(..i).unwrap_or(&s).to_string(),
        None => s,
    }
}

#[cfg(unix)]
fn go_errno(n: i32) -> Option<&'static str> {
    let text = match n {
        libc::EPERM => "operation not permitted",
        libc::ENOENT => "no such file or directory",
        libc::EINTR => "interrupted system call",
        libc::EIO => "input/output error",
        libc::ENXIO => "no such device or address",
        libc::E2BIG => "argument list too long",
        libc::EBADF => "bad file descriptor",
        libc::ENOMEM => "cannot allocate memory",
        libc::EACCES => "permission denied",
        libc::EEXIST => "file exists",
        libc::ENODEV => "no such device",
        libc::ENOTDIR => "not a directory",
        libc::EISDIR => "is a directory",
        libc::EINVAL => "invalid argument",
        libc::ENFILE => "too many open files in system",
        libc::EMFILE => "too many open files",
        libc::ETXTBSY => "text file busy",
        libc::EFBIG => "file too large",
        libc::ENOSPC => "no space left on device",
        libc::EROFS => "read-only file system",
        libc::EMLINK => "too many links",
        libc::EPIPE => "broken pipe",
        libc::ENAMETOOLONG => "file name too long",
        libc::ENOSYS => "function not implemented",
        libc::ENOTEMPTY => "directory not empty",
        libc::ELOOP => "too many levels of symbolic links",
        _ => return platform_errno(n),
    };
    Some(text)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn platform_errno(n: i32) -> Option<&'static str> {
    let text = match n {
        libc::EXDEV => "invalid cross-device link",
        libc::EBUSY => "device or resource busy",
        libc::ERANGE => "numerical result out of range",
        libc::ENODATA => "no data available",
        libc::EOPNOTSUPP => "operation not supported",
        libc::EDQUOT => "disk quota exceeded",
        libc::EAGAIN => "resource temporarily unavailable",
        _ => return None,
    };
    Some(text)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn platform_errno(n: i32) -> Option<&'static str> {
    let text = match n {
        libc::EXDEV => "cross-device link",
        libc::EBUSY => "resource busy",
        libc::ERANGE => "result too large",
        libc::ENOTSUP => "operation not supported",
        libc::EOPNOTSUPP => "operation not supported on socket",
        libc::EDQUOT => "disc quota exceeded",
        libc::EAGAIN => "resource temporarily unavailable",
        _ => return None,
    };
    Some(text)
}

#[cfg(windows)]
fn go_errno(_: i32) -> Option<&'static str> {
    None
}

/// Go's strconv.Quote, which `%q` prints: printable runes as they are, the rest escaped.
/// Printable is approximated as not a control and not a space other than ' ', where Go
/// consults Unicode's tables.
pub(crate) fn quote(s: &[u8]) -> String {
    let mut out = String::from("\"");
    let mut rest = s;
    while !rest.is_empty() {
        let (c, width) = decode_rune(rest);
        let raw = rest.get(..width).unwrap_or_default();
        rest = rest.get(width..).unwrap_or_default();
        match c {
            None => {
                for b in raw {
                    out.push_str(&format!("\\x{b:02x}"));
                }
            }
            Some('"') => out.push_str("\\\""),
            Some('\\') => out.push_str("\\\\"),
            Some('\x07') => out.push_str("\\a"),
            Some('\x08') => out.push_str("\\b"),
            Some('\x0c') => out.push_str("\\f"),
            Some('\n') => out.push_str("\\n"),
            Some('\r') => out.push_str("\\r"),
            Some('\t') => out.push_str("\\t"),
            Some('\x0b') => out.push_str("\\v"),
            Some(c) if c == ' ' || (!c.is_control() && !c.is_whitespace()) => out.push(c),
            Some(c) if (c as u32) < 0x20 || c == '\x7f' => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            Some(c) if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
            Some(c) => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push('"');
    out
}

/// Go's utf8.DecodeRune: the rune at the start of `s` and its width, or None and 1 for a
/// byte that starts no valid rune.
pub(crate) fn decode_rune(s: &[u8]) -> (Option<char>, usize) {
    let width = match s.first() {
        None => return (None, 0),
        Some(&b) if b < 0x80 => 1,
        Some(&b) if b >= 0xf0 => 4,
        Some(&b) if b >= 0xe0 => 3,
        Some(&b) if b >= 0xc0 => 2,
        Some(_) => return (None, 1),
    };
    match s.get(..width).and_then(|r| std::str::from_utf8(r).ok()) {
        Some(r) => (r.chars().next(), width),
        None => (None, 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_as_go_does() {
        assert_eq!(quote(b"a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(quote(b"\x00\n\x7f"), r#""\x00\n\x7f""#);
        assert_eq!(quote("é日".as_bytes()), "\"é日\"");
        assert_eq!(quote(b"\xff"), r#""\xff""#);
        assert_eq!(quote("\u{a0}".as_bytes()), "\"\\u00a0\"");
    }
}
