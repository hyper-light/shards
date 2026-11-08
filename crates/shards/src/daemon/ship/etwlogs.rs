//! The etwlogs driver's sending (Windows): each line an event of moby's ETW provider,
//! {a3693192-9ed6-46d2-a981-f8226c8363bd}, its message as moby's etwlogs writes one
//! (daemon/logger/etwlogs/etwlogs_windows.go, createLogMessage), EventWriteString of
//! `container_name: NAME, image_name: IMAGE, container_id: ID, image_id: IMAGE_ID, source:
//! STREAM, log: LINE` at level 0 and keyword 0, what its consumers parse (D198). An event
//! written is ETW's: each
//! session that listens for the provider has it, and none that does not, delivered as soon
//! as it is written, for every sender alike. The provider is registered once for the
//! daemon's life, so that no run's start or end registers or unregisters it (D198).

use std::io;
use std::sync::{Mutex, PoisonError};

use windows_sys::Win32::System::Diagnostics::Etw::{EventRegister, EventWriteString, REGHANDLE};
use windows_sys::core::GUID;

use super::{Driver, Message, Wait};
use crate::logging::Details;
use crate::spec::LOG_STDERR;

/// moby's etwlogs provider, which `logman start -ets NAME -p {GUID}` listens for.
pub(crate) const PROVIDER: GUID = GUID::from_u128(0xa3693192_9ed6_46d2_a981_f8226c8363bd);
/// The most bytes of UTF-8 one UTF-16 unit of an event spells: a character of three bytes
/// is one unit, one of four two units, one of one or two bytes a unit (and a byte that is
/// no UTF-8 U+FFFD's one unit).
const UTF8_PER_UNIT: usize = 3;
/// ERROR_MORE_DATA, which EventWriteString says of an event too long for a session's
/// buffers, that session not given it; and ERROR_ARITHMETIC_OVERFLOW, of one past ETW's own
/// bound, no session given it (EventWriteString, Microsoft Learn).
const SOME_REFUSED: u32 = 234;
const ALL_REFUSED: u32 = 534;

/// The most of an event's string, in bytes, its NUL among it, that the sessions listening
/// take, as their answers show it while shards runs (D163). ETW states its own bound, "the
/// total event size ... must be less than 64KB" with its header, and not a session's, which
/// its buffers set: an event too long for one is refused it (ERROR_MORE_DATA). So the first
/// try is ETW's stated bound, 64 KiB less an EVENT_HEADER; then each answer narrows it, the
/// longest event every session took and the shortest one refused, the next try between
/// them: within a handful of long events it is the smallest listening session's (PM M492
/// found 8,040 bytes for logman's default 8 KB buffers, 65,448 for 128 KB). A session
/// started later that refuses what all took starts it again. Kept for the daemon's life, as
/// the sessions are the host's.
#[derive(Debug, Default)]
struct Learned {
    /// The longest event every session took, since the shortest refused was found.
    took: usize,
    /// The shortest event a session refused.
    refused: Option<usize>,
}

impl Learned {
    /// ETW's own bound: 64 KiB less an event's header.
    fn stated() -> usize {
        (64 << 10) - std::mem::size_of::<windows_sys::Win32::System::Diagnostics::Etw::EVENT_HEADER>()
    }

    /// The longest event to try next, in bytes, a whole number of UTF-16 units.
    fn most(&self) -> usize {
        let most = match self.refused {
            None => Self::stated(),
            Some(r) if r <= self.took + 2 => self.took,
            Some(r) => self.took + (r - self.took) / 2,
        };
        (most & !1).max(4)
    }

    /// What an event of `bytes` was answered.
    fn answered(&mut self, bytes: usize, refused: bool) {
        if refused {
            if bytes <= self.took {
                // A session that refuses what all took: start again below it.
                self.took = 0;
            }
            self.refused = Some(self.refused.map_or(bytes, |r| r.min(bytes)));
        } else if self.refused.is_some_and(|r| bytes < r) {
            self.took = self.took.max(bytes);
        }
    }
}

static LEARNED: Mutex<Learned> = Mutex::new(Learned {
    took: 0,
    refused: None,
});

fn learned() -> std::sync::MutexGuard<'static, Learned> {
    LEARNED.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(super) struct Etwlogs {
    /// Each event's words before its stream's: its microVM's and its image's names and
    /// IDs, the fields its consumers parse (D198).
    head: String,
}

impl Etwlogs {
    pub(super) fn new(details: &Details) -> io::Result<Etwlogs> {
        provider()?;
        Ok(Etwlogs {
            // Its name without inspect's leading slash, as its consumers read it (D198).
            head: format!(
                "container_name: {}, image_name: {}, container_id: {}, image_id: {}, ",
                details.name.trim_start_matches('/'),
                details.image_name,
                details.id,
                details.image_id
            ),
        })
    }

    /// The event `m` is: its words in UTF-16, NUL-terminated, as EventWriteString takes
    /// them. A NUL in its line, which moby's refuses with the line (Go's UTF16FromString)
    /// and so loses it, is U+FFFD here, as its bytes that are no UTF-8 are: the line is
    /// sent, and none of it cut short.
    fn event(&self, stream: u8, line: &[u8]) -> Vec<u16> {
        let source = if stream == LOG_STDERR { "stderr" } else { "stdout" };
        let line = String::from_utf8_lossy(line).replace('\0', "\u{fffd}");
        format!("{}source: {source}, log: {line}", self.head)
            .encode_utf16()
            .chain([0])
            .collect()
    }
}

impl Driver for Etwlogs {
    /// The most of a line's bytes an event within ETW's stated bound can spell, its every
    /// character three bytes of UTF-8 and one unit of UTF-16: what fits the sessions is
    /// asked of each event as it is written ([`Driver::fits`]).
    fn part(&self) -> usize {
        Learned::stated() / 2 * UTF8_PER_UNIT
    }

    fn fits(&mut self, m: &Message<'_>) -> io::Result<bool> {
        Ok(self.event(m.stream, m.line).len() * 2 <= learned().most())
    }

    fn send(&mut self, m: &Message<'_>, _wait: &Wait<'_>) -> io::Result<()> {
        self.send_line(m.stream, m.line)
    }
}

impl Etwlogs {
    /// Writes `line` as an event; where no session took it as too long (or the sessions'
    /// bound has narrowed since it was cut), in halves, each cut at a character's boundary,
    /// as ETW's events carry no part's place to join them by. One a session refused that
    /// others took is theirs: written again, they would have it twice.
    fn send_line(&self, stream: u8, line: &[u8]) -> io::Result<()> {
        let handle = provider()?;
        let event = self.event(stream, line);
        let bytes = event.len() * 2;
        let answer = if bytes > learned().most() {
            ALL_REFUSED
        } else {
            // SAFETY: the provider's registration, which lives as long as the daemon, and
            // a NUL-terminated wide string that outlives the call.
            let answer = unsafe { EventWriteString(handle, 0, 0, event.as_ptr()) };
            if matches!(answer, 0 | SOME_REFUSED | ALL_REFUSED) {
                learned().answered(bytes, answer != 0);
            }
            answer
        };
        match answer {
            0 | SOME_REFUSED => Ok(()),
            ALL_REFUSED if line.len() > 1 => {
                let half = super::boundary(line, line.len() / 2).max(1);
                let (a, b) = line.split_at(half.min(line.len()));
                self.send_line(stream, a)?;
                self.send_line(stream, b)
            }
            e => Err(io::Error::from_raw_os_error(i32::try_from(e).unwrap_or(i32::MAX))),
        }
    }
}

/// The provider's registration (EventRegister), made by the first etwlogs logger and kept
/// for the daemon's life: one that failed is asked for again by the next.
fn provider() -> io::Result<REGHANDLE> {
    static REGISTERED: Mutex<Option<REGHANDLE>> = Mutex::new(None);
    let mut held = REGISTERED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(handle) = *held {
        return Ok(handle);
    }
    let mut handle: REGHANDLE = 0;
    // SAFETY: the provider's GUID, no callback, and a handle for the call to fill.
    match unsafe { EventRegister(&PROVIDER, None, std::ptr::null(), &raw mut handle) } {
        0 => {
            *held = Some(handle);
            Ok(handle)
        }
        e => Err(io::Error::from_raw_os_error(i32::try_from(e).unwrap_or(i32::MAX))),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Each event's words are moby's createLogMessage's, a NUL in its line U+FFFD.
    #[test]
    fn events_carry_a_runs_line_and_details_as_etw_readers_parse_them() {
        let details = Details {
            id: "0123456789ab".repeat(5) + "cdef",
            name: "/web".into(),
            image_id: "sha256:".to_string() + &"a".repeat(64),
            image_name: "alpine:3".into(),
            entrypoint: String::new(),
            args: Vec::new(),
            env: Vec::new(),
            labels: std::collections::BTreeMap::new(),
            hostname: String::new(),
            created: String::new(),
        };
        let etw = Etwlogs {
            head: Etwlogs::new(&details).unwrap().head,
        };
        let said = |stream, line: &[u8]| {
            let event = etw.event(stream, line);
            String::from_utf16(event.strip_suffix(&[0]).unwrap()).unwrap()
        };
        assert_eq!(
            said(crate::spec::LOG_STDOUT, b"hello"),
            format!(
                "container_name: web, image_name: alpine:3, container_id: {}, image_id: {}, source: stdout, log: hello",
                details.id, details.image_id
            )
        );
        assert!(said(LOG_STDERR, b"a\0b").ends_with("source: stderr, log: a\u{fffd}b"));
    }

    /// Do: answer the learner as a session that takes 8,040 bytes of string, then as one
    /// started later that takes 4,000. Expect its first try ETW's stated bound, then each
    /// try narrowed to exactly what the session takes within a handful of refusals; then
    /// started again below the later session's and narrowed to it.
    #[test]
    fn the_sessions_bound_is_learned_from_their_answers() {
        let mut l = Learned::default();
        assert_eq!(l.most(), (64 << 10) - 80);
        let session = |l: &mut Learned, takes: usize| {
            let mut refusals = 0;
            for _ in 0..64 {
                let n = l.most();
                l.answered(n, n > takes);
                refusals += usize::from(n > takes);
            }
            refusals
        };
        let refused = session(&mut l, 8040);
        assert_eq!(l.most(), 8040);
        assert!(refused <= 16, "{refused}");
        session(&mut l, 4000);
        assert_eq!(l.most(), 4000);
    }

    /// Do: while a session of logman's default buffers listens for a provider of the test's
    /// own, write events of the learner's next try, each answered as ETW answers it, until
    /// the try holds. Expect the try then taken and a unit more refused: what the session
    /// takes, learned from its answers alone. Skips where no session can be started (one is
    /// an administrator's or Performance Log Users').
    #[test]
    fn a_sessions_bound_is_learned_from_a_real_sessions_answers() {
        use windows_sys::Win32::System::Diagnostics::Etw::EventUnregister;
        let id = 0x5a4d_0000_4e00_0000_0000_0000_0000_0000 | u128::from(std::process::id());
        let hex = format!("{id:032x}");
        let guid = format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        );
        let name = format!("shards-tests-etw-{}", std::process::id());
        let dir = shards_testdir::TempDir::new("etw").unwrap();
        let etl = dir.path().join("t.etl");
        let started = std::process::Command::new("logman")
            .args(["start", &name, "-p", &format!("{{{guid}}}"), "-o"])
            .arg(&etl)
            .arg("-ets")
            .output()
            .unwrap();
        if !started.status.success() {
            println!("SKIP: logman start: {}", String::from_utf8_lossy(&started.stdout));
            return;
        }
        let provider = GUID::from_u128(id);
        let mut handle: REGHANDLE = 0;
        // SAFETY: a GUID, no callback, and a handle for the call to fill.
        assert_eq!(
            unsafe { EventRegister(&provider, None, std::ptr::null(), &raw mut handle) },
            0
        );
        let write = |bytes: usize| {
            let mut s: Vec<u16> = vec![u16::from(b'x'); bytes / 2 - 1];
            s.push(0);
            // SAFETY: the registration made above and a NUL-terminated wide string.
            unsafe { EventWriteString(handle, 0, 0, s.as_ptr()) }
        };
        let mut l = Learned::default();
        for _ in 0..64 {
            let n = l.most();
            let answer = write(n);
            assert!(matches!(answer, 0 | SOME_REFUSED | ALL_REFUSED), "{answer}");
            l.answered(n, answer != 0);
        }
        let most = l.most();
        let (taken, more) = (write(most), write(most + 2));
        // SAFETY: the registration made above, once.
        unsafe { EventUnregister(handle) };
        let _ = std::process::Command::new("logman")
            .args(["stop", &name, "-ets"])
            .output();
        println!("a default session's events: at most {most} bytes of string");
        assert_eq!((taken, more), (0, SOME_REFUSED), "{most}");
    }
}
