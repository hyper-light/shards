//! The etwlogs driver's sending (Windows): each line an event of moby's ETW provider,
//! {a3693192-9ed6-46d2-a981-f8226c8363bd}, its message as moby's etwlogs writes one
//! (daemon/logger/etwlogs/etwlogs_windows.go, createLogMessage), EventWriteString of
//! `container_name: NAME, image_name: IMAGE, container_id: ID, image_id: IMAGE_ID, source:
//! STREAM, log: LINE` at level 0 and keyword 0, as moby's. An event written is ETW's: each
//! session that listens for the provider has it, and none that does not, delivered as soon
//! as it is written, for every sender alike. The provider is registered once for the
//! daemon's life, as moby counts its registrations while any of its loggers lives.

use std::io;
use std::sync::{Mutex, PoisonError};

use windows_sys::Win32::System::Diagnostics::Etw::{EventRegister, EventWriteString, REGHANDLE};
use windows_sys::core::GUID;

use super::{Driver, Message, Wait};
use crate::logging::Details;
use crate::spec::LOG_STDERR;

/// moby's etwlogs provider, which `logman start -ets NAME -p {GUID}` listens for.
pub(crate) const PROVIDER: GUID = GUID::from_u128(0xa3693192_9ed6_46d2_a981_f8226c8363bd);

pub(super) struct Etwlogs {
    /// Each event's words before its stream's: its container's and its image's names and
    /// IDs, as moby's logger.Info gives them.
    head: String,
}

impl Etwlogs {
    pub(super) fn new(details: &Details) -> io::Result<Etwlogs> {
        provider()?;
        Ok(Etwlogs {
            // Its name without inspect's leading slash, as moby's logger.Info.Name gives it.
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Each event's words are moby's createLogMessage's, a NUL in its line U+FFFD.
    #[test]
    fn events_say_what_mobys_do() {
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
}
