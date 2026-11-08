//! The oslog driver's sending (macOS, D199): each line of a run's log an entry of macOS's
//! unified log, the journal's counterpart there, written by os_log in the daemon (oslog.c),
//! as macOS keeps nothing sent to syslogd's socket (PM M374). Its subsystem `shards`, its
//! category the run's tag, as journald's SYSLOG_IDENTIFIER is, so that `log show
//! --predicate 'subsystem == "shards" AND category == "TAG"'` finds a run's lines; stdout's
//! of the default type, stderr's of the error type, as journald's priorities 6 and 3. A
//! line longer than os_log keeps whole goes in parts of at most that many bytes, each cut at
//! a character's boundary. How long that is the log itself shows as the daemon runs
//! (`whole`): os/log.h states "a physical cap of 1024 bytes per log line for dynamic
//! content", and the log keeps less of a `%s` (1,015 bytes on macOS 26, PM M374, M494), so
//! the driver writes entries of its own and reads them back from its process's log store.
//! An entry written is the log's: delivered.

use std::ffi::{CString, c_char, c_long, c_void};
use std::io;
use std::sync::{Mutex, PoisonError};

use super::{Driver, Message, Wait};
use crate::spec::LOG_STDERR;

unsafe extern "C" {
    fn shards_os_log_create(subsystem: *const c_char, category: *const c_char) -> *mut c_void;
    fn shards_os_log(log: *mut c_void, kind: u8, line: *const c_char);
    fn shards_os_log_release(log: *mut c_void);
    fn shards_os_log_read_back(
        category: *const c_char,
        prefix: *const c_char,
        buf: *mut c_char,
        cap: c_long,
    ) -> c_long;
}

/// os_log_type_t's (os/log.h): OS_LOG_TYPE_DEFAULT and OS_LOG_TYPE_ERROR.
const DEFAULT: u8 = 0x00;
const ERROR: u8 = 0x10;
/// The cap os/log.h states on a line's dynamic content: "There is a physical cap of 1024
/// bytes per log line for dynamic content, such as %s and %@ ... All content exceeding the
/// limit will be truncated" (macOS SDK os/log.h, os_log_with_type). Where the log cannot be
/// read back, a line goes in parts of it.
const STATED: usize = 1024;

/// How many bytes of a line os_log keeps whole, as this host's log shows it: learned once
/// for the daemon's life by [`learn`], the stated cap where the log cannot be read back.
fn whole() -> usize {
    static WHOLE: Mutex<Option<usize>> = Mutex::new(None);
    let mut held = WHOLE.lock().unwrap_or_else(PoisonError::into_inner);
    *held.get_or_insert_with(|| learn().unwrap_or(STATED))
}

/// What this process's log keeps whole of a line written as the driver writes one: entries
/// of a category of its own written and read back, by halves between the shortest (its
/// probe's words) and one past the stated cap, each length kept whole or not as its entry,
/// read back, says. None where an entry cannot be read back.
fn learn() -> Option<usize> {
    let category = format!("probe-{}", std::process::id());
    let log = Oslog::new(&category).ok()?;
    let mut nonce = [0u8; 8];
    shards_vmm::platform::fill_random(&mut nonce).ok()?;
    let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    let kept = |n: usize| -> Option<bool> {
        let prefix = format!("probe {nonce} {n} ");
        let line = format!("{prefix}{}", "x".repeat(n.saturating_sub(prefix.len())));
        let c = CString::new(line.clone()).ok()?;
        // SAFETY: the log this holds, and a NUL-terminated string that outlives the call.
        unsafe { shards_os_log(log.log.0, DEFAULT, c.as_ptr()) };
        read_back(&category, &prefix, line.len() + 64).map(|got| got == line)
    };
    let shortest = format!("probe {nonce} {STATED} ").len();
    let (mut lo, mut hi) = (shortest, STATED + 1);
    if !kept(lo)? {
        return None;
    }
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if kept(mid)? {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(lo)
}

/// The last entry of `category` beginning with `prefix` in this process's log store, read
/// back (oslog.c); none where none is, or the store cannot be read.
fn read_back(category: &str, prefix: &str, cap: usize) -> Option<String> {
    let (category, prefix) = (CString::new(category).ok()?, CString::new(prefix).ok()?);
    let mut buf = vec![0u8; cap];
    // An entry is in the log as os_log returns; it is read again once, should the store not
    // yet show it.
    for _ in 0..2 {
        // SAFETY: two NUL-terminated strings, and a buffer of `cap` bytes that the call
        // writes at most `cap` of.
        let n = unsafe {
            shards_os_log_read_back(
                category.as_ptr(),
                prefix.as_ptr(),
                buf.as_mut_ptr().cast(),
                c_long::try_from(cap).ok()?,
            )
        };
        if n >= 0 {
            let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            return String::from_utf8(buf.get(..end)?.to_vec()).ok();
        }
        if n == -2 {
            return None;
        }
        std::thread::yield_now();
    }
    None
}
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
    /// What os_log keeps of an entry whole, as this host's log shows ([`whole`]): its own
    /// parts of a line, cut where a character begins, are what the shipper gives it.
    fn part(&self) -> usize {
        whole()
    }

    fn send(&mut self, m: &Message<'_>, _wait: &Wait<'_>) -> io::Result<()> {
        let kind = if m.stream == LOG_STDERR { ERROR } else { DEFAULT };
        // Its bytes that are no UTF-8, and a NUL, which would end the C string, U+FFFD.
        let line = String::from_utf8_lossy(m.line).replace('\0', "\u{fffd}");
        for part in parts(&line, whole()) {
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
        const WHOLE: usize = 1015;
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

    /// Do: learn how long a line os_log keeps whole by reading this process's entries
    /// back; then write a line that long and one a byte longer, and read both back. Expect
    /// a length below the stated cap; the first kept whole, the second not: what the
    /// driver cuts at is what the log keeps, learned from the log alone.
    #[test]
    fn what_os_log_keeps_whole_is_learned_from_the_log() {
        let learned = learn().expect("this process's log entries read back");
        println!("os_log keeps {learned} bytes of a line whole");
        assert!(learned <= STATED, "{learned}");
        let log = Oslog::new("probe-check").unwrap();
        for (n, whole) in [(learned, true), (learned + 1, false)] {
            let prefix = format!("check {} {n} ", std::process::id());
            let line = format!("{prefix}{}", "y".repeat(n - prefix.len()));
            let c = CString::new(line.clone()).unwrap();
            // SAFETY: the log held above, and a NUL-terminated string that outlives the call.
            unsafe { shards_os_log(log.log.0, DEFAULT, c.as_ptr()) };
            let got = read_back("probe-check", &prefix, n + 64).unwrap();
            assert_eq!(got == line, whole, "{n}: {} bytes read back", got.len());
        }
    }
}
