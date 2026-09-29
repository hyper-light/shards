//! Pulling images from OCI registries (docs/research/registry-pull.md). This is the one
//! crate with C in its build: the TLS provider (registry-pull R3).

use std::fmt;
use std::io;

pub mod auth;
pub mod http;
#[cfg(test)]
mod testing;
pub mod tls;
pub mod url;

/// Why a registry could not be reached, or refused.
#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error(e.to_string())
    }
}

impl From<rustls::Error> for Error {
    fn from(e: rustls::Error) -> Error {
        Error(format!("TLS: {e}"))
    }
}
