//! A tree written into a directory as fsutil's DiskWriter receives one (tonistiigi/fsutil
//! a2aa163d723f diskwriter.go, diskwriter_unix.go), which is how buildx writes a build's
//! `local` output (buildkit session/filesync/diffcopy.go):
//!
//! - The destination is made as `MkdirAll(dest, 0700)` makes it; what it holds is merged
//!   into, never emptied, unless a mirror is asked for (`mode=delete`, DiffNone).
//! - An entry over a directory that is a directory too only takes the entry's attributes;
//!   any other replaces what is there, a directory in its way removed whole.
//! - Each entry gets its mode (set-id and sticky bits too), its owner, its extended
//!   attributes (where the system takes them, as fsutil ignores Setxattr's errors) and its
//!   time, a directory's once everything in it is written.
//!
//! Every name is walked from the destination as Go's os.Root walks it ([`crate::sys`]),
//! so no entry, and no symlink the destination or the tree holds, writes outside it.
//! fsutil joins paths, and follows symlinks the destination already had.

use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::Path;

use crate::error::Error;
use crate::gopath::{self, NATIVE, Os};
use crate::sys::{self, FileKind, Root};
use crate::tar::Time;

/// What an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind<'a> {
    Dir,
    File,
    Symlink(&'a [u8]),
    /// A hard link to an entry received before it, by that entry's path.
    Link(&'a [u8]),
    Char {
        major: u32,
        minor: u32,
    },
    Block {
        major: u32,
        minor: u32,
    },
    Fifo,
}

/// An entry of the tree: its path under the destination, `/`-separated with no leading
/// `/`, and its attributes.
#[derive(Debug, Clone)]
pub struct Entry<'a> {
    pub path: &'a [u8],
    pub kind: Kind<'a>,
    /// Permission, set-id and sticky bits.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: Time,
    pub xattrs: Vec<(&'a [u8], &'a [u8])>,
}

/// A destination being received into.
#[derive(Debug)]
pub struct Receiver {
    root: Root,
    /// Directories made, and their times, set once all is written (DiskWriter.Wait).
    dirs: Vec<(Vec<u8>, Time)>,
    /// With a mirror asked for, every path received, to remove the rest.
    received: Option<BTreeSet<Vec<u8>>>,
}

impl Receiver {
    /// Makes `dest` as `MkdirAll(dest, 0700)` and receives into it; with `mirror`, what
    /// it held that the tree lacks goes when [`Receiver::finish`] runs.
    pub fn create(dest: &Path, mirror: bool) -> Result<Receiver, Error> {
        let name = sys::path_bytes(dest);
        mkdir_all(dest).map_err(|e| Error::path("mkdir", &name, &e))?;
        Ok(Receiver {
            root: Root::open(&name)?,
            dirs: Vec::new(),
            received: mirror.then(BTreeSet::new),
        })
    }

    /// Receives `e`; a file's bytes are written by `data`.
    pub fn put(
        &mut self,
        e: &Entry<'_>,
        data: &mut dyn FnMut(&mut dyn Write) -> io::Result<()>,
    ) -> Result<(), Error> {
        let path = e.path;
        if let Some(received) = &mut self.received {
            received.insert(path.to_vec());
        }
        let old = match self.root.lstat(path) {
            Ok(st) => Some(st.kind),
            Err(w) if w.is_not_found() => None,
            Err(w) => return Err(w.error("lstat", path)),
        };
        if e.kind == Kind::Dir && old == Some(FileKind::Dir) {
            return self.attributes(e, path);
        }
        // Another kind, or a file of the same: removed, as the rename fsutil makes over
        // it, and a directory in its way, removes it.
        if old.is_some() {
            self.root
                .remove_all(path)
                .map_err(|w| w.error("unlinkat", path))?;
        }
        match e.kind {
            Kind::Dir => {
                self.root
                    .mkdir(path, e.mode & 0o777)
                    .map_err(|w| w.error("mkdirat", path))?;
                self.dirs.push((path.to_vec(), e.mtime));
            }
            Kind::File => {
                let mut f = self
                    .root
                    .create(path, e.mode & 0o777)
                    .map_err(|w| w.error("openat", path))?;
                data(&mut f).map_err(|err| Error::path("write", path, &err))?;
                f.flush().map_err(|err| Error::path("write", path, &err))?;
            }
            Kind::Symlink(target) => {
                self.root
                    .symlink(target, path)
                    .map_err(|w| w.error("symlinkat", path))?;
            }
            Kind::Link(old) => {
                self.root
                    .link(old, path)
                    .map_err(|w| w.link_error("linkat", old, path))?;
            }
            Kind::Char { major, minor } => self.node(path, sys::S_IFCHR, e.mode, major, minor)?,
            Kind::Block { major, minor } => self.node(path, sys::S_IFBLK, e.mode, major, minor)?,
            Kind::Fifo => self.node(path, sys::S_IFIFO, e.mode, 0, 0)?,
        }
        self.attributes(e, path)
    }

    fn node(&self, path: &[u8], kind: u32, mode: u32, major: u32, minor: u32) -> Result<(), Error> {
        let (dir, base) = parent_and_base(path);
        self.root
            .mknod(&dir, &base, kind | (mode & 0o7777), major, minor)
            .map_err(|w| w.error("openat", &dir))?
            .map_err(|err| Error::path("mknodat", path, &err))
    }

    /// rewriteMetadata: attributes, owner, mode (not a symlink's), then time.
    fn attributes(&self, e: &Entry<'_>, path: &[u8]) -> Result<(), Error> {
        let (dir, base) = parent_and_base(path);
        if NATIVE != Os::Windows {
            for (key, value) in &e.xattrs {
                // Ignored where they fail, as fsutil ignores Setxattr's errors.
                let _ = self.root.set_xattr(&dir, &base, key, value);
            }
        }
        self.root
            .lchown_in(&dir, &base, i64::from(e.uid), i64::from(e.gid))
            .map_err(|w| w.error("openat", &dir))?
            .map_err(|err| Error::path("lchown", path, &err))?;
        let symlink = matches!(e.kind, Kind::Symlink(_))
            || matches!(e.kind, Kind::Link(_))
                && self.root.lstat(path).is_ok_and(|st| st.kind == FileKind::Symlink);
        if NATIVE != Os::Windows && !symlink {
            self.root
                .chmod_nofollow(&dir, &base, e.mode & 0o7777)
                .map_err(|w| w.error("openat", &dir))?
                .map_err(|err| Error::path("chmod", path, &err))?;
        }
        if symlink {
            if NATIVE != Os::Windows {
                self.root
                    .lchtimes(&dir, &base, e.mtime, e.mtime)
                    .map_err(|w| w.error("openat", &dir))?
                    .map_err(|err| Error::path("lchtimes", path, &err))?;
            }
        } else {
            self.root
                .chtimes(path, e.mtime, e.mtime)
                .map_err(|w| w.error("chtimesat", path))?;
        }
        Ok(())
    }

    /// Sets the times of the directories made, once nothing more goes into them, and with
    /// a mirror, removes what the tree did not hold.
    pub fn finish(self) -> Result<(), Error> {
        if let Some(received) = &self.received {
            self.prune(b".", received)?;
        }
        for (path, mtime) in self.dirs.iter().rev() {
            self.root
                .chtimes(path, *mtime, *mtime)
                .map_err(|w| w.error("chtimesat", path))?;
        }
        Ok(())
    }

    /// Removes what `dir` holds that was not received, and looks into what was.
    fn prune(&self, dir: &[u8], received: &BTreeSet<Vec<u8>>) -> Result<(), Error> {
        let mut names = self.root.read_dir(dir).map_err(|w| w.error("open", dir))?;
        names.sort();
        for name in names {
            let path = if dir == b"." {
                name
            } else {
                [dir, b"/", &name].concat()
            };
            if !received.contains(&path) {
                self.root
                    .remove_all(&path)
                    .map_err(|w| w.error("unlinkat", &path))?;
            } else if self.root.lstat(&path).is_ok_and(|st| st.kind == FileKind::Dir) {
                self.prune(&path, received)?;
            }
        }
        Ok(())
    }
}

/// filepath.Dir and filepath.Base of a root-relative name.
fn parent_and_base(name: &[u8]) -> (Vec<u8>, Vec<u8>) {
    (gopath::dir(NATIVE, name), gopath::base(NATIVE, name))
}

#[cfg(unix)]
fn mkdir_all(dest: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dest)
}

#[cfg(not(unix))]
fn mkdir_all(dest: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dest)
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn entry<'a>(path: &'a [u8], kind: Kind<'a>, mode: u32, mtime: i64) -> Entry<'a> {
        Entry {
            path,
            kind,
            mode,
            uid: own().0,
            gid: own().1,
            mtime: Time::unix(mtime, 123_456_789),
            xattrs: Vec::new(),
        }
    }

    /// The test's own ids: what a file it makes is owned by.
    fn own() -> (u32, u32) {
        let probe = std::env::temp_dir().join(format!("receive-own-{}", std::process::id()));
        std::fs::write(&probe, b"").unwrap();
        let p = std::fs::metadata(&probe).unwrap();
        std::fs::remove_file(&probe).unwrap();
        (p.uid(), p.gid())
    }

    fn put(r: &mut Receiver, e: Entry<'_>, bytes: &[u8]) {
        r.put(&e, &mut |w| w.write_all(bytes)).unwrap();
    }

    /// A tree lands whole: kinds, modes with their set-id bits, links, nanosecond times
    /// on directories too; an existing destination is merged into, and with a mirror
    /// asked for, emptied of the rest; no symlink takes a write outside it.
    #[test]
    fn a_tree_is_received_as_fsutil_receives_it() {
        let tmp = std::env::temp_dir().join(format!("receive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let dest = tmp.join("a/b");
        std::fs::create_dir_all(tmp.join("outside")).unwrap();
        let mut r = Receiver::create(&dest, false).unwrap();
        put(&mut r, entry(b"d", Kind::Dir, 0o755, 1_000), b"");
        put(&mut r, entry(b"d/f", Kind::File, 0o4755, 2_000), b"hello");
        put(&mut r, entry(b"d/h", Kind::Link(b"d/f"), 0o4755, 2_000), b"");
        put(
            &mut r,
            entry(b"d/l", Kind::Symlink(b"../../outside"), 0o777, 3_000),
            b"",
        );
        // Through the symlink: refused, not written outside.
        assert!(
            r.put(&entry(b"d/l/x", Kind::File, 0o644, 1), &mut |w| w.write_all(b"x"))
                .is_err()
        );
        r.finish().unwrap();
        let md = |p: &str| std::fs::symlink_metadata(dest.join(p)).unwrap();
        assert_eq!(
            std::fs::metadata(&dest).unwrap().permissions().mode() & 0o7777,
            0o700 & !umask()
        );
        assert_eq!(md("d/f").permissions().mode() & 0o7777, 0o4755);
        assert_eq!(md("d/f").ino(), md("d/h").ino());
        assert_eq!((md("d").mtime(), md("d").mtime_nsec()), (1_000, 123_456_789));
        assert_eq!((md("d/l").mtime(), md("d/l").mtime_nsec()), (3_000, 123_456_789));
        assert!(!tmp.join("outside/x").exists());

        // Merged: what was there stays, what the tree holds replaces it.
        std::fs::write(dest.join("keep"), b"k").unwrap();
        std::fs::create_dir(dest.join("d/f2")).unwrap();
        let mut r = Receiver::create(&dest, false).unwrap();
        put(&mut r, entry(b"d", Kind::Dir, 0o750, 1_000), b"");
        put(
            &mut r,
            entry(b"d/f2", Kind::File, 0o600, 4_000),
            b"over a directory",
        );
        r.finish().unwrap();
        assert_eq!(std::fs::read(dest.join("keep")).unwrap(), b"k");
        assert_eq!(std::fs::read(dest.join("d/f2")).unwrap(), b"over a directory");
        assert_eq!(md("d").permissions().mode() & 0o7777, 0o750);
        assert!(dest.join("d/f").exists());

        // Mirrored: the rest goes.
        let mut r = Receiver::create(&dest, true).unwrap();
        put(&mut r, entry(b"d", Kind::Dir, 0o755, 1_000), b"");
        put(&mut r, entry(b"d/f", Kind::File, 0o644, 2_000), b"hello");
        r.finish().unwrap();
        assert!(!dest.join("keep").exists());
        assert!(!dest.join("d/f2").exists() && !dest.join("d/l").exists());
        assert_eq!(std::fs::read(dest.join("d/f")).unwrap(), b"hello");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    fn umask() -> u32 {
        let probe = std::env::temp_dir().join(format!("receive-umask-{}", std::process::id()));
        std::fs::DirBuilder::new().create(&probe).unwrap();
        let mode = std::fs::metadata(&probe).unwrap().permissions().mode() & 0o777;
        std::fs::remove_dir(&probe).unwrap();
        0o777 & !mode
    }
}
