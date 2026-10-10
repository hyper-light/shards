//! Images for shards microVMs. Container layers are flattened on the host into one
//! read-only EROFS filesystem per image, which guests mount from virtio-pmem
//! (docs/research/image-storage.md R1-R2).

use std::fmt;
use std::io;

pub mod config;
pub mod erofs;
pub mod go;
pub mod json;
pub mod layer;
pub mod oci;
pub mod osi;
pub mod platform;
pub mod reference;
pub mod save;
pub mod store;
mod tables;
pub mod tar;

/// Why an image could not be read or built.
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

fn bad<T>(msg: impl Into<String>) -> Result<T, Error> {
    Err(Error(msg.into()))
}
