//! Images for shards microVMs. Container layers are flattened on the host into one
//! read-only EROFS filesystem per image, which guests mount from virtio-pmem
//! (docs/research/image-storage.md R1-R2).

pub mod erofs;
