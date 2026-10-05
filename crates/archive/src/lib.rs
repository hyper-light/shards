//! Tar archives of a directory's files as moby/go-archive v0.3.3 makes them
//! (`TarWithOptions`, here [`pack`]) and unpacks them (`Unpack`, here [`unpack`]), with
//! Go's archive/tar's encoding ([`tar`]): what `docker export` and `docker cp` send and
//! take. [`copy`] holds go-archive's copy.go, which decides `docker cp`'s paths.
//!
//! Everything streams: archives are written to an `io::Write` and read from an
//! `io::Read`, a file's data never held whole. scripts/archive/generate holds all of it to
//! go-archive and Go, byte for byte and tree for tree (tests/oracle.rs).
//!
//! Where this differs from go-archive, on purpose:
//! - Hard links are found by device and inode, not inode alone, so files of two
//!   filesystems never become links of each other.
//! - What go-archive logs and leaves as a broken or empty archive fails instead: a missing
//!   source, a pattern that cannot be matched, a file that cannot be opened or shrank after
//!   its header was written.
//! - An old GNU sparse map is held to 1 MiB, as Go holds every other header.
//! - A symlink entry's extended attributes are set on the symlink, not its target.
//! - Archives are not decompressed: `docker export` and `docker cp` send them plain.
//! - On Windows, devices and FIFOs are skipped (go-archive fails on them there).
//!
//! [`apply_layer`] applies a layer over a tree, whiteouts as deletions (diff.go's
//! UnpackLayer); [`WhiteoutFormat::Overlay`] packs an overlayfs upper directory into such a
//! layer and unpacks one back into an upper directory.
//!
//! Not ported, as nothing here uses them: ID mappings, user namespaces, compression, and
//! go-archive's change and diff helpers.

mod error;
mod gopath;
mod layer;
mod pack;
mod patterns;
mod root;
mod sys;
mod unpack;
mod whiteout;

pub mod copy;
pub mod tar;

pub use error::{Error, Kind};
pub use layer::apply_layer;
pub use pack::{PackOptions, pack};
pub use unpack::{UnpackOptions, unpack, untar};
pub use whiteout::{
    WHITEOUT_LINK_DIR, WHITEOUT_META_PREFIX, WHITEOUT_OPAQUE_DIR, WHITEOUT_PREFIX, WhiteoutFormat,
};
