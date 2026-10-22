//! Pulling images from OCI registries (docs/research/registry-pull.md). This is the one
//! crate with C in its build: the TLS provider (registry-pull R3).

use std::fmt;
use std::io;

#[cfg(target_os = "macos")]
mod apple;
pub mod auth;
pub mod certs;
pub mod credentials;
mod fetch;
pub mod http;
pub mod proxy;
pub mod pull;
pub mod push;
pub mod referrers;
pub mod registries;
pub mod registry;
pub mod search;
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
    /// Whether the server did not speak TLS: it answered the handshake in plain HTTP, or
    /// never finished it, as containerd's `isTLSError` finds. A loopback registry on a
    /// port that names no scheme is then asked in plain HTTP (registry.rs).
    not_tls: bool,
    /// Whether every address the server was dialled at refused the connection: a
    /// registry named insecure without a port is then asked in plain HTTP on HTTP's
    /// (registry.rs, D194).
    refused: bool,
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
    /// A token server refused the credentials (401 or 403): what a login is refused by.
    /// Every other caller takes it as [`Other`](ErrorKind::Other).
    Refused,
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
            not_tls: false,
            refused: false,
        }
    }

    /// The same error, from a server that did not speak TLS.
    pub(crate) fn not_tls(self) -> Error {
        Error {
            not_tls: true,
            ..self
        }
    }

    pub(crate) fn is_not_tls(&self) -> bool {
        self.not_tls
    }

    /// The same error, from a server that refused the connection.
    pub(crate) fn refused(self) -> Error {
        Error {
            refused: true,
            ..self
        }
    }

    pub(crate) fn is_refused(&self) -> bool {
        self.refused
    }

    /// The same error, with what dockerd says of it.
    pub(crate) fn said(self, said: Said) -> Error {
        Error {
            said: Some(said),
            ..self
        }
    }

    /// The error a pull or a push reports: a registry's refusal in the registry's own
    /// words, anything else as it is.
    pub(crate) fn into_reported(self) -> Error {
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
            not_tls: self.not_tls,
            refused: self.refused,
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
