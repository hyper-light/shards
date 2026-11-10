//! One held journal writer, with byte authority supplied by its trusted producer.
//! No production producer exists yet: neither actor scratch nor a pathname is authority.
use std::fs::File;
use std::io::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Intent,
    Terminal,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    Exhausted {
        needed: u64,
        remaining: u64,
    },
    Closed,
    Append {
        stage: Stage,
        accepted: usize,
        expected: usize,
        code: Option<i32>,
    },
    Flush {
        stage: Stage,
        bytes: usize,
        code: Option<i32>,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    MemoryOnly,
    Reserved,
    IntentSynced,
    TerminalSynced,
    Failed(Failure),
}
enum State {
    Ready,
    Failed(Failure),
    Incomplete,
}

/// Fields are private and there is deliberately no production constructor. Enabling
/// this requires a producer proving exclusive writer/object protection, persistent
/// storage and its admitted byte extent, durable name creation, and restart ownership.
pub(crate) struct Journal {
    file: File,
    remaining: u64,
    state: State,
}
impl Journal {
    pub(super) fn reserve(&mut self, bytes: u64) -> Result<Reservation<'_>, Failure> {
        match self.state {
            State::Ready => {}
            State::Failed(error) => return Err(error),
            State::Incomplete => return Err(Failure::Closed),
        }
        self.remaining = self.remaining.checked_sub(bytes).ok_or(Failure::Exhausted {
            needed: bytes,
            remaining: self.remaining,
        })?;
        self.state = State::Incomplete;
        Ok(Reservation {
            journal: self,
            reserved: bytes,
            appended: 0,
            status: Status::Reserved,
        })
    }

    #[cfg(test)]
    pub(super) fn for_test(file: File, bytes: u64) -> Self {
        Self {
            file,
            remaining: bytes,
            state: State::Ready,
        }
    }
}

pub(super) struct Reservation<'a> {
    journal: &'a mut Journal,
    reserved: u64,
    appended: u64,
    status: Status,
}
impl Reservation<'_> {
    pub(super) fn intent(&mut self, bytes: &[u8]) -> Result<(), Failure> {
        if self.status != Status::Reserved {
            return Err(Failure::Closed);
        }
        self.append(Stage::Intent, bytes)?;
        self.status = Status::IntentSynced;
        Ok(())
    }
    pub(super) fn terminal(mut self, bytes: &[u8]) -> Result<Status, Failure> {
        match self.status {
            Status::IntentSynced => {}
            Status::Failed(error) => return Err(error),
            _ => return Err(Failure::Closed),
        }
        self.append(Stage::Terminal, bytes)?;
        self.status = Status::TerminalSynced;
        Ok(self.status)
    }
    fn fail(&mut self, error: Failure) -> Failure {
        self.status = Status::Failed(error);
        self.journal.state = State::Failed(error);
        error
    }
    fn append(&mut self, stage: Stage, bytes: &[u8]) -> Result<(), Failure> {
        let Some(available) = self.reserved.checked_sub(self.appended) else {
            return Err(self.fail(Failure::Closed));
        };
        let expected = bytes.len();
        if u64::try_from(expected).map_or(true, |n| n > available) || expected > isize::MAX as usize {
            return Err(self.fail(Failure::Closed));
        }
        // One attempt: a short append is an incomplete record, never an instruction
        // to retry an operation. Even EINTR closes admission conservatively.
        let accepted = match self.journal.file.write(bytes) {
            Ok(n) => n,
            Err(error) => {
                return Err(self.fail(Failure::Append {
                    stage,
                    accepted: 0,
                    expected,
                    code: error.raw_os_error(),
                }));
            }
        };
        self.appended = u64::try_from(accepted)
            .ok()
            .and_then(|n| self.appended.checked_add(n))
            .filter(|n| *n <= self.reserved)
            .ok_or_else(|| self.fail(Failure::Closed))?;
        if accepted != expected {
            return Err(self.fail(Failure::Append {
                stage,
                accepted,
                expected,
                code: None,
            }));
        }
        // Linux fdatasync includes size metadata needed to retrieve the appended
        // bytes. It does not establish the file's name or underlying storage promise.
        self.journal.file.sync_data().map_err(|error| {
            self.fail(Failure::Flush {
                stage,
                bytes: accepted,
                code: error.raw_os_error(),
            })
        })
    }
}
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        match self.status {
            Status::Reserved | Status::TerminalSynced => {
                // Both amounts derive from the same checked subtraction in reserve.
                match self
                    .reserved
                    .checked_sub(self.appended)
                    .and_then(|n| self.journal.remaining.checked_add(n))
                {
                    Some(remaining) => {
                        self.journal.remaining = remaining;
                        self.journal.state = State::Ready;
                    }
                    None => self.journal.state = State::Incomplete,
                }
            }
            Status::IntentSynced => self.journal.state = State::Incomplete,
            Status::Failed(_) | Status::MemoryOnly => {}
        }
    }
}

impl Status {
    pub(super) fn write(self, out: &mut impl std::fmt::Write) -> std::fmt::Result {
        out.write_str("{\"kind\":")?;
        super::quoted(
            out,
            match self {
                Self::MemoryOnly => "memory_only",
                Self::Reserved => "reserved",
                Self::IntentSynced => "intent_synced",
                Self::TerminalSynced => "terminal_synced",
                Self::Failed(_) => "failed",
            },
        )?;
        if let Self::Failed(error) = self {
            out.write_str(",\"cause\":")?;
            error.write(out)?;
        }
        out.write_str("}")
    }
}
impl Failure {
    pub(super) fn write(self, out: &mut impl std::fmt::Write) -> std::fmt::Result {
        out.write_str("{\"kind\":")?;
        super::quoted(
            out,
            match self {
                Self::Exhausted { .. } => "audit_exhausted",
                Self::Closed => "audit_closed",
                Self::Append { .. } => "audit_append",
                Self::Flush { .. } => "audit_flush",
            },
        )?;
        match self {
            Self::Exhausted { needed, remaining } => {
                write!(out, ",\"needed\":{needed},\"remaining\":{remaining}")?
            }
            Self::Closed => {}
            Self::Append {
                stage,
                accepted,
                expected,
                code,
            } => {
                write_stage(out, stage)?;
                write!(out, ",\"accepted\":{accepted},\"expected\":{expected}")?;
                write_code(out, code)?;
            }
            Self::Flush { stage, bytes, code } => {
                write_stage(out, stage)?;
                write!(out, ",\"bytes\":{bytes}")?;
                write_code(out, code)?;
            }
        }
        out.write_str("}")
    }
}

fn write_stage(out: &mut impl std::fmt::Write, stage: Stage) -> std::fmt::Result {
    out.write_str(",\"stage\":")?;
    super::quoted(
        out,
        match stage {
            Stage::Intent => "intent",
            Stage::Terminal => "terminal",
        },
    )
}
fn write_code(out: &mut impl std::fmt::Write, code: Option<i32>) -> std::fmt::Result {
    out.write_str(",\"os_code\":")?;
    match code {
        Some(code) => write!(out, "{code}"),
        None => out.write_str("null"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

    #[test]
    fn broker_audit_native_partial_write_closes_instead_of_retrying() {
        let mut fds = [-1; 2];
        assert_eq!(
            // SAFETY: pipe2 writes two descriptors, owned below on success.
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
            0
        );
        // SAFETY: transfer both newly created descriptors exactly once.
        let (_reader, writer) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        // SAFETY: query the held pipe's actual capacity without changing its limit.
        let capacity = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETPIPE_SZ) };
        let expected = usize::try_from(capacity).unwrap().checked_add(1).unwrap();
        let bytes = vec![b'x'; expected];
        // Invalid producer input exercises a real positive short write, not a fake
        // durable sink or a claimed regular-file/storage exhaustion result.
        let mut journal = Journal::for_test(File::from(writer), u64::try_from(expected).unwrap());
        let error = journal
            .reserve(u64::try_from(expected).unwrap())
            .unwrap()
            .intent(&bytes)
            .unwrap_err();
        assert!(
            matches!(error, Failure::Append { stage: Stage::Intent, accepted, expected: n, code: None }
            if accepted > 0 && accepted < n && n == expected)
        );
        assert_eq!(journal.reserve(0).err(), Some(error));
    }

    #[test]
    fn broker_audit_native_sync_error_is_not_a_durable_receipt() {
        let mut fds = [-1; 2];
        // SAFETY: pipe2 writes the two supplied descriptors, owned below on success.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: both new descriptors are transferred exactly once.
        let (_reader, writer) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        // Deliberately invalid producer input: actual write succeeds, actual sync fails.
        let mut journal = Journal::for_test(File::from(writer), 2);
        let error = journal.reserve(2).unwrap().intent(b"x").unwrap_err();
        assert_eq!(
            error,
            Failure::Flush {
                stage: Stage::Intent,
                bytes: 1,
                code: Some(libc::EINVAL)
            }
        );
        assert_eq!(journal.reserve(1).err(), Some(error));
    }

    #[test]
    fn broker_audit_unused_reservations_return_but_unfinished_intents_close() {
        let tmp = shards_testdir::TempDir::new("broker-journal").unwrap();
        let file = File::create(tmp.join("journal")).unwrap();
        let mut journal = Journal::for_test(file, 4);
        drop(journal.reserve(4).unwrap());
        let mut reserved = journal.reserve(4).unwrap();
        reserved.intent(b"i").unwrap();
        assert_eq!(reserved.terminal(b"t").unwrap(), Status::TerminalSynced);
        let mut reserved = journal.reserve(2).unwrap();
        reserved.intent(b"i").unwrap();
        drop(reserved);
        assert_eq!(journal.reserve(0).err(), Some(Failure::Closed));
        assert_eq!(std::fs::read(tmp.join("journal")).unwrap(), b"iti");
    }
}
