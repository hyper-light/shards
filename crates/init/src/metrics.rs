//! On-demand cgroup observations, read from supervisor-held objects.
//!
//! Linux cgroup-v2.rst defines cpu.stat microseconds, memory.current bytes,
//! pids.current subtree TIDs and io.stat's per-device cumulative bytes. These
//! observations confer no authority and do not reuse Docker STATS' zero defaults.

mod events;
pub mod otlp;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;

use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _};

use otlp::{Failure, IoBytes, Scope, Snapshot};

const DECIMAL: usize = u64::MAX.ilog10() as usize + 1;
// The longest selected token is "wbytes=" followed by a u64. Unknown kernel
// fields need no retained text and do not enlarge this numeric grammar bound.
const TOKEN_BYTES: usize = b"wbytes=".len() + DECIMAL;

enum Token<'a> {
    Word(&'a [u8]),
    Long(&'a [u8]),
    Line,
}

pub use otlp::Limit;

/// One actual kernel page, reused across every file and scope in a request.
pub struct Reader {
    bytes: Vec<u8>,
}

impl Reader {
    pub fn new() -> io::Result<Self> {
        // SAFETY: sysconf queries the running kernel, with no pointers.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page = usize::try_from(page)
            .ok()
            .filter(|&n| n != 0)
            .ok_or(io::ErrorKind::InvalidData)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(page)
            .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
        // Capacity for the entire resize was reserved fallibly above.
        bytes.resize(page, 0);
        Ok(Self { bytes })
    }

    fn tokens(
        &mut self,
        group: &File,
        name: &'static CStr,
        mut take: impl FnMut(Token<'_>) -> Result<(), Failure>,
    ) -> Result<(), Failure> {
        // SAFETY: a fixed control name relative to the supervisor-held directory.
        let fd = unsafe {
            libc::openat(
                group.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(native(io::Error::last_os_error()));
        }
        // SAFETY: openat returned a fresh descriptor owned here alone.
        let mut file = unsafe { File::from_raw_fd(fd) };
        let mut word = [0u8; TOKEN_BYTES];
        let (mut used, mut long) = (0, false);
        loop {
            let n = match file.read(&mut self.bytes) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                result => result.map_err(native)?,
            };
            if n == 0 {
                if used != 0 {
                    let text = word.get(..used).ok_or(Failure::Invalid)?;
                    take(if long {
                        Token::Long(text)
                    } else {
                        Token::Word(text)
                    })?;
                }
                return take(Token::Line);
            }
            for &byte in self.bytes.get(..n).ok_or(Failure::Invalid)? {
                if byte.is_ascii_whitespace() {
                    if used != 0 {
                        let text = word.get(..used).ok_or(Failure::Invalid)?;
                        take(if long {
                            Token::Long(text)
                        } else {
                            Token::Word(text)
                        })?;
                        (used, long) = (0, false);
                    }
                    if byte == b'\n' {
                        take(Token::Line)?;
                    }
                } else if let Some(slot) = word.get_mut(used) {
                    *slot = byte;
                    used += 1;
                } else {
                    long = true;
                }
            }
        }
    }

    fn scalar(&mut self, group: &File, name: &'static CStr) -> Result<u64, Failure> {
        let mut value = None;
        self.tokens(group, name, |token| {
            match token {
                Token::Word(word) if value.is_none() => value = Some(number(word)?),
                Token::Word(_) => return Err(Failure::Invalid),
                Token::Long(_) => return Err(Failure::Overflow),
                Token::Line => {}
            }
            Ok(())
        })?;
        value.ok_or(Failure::Missing)
    }

    pub fn memory_limit(&mut self, group: &File) -> Result<Limit, Failure> {
        self.limit(group, c"memory.max")
    }

    /// Task/TID ceiling, including threads, not a count of process leaders.
    pub fn task_limit(&mut self, group: &File) -> Result<Limit, Failure> {
        self.limit(group, c"pids.max")
    }

    fn limit(&mut self, group: &File, name: &'static CStr) -> Result<Limit, Failure> {
        let mut value = None;
        self.tokens(group, name, |token| {
            match token {
                Token::Word(word) if value.is_none() => {
                    value = Some(match word {
                        b"max" => Limit::Unbounded,
                        digits => Limit::Finite(number(digits)?),
                    });
                }
                Token::Word(_) => return Err(Failure::Invalid),
                Token::Long(_) => return Err(Failure::Overflow),
                Token::Line => {}
            }
            Ok(())
        })?;
        value.ok_or(Failure::Missing)
    }

    fn cpu(&mut self, group: &File) -> Result<u64, Failure> {
        enum State {
            Key,
            Usage,
            Other,
        }
        let (mut state, mut value) = (State::Key, None);
        self.tokens(group, c"cpu.stat", |token| {
            match (&state, token) {
                (State::Key, Token::Word(b"usage_usec")) => state = State::Usage,
                (State::Key, Token::Word(_) | Token::Long(_)) => state = State::Other,
                (State::Usage, Token::Word(word)) if value.is_none() => {
                    value = Some(number(word)?);
                    state = State::Other;
                }
                (State::Usage, Token::Long(_)) => return Err(Failure::Overflow),
                (State::Usage, _) => return Err(Failure::Invalid),
                (_, Token::Line) => state = State::Key,
                _ => {}
            }
            Ok(())
        })?;
        value.ok_or(Failure::Missing)
    }

    fn io(&mut self, group: &File) -> Result<(u64, u64), Failure> {
        enum State {
            Device,
            Fields { read: Option<u64>, write: Option<u64> },
        }
        let (mut state, mut read, mut write) = (State::Device, 0u64, 0u64);
        self.tokens(group, c"io.stat", |token| {
            match (&mut state, token) {
                (State::Device, Token::Word(word)) => {
                    let Some(colon) = word.iter().position(|&b| b == b':') else {
                        return Err(Failure::Invalid);
                    };
                    number(word.get(..colon).ok_or(Failure::Invalid)?)?;
                    number(word.get(colon + 1..).ok_or(Failure::Invalid)?)?;
                    state = State::Fields {
                        read: None,
                        write: None,
                    };
                }
                (State::Fields { read, write }, Token::Word(word)) => {
                    let field = if let Some(n) = word.strip_prefix(b"rbytes=") {
                        Some((read, n))
                    } else {
                        word.strip_prefix(b"wbytes=").map(|n| (write, n))
                    };
                    if let Some((slot, digits)) = field {
                        if slot.is_some() {
                            return Err(Failure::Invalid);
                        }
                        *slot = Some(number(digits)?);
                    }
                }
                (State::Fields { read: r, write: w }, Token::Line) => {
                    // Linux prints a device's byte fields together, or neither where its
                    // blkg has done no read or write (v6.18 block/blk-cgroup.c 1206-1210).
                    let (r, w) = match (*r, *w) {
                        (Some(r), Some(w)) => (r, w),
                        (None, None) => (0, 0),
                        _ => return Err(Failure::Missing),
                    };
                    read = read.checked_add(r).ok_or(Failure::Overflow)?;
                    write = write.checked_add(w).ok_or(Failure::Overflow)?;
                    state = State::Device;
                }
                (_, Token::Long(prefix))
                    if prefix.starts_with(b"rbytes=") || prefix.starts_with(b"wbytes=") =>
                {
                    return Err(Failure::Overflow);
                }
                (State::Device, Token::Long(_)) => return Err(Failure::Invalid),
                _ => {}
            }
            Ok(())
        })?;
        Ok((read, write))
    }

    /// Whether the cgroup's parent enables `controller` for it, which is what its
    /// cgroup.controllers lists (Linux v6.18 cgroup-v2.rst 935-940).
    fn enabled(&mut self, group: &File, controller: &[u8]) -> Result<bool, Failure> {
        let mut listed = false;
        self.tokens(group, c"cgroup.controllers", |token| {
            listed |= matches!(token, Token::Word(word) if word == controller);
            Ok(())
        })?;
        Ok(listed)
    }

    pub fn snapshot(&mut self, group: &File, scope: Scope) -> io::Result<Snapshot> {
        let cpu_usec = self.cpu(group);
        let memory_bytes = self.scalar(group, c"memory.current");
        let threads = self.scalar(group, c"pids.current");
        let memory_limit = self.memory_limit(group);
        let task_limit = self.task_limit(group);
        let events = events::read(self, group);
        let (read_bytes, write_bytes) = match self.io(group) {
            Ok((r, w)) => (Ok(IoBytes::Counted(r)), Ok(IoBytes::Counted(w))),
            // io.stat exists where io is enabled for the cgroup (1150 of 1150 live cgroups,
            // audit-fix-init-v1/runs/cgroup-io-probe.log). Only a listing that leaves io
            // out explains its absence; an unreadable listing keeps the failure.
            Err(Failure::Io(libc::ENOENT)) if self.enabled(group, b"io") == Ok(false) => {
                (Ok(IoBytes::NotDelegated), Ok(IoBytes::NotDelegated))
            }
            Err(e) => (Err(e), Err(e)),
        };
        let time_unix_nano = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|t| u64::try_from(t.as_nanos()).ok())
            .filter(|&t| t != 0)
            .ok_or(io::ErrorKind::InvalidData)?;
        Ok(Snapshot {
            scope,
            time_unix_nano,
            cpu_usec,
            memory_bytes,
            threads,
            memory_limit,
            task_limit,
            events,
            read_bytes,
            write_bytes,
        })
    }
}

fn native(error: io::Error) -> Failure {
    Failure::Io(error.raw_os_error().unwrap_or(libc::EIO))
}

fn number(bytes: &[u8]) -> Result<u64, Failure> {
    if bytes.is_empty() {
        return Err(Failure::Invalid);
    }
    bytes.iter().try_fold(0u64, |n, &b| {
        if !b.is_ascii_digit() {
            return Err(Failure::Invalid);
        }
        n.checked_mul(10)
            .and_then(|n| n.checked_add(u64::from(b - b'0')))
            .ok_or(Failure::Overflow)
    })
}
