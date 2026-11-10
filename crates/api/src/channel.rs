//! The existing Firecracker CONNECT handshake, with one optional absolute deadline.
use std::io::{self, Read as _, Write as _};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::Stream;
use crate::stream::{self, Socket};

// Firecracker's host-initiated CONNECT/OK messages carry a decimal u32 port.
pub(crate) const ANSWER: usize = "OK ".len() + "4294967295".len() + "\n".len();
const REQUEST: usize = "CONNECT ".len() + "4294967295".len() + "\n".len();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakePhase {
    Dial,
    Write,
    Read,
    ClearTimeouts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyError {
    TooLong,
    NotOk,
}

/// A handshake's actual phase/cause, without allocating to report it.
#[derive(Debug)]
pub enum HandshakeError {
    Expired {
        phase: HandshakePhase,
    },
    Io {
        phase: HandshakePhase,
        source: io::Error,
    },
    Closed,
    Reply(ReplyError),
}

impl HandshakeError {
    pub(crate) fn allocation(&self) -> bool {
        matches!(self, Self::Io { source, .. } if source.kind() == io::ErrorKind::OutOfMemory)
    }

    pub(crate) fn retryable(&self) -> bool {
        match self {
            Self::Expired { .. } | Self::Closed => true,
            Self::Io {
                phase: HandshakePhase::Dial,
                source,
            } => {
                matches!(
                    source.kind(),
                    io::ErrorKind::NotFound
                        | io::ErrorKind::ConnectionRefused
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) || {
                    #[cfg(windows)]
                    {
                        source.raw_os_error() == Some(windows_sys::Win32::Networking::WinSock::WSAENOBUFS)
                    }
                    #[cfg(not(windows))]
                    {
                        false
                    }
                }
            }
            Self::Io {
                phase: HandshakePhase::Write | HandshakePhase::Read,
                ..
            } => true,
            Self::Io { .. } | Self::Reply(_) => false,
        }
    }
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired { phase } => write!(f, "{phase:?}: handshake deadline expired"),
            Self::Io { phase, source } => write!(
                f,
                "{phase:?}: {:?} (OS {:?})",
                source.kind(),
                source.raw_os_error()
            ),
            Self::Closed => f.write_str("closed unanswered (the guest does not listen there)"),
            Self::Reply(ReplyError::TooLong) => f.write_str("answered past the longest OK, no line"),
            Self::Reply(ReplyError::NotOk) => f.write_str("answered not OK"),
        }
    }
}
impl std::error::Error for HandshakeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn remaining(deadline: Option<Instant>, phase: HandshakePhase) -> Result<Option<Duration>, HandshakeError> {
    deadline
        .map(|at| {
            let left = at.saturating_duration_since(Instant::now());
            if left.is_zero() {
                Err(HandshakeError::Expired { phase })
            } else {
                Ok(left)
            }
        })
        .transpose()
}

pub(crate) fn handshake(
    path: &Path,
    port: u32,
    deadline: Option<Instant>,
    cost: &mut Duration,
) -> Result<Socket, HandshakeError> {
    use HandshakePhase::{ClearTimeouts, Dial, Read, Write};
    let began = Instant::now();
    remaining(deadline, Dial)?;
    let mut s = stream::connect_until(path, deadline).map_err(|source| {
        *cost = began.elapsed();
        HandshakeError::Io { phase: Dial, source }
    })?;
    let mut request = [0; REQUEST];
    let mut unused = request.as_mut_slice();
    writeln!(&mut unused, "CONNECT {port}").map_err(|source| HandshakeError::Io { phase: Write, source })?;
    let used = REQUEST.saturating_sub(unused.len());
    let mut ask = request
        .get(..used)
        .ok_or(HandshakeError::Reply(ReplyError::NotOk))?;
    while !ask.is_empty() {
        Stream::set_write_timeout(&s, remaining(deadline, Write)?)
            .map_err(|source| HandshakeError::Io { phase: Write, source })?;
        let sent = s.write(ask);
        *cost = began.elapsed();
        remaining(deadline, Write)?;
        match sent {
            Ok(0) => {
                return Err(HandshakeError::Io {
                    phase: Write,
                    source: io::ErrorKind::WriteZero.into(),
                });
            }
            Ok(n) => ask = ask.get(n..).ok_or(HandshakeError::Reply(ReplyError::NotOk))?,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(source) => return Err(HandshakeError::Io { phase: Write, source }),
        }
    }
    let mut line = [0; ANSWER];
    let mut used = 0usize;
    loop {
        let byte = line
            .get_mut(used..used.saturating_add(1))
            .ok_or(HandshakeError::Reply(ReplyError::TooLong))?;
        Stream::set_read_timeout(&s, remaining(deadline, Read)?)
            .map_err(|source| HandshakeError::Io { phase: Read, source })?;
        let read = s.read(byte);
        remaining(deadline, Read)?;
        match read {
            Ok(0) => return Err(HandshakeError::Closed),
            Ok(_) => {
                used = used.saturating_add(1);
                if byte.first() == Some(&b'\n') {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(source) => return Err(HandshakeError::Io { phase: Read, source }),
        }
    }
    // UTF-8 is checked without a lossy conversion allocation. Preserve the accepted
    // decimal u32 and trailing-whitespace grammar of the existing handshake.
    let ok = line
        .get(..used)
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|answer| answer.strip_prefix("OK "))
        .is_some_and(|n| n.trim_end().parse::<u32>().is_ok());
    if !ok {
        return Err(HandshakeError::Reply(ReplyError::NotOk));
    }
    Stream::set_write_timeout(&s, None)
        .and_then(|()| Stream::set_read_timeout(&s, None))
        .map_err(|source| HandshakeError::Io {
            phase: ClearTimeouts,
            source,
        })?;
    remaining(deadline, ClearTimeouts)?;
    Ok(s)
}

/// The narrow existing String-refusal bridge. Every append can fail, including
/// rendering an error; OOM is returned without constructing a fallback String.
pub(crate) fn message(args: std::fmt::Arguments<'_>) -> Result<String, ()> {
    struct Text(String);
    impl std::fmt::Write for Text {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            self.0.try_reserve(s.len()).map_err(|_| std::fmt::Error)?;
            self.0.push_str(s);
            Ok(())
        }
    }
    let mut text = Text(String::new());
    std::fmt::write(&mut text, args).map_err(|_| ())?;
    Ok(text.0)
}
