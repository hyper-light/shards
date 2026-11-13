//! What a host saw change in a directory it shares, told to its guest (D235): records of
//! `NAME\0PATH\0`, NAME the share's (its virtio-fs tag where the guest reads them, the
//! share's index where its share process says them), PATH the file's within the share,
//! its components joined by `/`, empty for the share's root. Neither holds a NUL; PATH's
//! components are plain, never empty, `.` or `..`, so that no record names anything
//! outside its share.
//!
//! How many records wait at once is the guest's to say ([`QUEUE_XATTR`]): as many touches
//! as one of its inotify instances holds the events of ([`most_waiting`]).

use alloc::vec::Vec;

/// The attribute init's notice built-in sets on each of its shares' roots as it starts, as
/// [`NOTICE_ID`](crate::NOTICE_ID) (D235): its value, in decimal, the guest kernel's
/// `fs.inotify.max_queued_events`, the events each inotify instance made since holds before
/// it tells its reader only that it lost some (fs/notify/inotify/inotify_user.c,
/// `inotify_new_group`; fs/notify/notification.c, `fsnotify_insert_event`). Its server takes
/// it of the root alone and keeps it on no file; its share process then bounds what waits to
/// be told by it ([`most_waiting`]).
pub const QUEUE_XATTR: &str = "trusted.shards.notice-queue";

/// The events a touch of the guest's is told as to one inotify instance, at most: once to
/// its directory's watch, with its name, and once to its own (fs/notify/fsnotify.c,
/// `fsnotify_handle_event`: "report the event once to parent dir with name ... and once to
/// child without name").
pub const EVENTS_A_TOUCH: u32 = 2;

/// How many of the host's changes may wait to be told together, for a guest whose inotify
/// instances hold `queue` events each: the touches whose events one instance holds whole,
/// [`EVENTS_A_TOUCH`] each. Past them, a change is told by a directory, which a watcher
/// reads again whole, rather than lost to its queue's overflow.
pub fn most_waiting(queue: u32) -> u32 {
    queue / EVENTS_A_TOUCH
}

/// Why a [`QUEUE_XATTR`] value is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueRefusal {
    /// It is not decimal digits alone, at least one.
    NotDecimal,
    /// It is past what the sysctl holds, a C `int` (`proc_dointvec_minmax`).
    PastInt,
}

impl core::fmt::Display for QueueRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            QueueRefusal::NotDecimal => "a queue's length that is not decimal digits",
            QueueRefusal::PastInt => "a queue's length past what fs.inotify.max_queued_events holds",
        })
    }
}

/// `queue` as [`QUEUE_XATTR`]'s value.
pub fn queue_word(queue: u32) -> Vec<u8> {
    let mut digits = Vec::new();
    let mut rest = queue;
    loop {
        // A decimal digit: below 10, so one byte.
        digits.push(b'0' + (rest % 10) as u8);
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    digits.reverse();
    digits
}

/// A [`QUEUE_XATTR`] value's queue length: decimal digits alone, within a C `int`, as the
/// sysctl it is read from holds (fs/notify/inotify/inotify_user.c, `max_queued_events`, at
/// least 0); a trailing newline, as procfs writes one, taken off.
pub fn parse_queue(value: &[u8]) -> Result<u32, QueueRefusal> {
    let digits = value.strip_suffix(b"\n").unwrap_or(value);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(QueueRefusal::NotDecimal);
    }
    let mut queue: u32 = 0;
    for d in digits {
        queue = queue
            .checked_mul(10)
            .and_then(|q| q.checked_add(u32::from(d - b'0')))
            .ok_or(QueueRefusal::PastInt)?;
    }
    if queue > i32::MAX as u32 {
        return Err(QueueRefusal::PastInt);
    }
    Ok(queue)
}

/// `name` and `path` as a record, appended to `out`; `false`, with nothing appended, where
/// either is not one a record holds.
pub fn encode(name: &[u8], path: &[u8], out: &mut Vec<u8>) -> bool {
    if name.is_empty() || name.contains(&0) || !plain(path) {
        return false;
    }
    out.extend_from_slice(name);
    out.push(0);
    out.extend_from_slice(path);
    out.push(0);
    true
}

/// What the start of a stream of records holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoded<'a> {
    /// A whole record, and the bytes past it.
    Record {
        name: &'a [u8],
        path: &'a [u8],
        rest: &'a [u8],
    },
    /// Part of one: the rest is to come.
    Partial,
    /// Not one a record holds: the stream is not records.
    Malformed,
}

/// The first record of `bytes`.
pub fn decode(bytes: &[u8]) -> Decoded<'_> {
    let mut parts = bytes.splitn(3, |&b| b == 0);
    let (Some(name), Some(path), Some(rest)) = (parts.next(), parts.next(), parts.next()) else {
        return Decoded::Partial;
    };
    if name.is_empty() || !plain(path) {
        return Decoded::Malformed;
    }
    Decoded::Record { name, path, rest }
}

/// Whether `path` is one a record holds: empty, or components joined by `/`, none empty,
/// `.` or `..`, and no NUL.
pub fn plain(path: &[u8]) -> bool {
    path.is_empty()
        || (!path.contains(&0)
            && path
                .split(|&b| b == b'/')
                .all(|c| !matches!(c, b"" | b"." | b"..")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_read_as_written_and_nothing_outside_a_share_is_one() {
        let mut out = Vec::new();
        assert!(encode(b"shards0", b"src/main.rs", &mut out));
        assert!(encode(b"shards-plug1", b"", &mut out));
        let Decoded::Record { name, path, rest } = decode(&out) else {
            panic!("{out:?}");
        };
        assert_eq!((name, path), (&b"shards0"[..], &b"src/main.rs"[..]));
        assert_eq!(
            decode(rest),
            Decoded::Record {
                name: b"shards-plug1",
                path: b"",
                rest: b""
            }
        );
        // Part of one is none yet.
        assert_eq!(decode(b"shards0\0src"), Decoded::Partial);
        assert_eq!(decode(b"shards0"), Decoded::Partial);
        assert_eq!(decode(b""), Decoded::Partial);
        for bad in [&b"../x"[..], b"a/../b", b"/a", b"a//b", b"a/", b".", b"a/./b"] {
            let mut none = Vec::new();
            assert!(!encode(b"t", bad, &mut none), "{bad:?}");
            assert!(none.is_empty());
            let mut record = b"t\0".to_vec();
            record.extend_from_slice(bad);
            record.push(0);
            assert_eq!(decode(&record), Decoded::Malformed, "{bad:?}");
        }
        assert!(!encode(b"", b"a", &mut Vec::new()));
        assert!(!encode(b"t\0u", b"a", &mut Vec::new()));
        assert_eq!(decode(b"\0a\0"), Decoded::Malformed);
    }

    /// A queue's length is read as written, procfs's newline taken off, and nothing else
    /// is one: no sign, no space, nothing past a C `int`. What waits is the touches whose
    /// events one instance holds whole.
    #[test]
    fn a_queue_is_read_as_written_and_bounds_what_waits() {
        for q in [0, 1, 9, 10, 16384, i32::MAX as u32] {
            assert_eq!(parse_queue(&queue_word(q)), Ok(q), "{q}");
        }
        assert_eq!(queue_word(16384), b"16384");
        assert_eq!(parse_queue(b"16384\n"), Ok(16384));
        for bad in [&b""[..], b"\n", b"-1", b"+1", b" 1", b"1 ", b"0x10", b"1\n\n"] {
            assert_eq!(parse_queue(bad), Err(QueueRefusal::NotDecimal), "{bad:?}");
        }
        for past in [&b"2147483648"[..], b"4294967296", b"99999999999999999999"] {
            assert_eq!(parse_queue(past), Err(QueueRefusal::PastInt), "{past:?}");
        }
        assert_eq!(most_waiting(16384), 8192);
        assert_eq!(most_waiting(1), 0);
        assert_eq!(most_waiting(0), 0);
    }
}
