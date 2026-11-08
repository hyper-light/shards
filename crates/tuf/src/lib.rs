//! The Update Framework's client, as go-tuf v2.4.2 runs it for sigstore-go and BuildKit's
//! policy helpers (D104): Sigstore's trusted root (`trusted_root.json`), from a root
//! carried in the binary, rotated and refreshed from the repository, cached, and trusted
//! only as the specification allows. Held to go-tuf by `tests/oracle.rs` against
//! `scripts/tuf/generate`.

pub mod client;
pub mod gojson;
pub mod keys;
pub mod metadata;
pub mod trusted;
pub mod updater;

pub use updater::{Fetch, FetchError};

/// go-tuf's errors, by kind, each printed as go-tuf prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// encoding/json's, and time's, own words.
    Json(String),
    Value(String),
    Type(String),
    Runtime(String),
    Repository(String),
    Unsigned(String),
    BadVersion(String),
    EqualVersion(String),
    Expired(String),
    LengthOrHashMismatch(String),
    Fetch(FetchError),
    /// A key that cannot be read, in x509's or the verifier's words.
    Key(String),
    /// The cache's.
    Io(String),
    Other(String),
}

impl Error {
    /// errors.Is(err, &ErrRepository{}): the repository served something not to trust.
    pub fn is_repository(&self) -> bool {
        matches!(
            self,
            Error::Repository(_)
                | Error::Unsigned(_)
                | Error::BadVersion(_)
                | Error::Expired(_)
                | Error::LengthOrHashMismatch(_)
        )
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Json(s) | Error::Key(s) | Error::Io(s) | Error::Other(s) => f.write_str(s),
            Error::Value(s) => write!(f, "value error: {s}"),
            Error::Type(s) => write!(f, "type error: {s}"),
            Error::Runtime(s) => write!(f, "runtime error: {s}"),
            Error::Repository(s) => write!(f, "repository error: {s}"),
            Error::Unsigned(s) => write!(f, "unsigned metadata error: {s}"),
            Error::BadVersion(s) => write!(f, "bad version number error: {s}"),
            Error::EqualVersion(s) => write!(f, "equal version number error: {s}"),
            Error::Expired(s) => write!(f, "expired metadata error: {s}"),
            Error::LengthOrHashMismatch(s) => write!(f, "length/hash verification error: {s}"),
            Error::Fetch(e) => write!(f, "{e}"),
        }
    }
}
