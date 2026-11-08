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
/// The most of an event's string, its NUL among it, in bytes, that a session of logman's
/// default buffers (8 KB) is given: one byte more and EventWriteString says ERROR_MORE_DATA,
/// the event lost to that session (PM M492, Windows 11 26200). A session of 128 KB buffers
/// takes 65,448 bytes, ETW's own bound, its 64 KB less an event's header, past which no
/// session has it (ERROR_ARITHMETIC_OVERFLOW). Moby's consumers' documented session is
/// logman's default (`logman start -ets DockerContainerLogs -p {GUID} 0 0 -o trace.etl`),
/// to which a 16 KiB part of moby's is lost whole.
const EVENT_MOST: usize = 8040;
/// The most bytes of UTF-8 one UTF-16 unit of an event spells: a character of three bytes
/// is one unit, one of four two units, one of one or two bytes a unit (and a byte that is
/// no UTF-8 U+FFFD's one unit).
const UTF8_PER_UNIT: usize = 3;

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
    fn event(&self, m: &Message<'_>) -> Vec<u16> {
        let source = if m.stream == LOG_STDERR {
            "stderr"
        } else {
            "stdout"
        };
        let line = String::from_utf8_lossy(m.line).replace('\0', "\u{fffd}");
        format!("{}source: {source}, log: {line}", self.head)
            .encode_utf16()
            .chain([0])
            .collect()
    }
}

impl Driver for Etwlogs {
    /// The most of a line's bytes an event within [`EVENT_MOST`] can spell, its every
    /// character three bytes of UTF-8 and one unit of UTF-16: what fits is asked of each
    /// event as it is written ([`Driver::fits`]).
    fn part(&self) -> usize {
        EVENT_MOST / 2 * UTF8_PER_UNIT
    }

    fn fits(&mut self, m: &Message<'_>) -> io::Result<bool> {
        Ok(self.event(m).len() * 2 <= EVENT_MOST)
    }

    fn send(&mut self, m: &Message<'_>, _wait: &Wait<'_>) -> io::Result<()> {
        let handle = provider()?;
        let event = self.event(m);
        // SAFETY: the provider's registration, which lives as long as the daemon, and a
        // NUL-terminated wide string that outlives the call.
        match unsafe { EventWriteString(handle, 0, 0, event.as_ptr()) } {
            0 => Ok(()),
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
            let event = etw.event(&Message {
                stream,
                at: 0,
                line,
                part: None,
            });
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

    /// Do: write events whose strings are each length around [`EVENT_MOST`], while a
    /// session of logman's default buffers listens for a provider of the test's own, as the
    /// etwlogs driver writes them (EventWriteString). Expect the most the driver lets an
    /// event be taken, and a unit more refused (PM M492's basis on this host). Skips where
    /// no session can be started (one is an administrator's or Performance Log Users').
    #[test]
    fn an_event_as_long_as_the_driver_lets_one_be_reaches_a_default_session() {
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
        // logman's default buffers, the basis: 8 KB.
        let query = std::process::Command::new("logman")
            .args(["query", &name, "-ets"])
            .output()
            .unwrap();
        let query = String::from_utf8_lossy(&query.stdout).into_owned();
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
        let (most, more) = (write(EVENT_MOST), write(EVENT_MOST + 2));
        // SAFETY: the registration made above, once.
        unsafe { EventUnregister(handle) };
        let _ = std::process::Command::new("logman")
            .args(["stop", &name, "-ets"])
            .output();
        let buffers = query.lines().find(|l| l.starts_with("Buffer Size:"));
        assert_eq!(
            buffers.map(|l| l.split_whitespace().last()),
            Some(Some("8")),
            "{query}"
        );
        assert_eq!((most, more), (0, 234), "ERROR_MORE_DATA past {EVENT_MOST} bytes");
    }
}
