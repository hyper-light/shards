//! Images for shards microVMs. Container layers are flattened on the host into one
//! read-only EROFS filesystem per image, which guests mount from virtio-pmem
//! (docs/research/image-storage.md R1-R2).

pub mod config;
pub use shards_erofs as erofs;
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

/// Why an image could not be read or built: one error with its EROFS writer's.
pub use shards_erofs::Error;

fn bad<T>(msg: impl Into<String>) -> Result<T, Error> {
    Err(Error(msg.into()))
}
