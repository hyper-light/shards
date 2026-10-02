//! Pulling images from OCI registries (docs/research/registry-pull.md). This is the one
//! crate with C in its build: the TLS provider (registry-pull R3).

use std::fmt;
use std::io;

pub mod auth;
pub mod certs;
pub mod credentials;
pub mod http;
pub mod pull;
pub mod push;
pub mod registry;
#[cfg(test)]
mod testing;
pub mod tls;
pub mod url;

/// Why a registry could not be reached, or refused.
#[derive(Debug)]
pub struct Error {
    message: String,
    kind: ErrorKind,
}

/// What kind of failure an error is, where a caller acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// A timeout, or a connection cut short: containerd tries again
    /// (`isTransientTransportErr`).
    Transient,
    /// The registry has no such content (404).
    NotFound,
    /// The request was cancelled ([`http::Cancel`]): nothing tries it again.
    Cancelled,
    /// A stored copy is not what its digest names any more: a pull fetches it again.
    Changed,
    Other,
}

impl Error {
    pub(crate) fn new(message: impl Into<String>) -> Error {
        Error::of(ErrorKind::Other, message)
    }

    pub(crate) fn of(kind: ErrorKind, message: impl Into<String>) -> Error {
        Error {
            message: message.into(),
            kind,
        }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The same error, with `context` in front of its message.
    pub(crate) fn context(self, context: impl fmt::Display) -> Error {
        Error {
            message: format!("{context}: {}", self.message),
            kind: self.kind,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// Timeouts and early ends are transient, as Go's are to containerd.
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        let kind = match e.kind() {
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::UnexpectedEof => {
                ErrorKind::Transient
            }
            _ => ErrorKind::Other,
        };
        Error::of(kind, e.to_string())
    }
}

impl From<shards_image::Error> for Error {
    fn from(e: shards_image::Error) -> Error {
        Error::new(e.to_string())
    }
}

impl From<rustls::Error> for Error {
    fn from(e: rustls::Error) -> Error {
        Error::new(format!("TLS: {e}"))
    }
}
