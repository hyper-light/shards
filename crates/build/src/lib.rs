//! What `shards build` does to files (docs/design/architecture.md D33): BuildKit's file
//! operations, on snapshots held in memory, and the layers BuildKit writes of them.

use std::fmt;

pub mod archive;
pub mod context;
pub mod copy;
pub mod data;
pub mod diff;
pub mod host;
pub mod mode;
pub mod ops;
pub mod stack;
pub mod sync;
pub mod upper;
pub mod vfs;

/// Why a file operation failed, worded as BuildKit words it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// `errors.Wrap(err, what)`.
pub(crate) fn wrap(what: &str, e: impl fmt::Display) -> Error {
    Error(format!("{what}: {e}"))
}
