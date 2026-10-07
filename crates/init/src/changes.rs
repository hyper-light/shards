//! What a container changed of its image's files, for `shards diff`: Docker's answer
//! (moby daemon/containerd/image_changes.go: go-archive v0.3.3 `ChangesDirs` of the
//! container's root against its image's), found without walking the whole image.
//!
//! The root is an overlay of the container's writable layer over the image
//! (run.rs, mount_root), and a directory the container changed anything in is in that
//! layer: only those are walked, each one's entries in the root compared with the
//! image's by go-archive's rules (changes_unix.go, statDifferent): mode, owner, device,
//! and for all but directories size and mtime, and the `security.capability` xattr. A
//! directory with changes inside that is not itself added or changed is changed too
//! (changes.go, addChanges). Docker's init layer, which the comparison starts from, is
//! what init makes of every container: its mounts and `/etc`'s files, which are not the
//! container's changes.
//!
//! Each change is a line as docker/cli prints it (diff.go): its kind (`C`, `A` or `D`),
//! a space and its path. Each directory's entries come in name order, each followed by
//! what is under it, where Docker's order within a directory is its map's, which
//! varies.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::Metadata;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::sync::OnceLock;

/// The image's layer and the container's writable one, as init mounted them, kept before
/// the root moved over them (run.rs).
static LAYERS: OnceLock<(OwnedFd, OwnedFd)> = OnceLock::new();

/// What init makes of every container, as Docker's init layer holds it (moby
/// daemon/initlayer/setup_unix.go): its mounts and the files it writes in `/etc`.
const MADE: [&str; 7] = [
    "/proc",
    "/sys",
    "/dev",
    "/etc/mtab",
    "/etc/hosts",
    "/etc/hostname",
    "/etc/resolv.conf",
];

/// Keeps the layers at `lower` and `upper` for [`write`]: called once they are mounted,
/// before the root moves over them.
pub fn keep(lower: &str, upper: &str) -> io::Result<()> {
    let open = |path: &str| -> io::Result<OwnedFd> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
            .open(path)?;
        Ok(file.into())
    };
    let _ = LAYERS.set((open(lower)?, open(upper)?));
    Ok(())
}

/// The image's layers kept by [`keep`]: the root as the image made it, before the run
/// wrote to it.
pub fn lower() -> Option<std::os::fd::BorrowedFd<'static>> {
    use std::os::fd::AsFd;
    LAYERS.get().map(|(lower, _)| lower.as_fd())
}

/// Where the writable layer kept by [`keep`] is reached, whatever the root.
pub fn upper() -> Option<String> {
    LAYERS
        .get()
        .map(|(_, upper)| format!("/proc/self/fd/{}", upper.as_raw_fd()))
}

/// A change: its kind and path.
type Change = (u8, Vec<u8>);

/// Writes the changes to `out`.
pub fn write(out: &mut impl Write) -> io::Result<()> {
    let (lower, upper) = LAYERS
        .get()
        .ok_or_else(|| io::Error::other("the container's layers were not kept"))?;
    // A kept descriptor names its directory wherever the root is (proc(5), /proc/pid/fd).
    let at = |fd: &OwnedFd| format!("/proc/self/fd/{}", fd.as_raw_fd());
    let walk = Walk {
        lower: at(lower),
        upper: at(upper),
        mounts: mounts()?,
    };
    let mut changes = Vec::new();
    walk.dir(b"", true, &mut changes)?;
    for (kind, path) in changes {
        out.write_all(&[kind, b' '])?;
        out.write_all(&path)?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

struct Walk {
    lower: String,
    upper: String,
    /// Where something is mounted over the root: what is there is not the container's
    /// files.
    mounts: Vec<Vec<u8>>,
}

impl Walk {
    /// The changes under directory `rel` (empty for the root), which is in the image
    /// (`old`) or not, appended in path order.
    fn dir(&self, rel: &[u8], old: bool, changes: &mut Vec<Change>) -> io::Result<()> {
        let merged = entries(&join(b"/", rel))?;
        let image = if old {
            entries(&join(self.lower.as_bytes(), rel)).unwrap_or_default()
        } else {
            BTreeMap::new()
        };
        // Every name of either, in order.
        let mut names: Vec<&OsString> = merged.keys().chain(image.keys()).collect();
        names.sort();
        names.dedup();
        for name in names {
            let path = [rel, b"/", name.as_bytes()].concat();
            if MADE.iter().any(|m| m.as_bytes() == path.as_slice()) {
                continue;
            }
            // `/etc` where the image has no directory is init's (container_files): only
            // what is in it is the container's, and it is changed if anything is.
            if path == b"/etc" && image.get(name).is_none_or(|m| !m.is_dir()) {
                if merged.get(name).is_some_and(|m| m.is_dir()) {
                    let start = changes.len();
                    self.dir(&path, false, changes)?;
                    if changes.len() > start {
                        changes.insert(start, (b'C', path));
                    }
                }
                continue;
            }
            match (merged.get(name), image.get(name)) {
                (Some(new), Some(was)) => {
                    if self.mounts.contains(&path) {
                        continue;
                    }
                    let start = changes.len();
                    let changed = different(was, new)
                        || capability(&join(self.lower.as_bytes(), &path)) != capability(&join(b"/", &path));
                    if changed {
                        changes.push((b'C', path.clone()));
                    }
                    // Only a directory in the writable layer can hold changes.
                    if new.is_dir() && self.written(&path) {
                        self.dir(&path, was.is_dir(), changes)?;
                        if !changed && changes.len() > start {
                            changes.insert(start, (b'C', path));
                        }
                    }
                }
                (Some(new), None) => {
                    if self.mounts.contains(&path) {
                        continue;
                    }
                    changes.push((b'A', path.clone()));
                    if new.is_dir() {
                        self.dir(&path, false, changes)?;
                    }
                }
                (None, Some(_)) => changes.push((b'D', path)),
                (None, None) => {}
            }
        }
        Ok(())
    }

    /// Whether `rel` is a directory in the writable layer.
    fn written(&self, rel: &[u8]) -> bool {
        std::fs::symlink_metadata(OsStr::from_bytes(&join(self.upper.as_bytes(), rel)))
            .is_ok_and(|m| m.is_dir())
    }
}

/// The mount points under the root, from `/proc/self/mountinfo` (proc(5)): each line's
/// fifth field, with its octal escapes undone.
fn mounts() -> io::Result<Vec<Vec<u8>>> {
    let info = std::fs::read("/proc/self/mountinfo")?;
    Ok(info
        .split(|&b| b == b'\n')
        .filter_map(|line| line.split(|&b| b == b' ').nth(4))
        .map(unescape)
        .filter(|p| p.as_slice() != b"/")
        .collect())
}

/// A mountinfo path: `\NNN` is the byte of octal NNN (the kernel's show_path).
fn unescape(path: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(path.len());
    let mut i = 0;
    while let Some(&b) = path.get(i) {
        let octal = path
            .get(i + 1..i + 4)
            .filter(|d| b == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)))
            .and_then(|d| u8::from_str_radix(std::str::from_utf8(d).ok()?, 8).ok());
        match octal {
            Some(byte) => {
                out.push(byte);
                i += 4;
            }
            None => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// `base` and `rel` (empty, or starting with `/`).
fn join(base: &[u8], rel: &[u8]) -> Vec<u8> {
    let base = if rel.is_empty() {
        base
    } else {
        base.strip_suffix(b"/").unwrap_or(base)
    };
    [base, rel].concat()
}

/// Each entry of directory `path`, by name, as lstat(2) has it; one gone as it is read is
/// left out.
fn entries(path: &[u8]) -> io::Result<BTreeMap<OsString, Metadata>> {
    let path = std::path::Path::new(OsStr::from_bytes(path));
    let mut found = BTreeMap::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if let Ok(meta) = std::fs::symlink_metadata(entry.path()) {
            found.insert(entry.file_name(), meta);
        }
    }
    Ok(found)
}

/// go-archive's statDifferent (changes_unix.go): mode, owner and device, and for all but
/// directories mtime (sameFsTime: equal, or the same second where either has no
/// nanoseconds) and size.
fn different(was: &Metadata, new: &Metadata) -> bool {
    let same_time = (was.mtime(), was.mtime_nsec()) == (new.mtime(), new.mtime_nsec())
        || (was.mtime() == new.mtime() && (was.mtime_nsec() == 0 || new.mtime_nsec() == 0));
    was.mode() != new.mode()
        || was.uid() != new.uid()
        || was.gid() != new.gid()
        || was.rdev() != new.rdev()
        || (!new.is_dir() && (!same_time || was.size() != new.size()))
}

/// A file's `security.capability` xattr, not following a link, or none.
fn capability(path: &[u8]) -> Option<Vec<u8>> {
    let path = std::ffi::CString::new(path).ok()?;
    let name = c"security.capability";
    let mut buf = vec![0u8; 256];
    // SAFETY: lgetxattr(2) writes at most the buffer's length.
    let n = unsafe { libc::lgetxattr(path.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    let n = usize::try_from(n).ok()?;
    buf.truncate(n);
    Some(buf)
}
