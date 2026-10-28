//! The oslog driver's sending (macOS, D199): each line of a run's log an entry of macOS's
//! unified log, the journal's counterpart there, written by os_log in the daemon (oslog.c),
//! as macOS keeps nothing sent to syslogd's socket (PM M374). Its subsystem `shards`, its
//! category the run's tag, as journald's SYSLOG_IDENTIFIER is, so that `log show
//! --predicate 'subsystem == "shards" AND category == "TAG"'` finds a run's lines; stdout's
//! of the default type, stderr's of the error type, as journald's priorities 6 and 3. A
//! line longer than the 1,015 bytes os_log keeps whole goes in parts of at most that many,
//! each cut at a character's boundary (M374). An entry written is the log's: delivered.

use std::ffi::{CString, c_char, c_void};
use std::io;

use super::{Driver, Message, Wait};
use crate::spec::LOG_STDERR;

unsafe extern "C" {
    fn shards_os_log_create(subsystem: *const c_char, category: *const c_char) -> *mut c_void;
    fn shards_os_log(log: *mut c_void, kind: u8, line: *const c_char);
    fn shards_os_log_release(log: *mut c_void);
}

/// os_log_type_t's (os/log.h): OS_LOG_TYPE_DEFAULT and OS_LOG_TYPE_ERROR.
const DEFAULT: u8 = 0x00;
const ERROR: u8 = 0x10;
/// The most bytes of a line os_log keeps whole (PM M374).
const WHOLE: usize = 1015;
/// Its entries' subsystem.
pub(crate) const SUBSYSTEM: &str = "shards";

/// An os_log log of the run's category, let go of as the driver goes.
struct Log(*mut c_void);

// SAFETY: an os_log object is used from any thread (os/log.h: logging is thread-safe), and
// this one is let go of once, as it drops.
unsafe impl Send for Log {}

impl Drop for Log {
    fn drop(&mut self) {
        // SAFETY: the log os_log_create returned retained, released once.
        unsafe { shards_os_log_release(self.0) };
    }
}

pub(super) struct Oslog {
    log: Log,
}

impl Oslog {
    /// Logs under category `tag`, a NUL in it U+FFFD.
    pub(super) fn new(tag: &str) -> io::Result<Oslog> {
        let c = |s: &str| CString::new(s.replace('\0', "\u{fffd}")).map_err(io::Error::other);
        let (subsystem, category) = (c(SUBSYSTEM)?, c(tag)?);
        // SAFETY: two NUL-terminated strings, which os_log_create copies.
        let log = unsafe { shards_os_log_create(subsystem.as_ptr(), category.as_ptr()) };
        if log.is_null() {
            return Err(io::Error::other("os_log_create made no log"));
        }
        Ok(Oslog { log: Log(log) })
    }
}

impl Driver for Oslog {
    fn send(&mut self, m: &Message<'_>, _wait: &Wait<'_>) -> io::Result<()> {
        let kind = if m.stream == LOG_STDERR { ERROR } else { DEFAULT };
        // Its bytes that are no UTF-8, and a NUL, which would end the C string, U+FFFD.
        let line = String::from_utf8_lossy(m.line).replace('\0', "\u{fffd}");
        for part in parts(&line, WHOLE) {
            let part = CString::new(part).map_err(io::Error::other)?;
            // SAFETY: the log this holds, and a NUL-terminated string that outlives the call.
            unsafe { shards_os_log(self.log.0, kind, part.as_ptr()) };
        }
        Ok(())
    }
}

/// `line` in parts of at most `most` bytes, each cut at a character's boundary: an empty
/// line one empty part.
fn parts(line: &str, most: usize) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = line;
    loop {
        if rest.len() <= most {
            parts.push(rest);
            return parts;
        }
        // The last boundary at or before `most`: a character is at most 4 bytes, and
        // `most` is far more than one.
        let mut cut = most;
        while cut > 0 && !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if cut == 0 {
            parts.push(rest);
            return parts;
        }
        let (part, after) = rest.split_at(cut);
        parts.push(part);
        rest = after;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A line goes whole up to os_log's 1,015 bytes, past them in parts of at most that
    /// many, each cut at a character's boundary; an empty line is one empty entry.
    #[test]
    fn lines_go_in_parts_os_log_keeps_whole() {
        let x = "x".repeat(3000);
        let lens = |line: &str| parts(line, WHOLE).iter().map(|p| p.len()).collect::<Vec<_>>();
        assert_eq!(lens(x.get(..1015).unwrap()), [1015]);
        assert_eq!(lens(&x), [1015, 1015, 970]);
        assert_eq!(lens(""), [0]);
        // A three-byte character across the boundary goes whole into the next part.
        let wide = format!("{}é€{}", "y".repeat(1012), "z".repeat(10));
        let cut = parts(&wide, WHOLE);
        assert_eq!(cut.iter().map(|p| p.len()).collect::<Vec<_>>(), [1014, 13]);
        assert_eq!(cut.concat(), wide);
    }
}
