//! Pulling images from OCI registries (docs/research/registry-pull.md). This is the one
//! crate with C in its build: the TLS provider (registry-pull R3).

use std::fmt;
use std::io;

pub mod auth;
pub mod certs;
pub mod credentials;
pub mod http;
pub mod proxy;
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
    /// What dockerd says in its place, where a registry refused a request.
    said: Option<Said>,
}

/// What dockerd says of a registry's refusal in place of containerd's words
/// (`translateRegistryError`, daemon/containerd/registry_errors.go).
#[derive(Debug)]
pub(crate) enum Said {
    /// The registry's own errors, which dockerd says alone.
    Instead(String),
    /// dockerd's word, before the whole error.
    Before(&'static str),
}

/// What kind of failure an error is, where a caller acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// A timeout, or a connection cut short: containerd tries again
    /// (`isTransientTransportErr`).
    Transient,
    /// The registry has no such content (404).
    NotFound,
    /// The store has no such content: what a push was to send is not here.
    Missing,
    /// The registry refused the request's authorization, or asked for credentials there
    /// are none of: containerd's ErrInvalidAuthorization.
    Unauthorized,
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
            said: None,
        }
    }

    /// The same error, with what dockerd says of it.
    pub(crate) fn said(self, said: Said) -> Error {
        Error {
            said: Some(said),
            ..self
        }
    }

    /// The error as dockerd reports a pull's or a push's: a registry's refusal in its
    /// words, anything else as it is.
    pub(crate) fn in_dockerds_words(self) -> Error {
        let message = match self.said {
            Some(Said::Instead(message)) => message,
            Some(Said::Before(word)) => format!("{word}: {}", self.message),
            None => self.message,
        };
        Error::of(self.kind, message)
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The same error, with `context` in front of its message.
    pub(crate) fn context(self, context: impl fmt::Display) -> Error {
        Error {
            message: format!("{context}: {}", self.message),
            kind: self.kind,
            said: self.said,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// `text` with its control characters escaped: what a registry says goes to a terminal.
pub(crate) fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// Timeouts and early ends are transient, as Go's are to containerd
/// (isTransientTransportErr); and so is a connection reset, aborted or broken under a
/// request, which containerd gives up on: a download resumes where it was cut, and the
/// requests sent again are a GET or HEAD, a new upload, or a PUT by digest.
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        let kind = match e.kind() {
            io::ErrorKind::TimedOut
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe => ErrorKind::Transient,
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
