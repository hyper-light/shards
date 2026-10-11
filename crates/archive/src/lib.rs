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
pub mod receive;
pub mod tar;

pub use error::{Error, Kind};
pub use layer::apply_layer;
pub use pack::{PackOptions, pack};
pub use unpack::{UnpackOptions, unpack, untar};
pub use whiteout::{
    WHITEOUT_LINK_DIR, WHITEOUT_META_PREFIX, WHITEOUT_OPAQUE_DIR, WHITEOUT_PREFIX, WhiteoutFormat,
};

/// Why a file of a root could not be read through Go's `os.Root.FS()`: its stat failed,
/// or its read did, each in Go's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootRead {
    Stat(Error),
    Read(Error),
}

/// `fs.ValidPath`, as `os.Root.FS()` checks a name (isValidRootFSPath): `/`-separated
/// elements, none empty, `.` or `..`, unless the whole is `.`; and no `\` on Windows.
fn valid_fs_path(name: &str) -> bool {
    if cfg!(windows) && name.contains('\\') {
        return false;
    }
    name == "." || name.split('/').all(|e| !e.is_empty() && e != "." && e != "..")
}

/// A file inside the directory `root`, as buildx reads a policy file through Go's
/// `os.OpenRoot(root).FS()` (go1.26.1 os/root.go): its name checked as fs.FS checks
/// names, stat'ed (`rootFS.Stat`, `Root.Stat`), then opened and read (`fs.ReadFile`),
/// each walk kept inside the root and following symlinks only while they stay in it.
pub fn read_in_root(root: &[u8], name: &str) -> Result<Vec<u8>, RootRead> {
    if !valid_fs_path(name) {
        return Err(RootRead::Stat(Error::other(format!(
            "stat {name}: invalid argument"
        ))));
    }
    let r = sys::Root::open(root).map_err(RootRead::Stat)?;
    r.stat(name.as_bytes())
        .map_err(|e| RootRead::Stat(e.error("statat", name.as_bytes())))?;
    let mut f = r
        .open_read(name.as_bytes())
        .map_err(|e| RootRead::Read(e.error("openat", name.as_bytes())))?;
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut f, &mut data)
        .map_err(|e| RootRead::Read(Error::path("read", name.as_bytes(), &e)))?;
    Ok(data)
}

#[cfg(all(test, unix))]
mod root_tests {
    use super::*;

    #[test]
    fn a_file_of_a_root_is_read_as_go_reads_it_through_the_roots_fs() {
        let dir_guard = shards_testdir::TempDir::new("read-in-root").unwrap();
        let dir = dir_guard.join("read-in-root");
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/p.rego"), b"package docker\n").unwrap();
        std::fs::write(dir.join("outside.rego"), b"x").unwrap();
        std::os::unix::fs::symlink("../outside.rego", root.join("out")).unwrap();
        std::os::unix::fs::symlink("sub/p.rego", root.join("in")).unwrap();
        let r = root.as_os_str().as_encoded_bytes();
        assert_eq!(read_in_root(r, "sub/p.rego").unwrap(), b"package docker\n");
        assert_eq!(read_in_root(r, "in").unwrap(), b"package docker\n");
        let stat_err = |name: &str| match read_in_root(r, name) {
            Err(RootRead::Stat(e)) => (e.kind(), e.to_string()),
            other => panic!("{name}: {other:?}"),
        };
        assert_eq!(
            stat_err("missing.rego"),
            (
                Kind::NotFound,
                "statat missing.rego: no such file or directory".into()
            )
        );
        assert_eq!(
            stat_err("out"),
            (Kind::Breakout, "statat out: path escapes from parent".into())
        );
        assert_eq!(
            stat_err("../outside.rego"),
            (Kind::Other, "stat ../outside.rego: invalid argument".into())
        );
        assert_eq!(
            stat_err("/abs.rego"),
            (Kind::Other, "stat /abs.rego: invalid argument".into())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
