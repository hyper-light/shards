//! A layer applied over a Linux tree a Windows host keeps on NTFS as WSL keeps one
//! (shards_ntfs, the way D197's shares keep their guests' files): each entry by its Linux
//! name, the characters no NTFS name holds escaped; its mode and a device's numbers in WSL's
//! `$LX` attributes; a symlink, FIFO or device WSL's reparse point, which an unelevated
//! process makes; each directory case-sensitive; an extended attribute an EA of its own; its
//! times to the 100 ns NTFS keeps. A Windows machine keeps its copy of a client's build
//! context so (D180, shards' `synced`): the client's tree, a Linux or macOS client's modes,
//! links and names with it, which NTFS's own files hold none of, read back as Linux's by
//! shards-build's walk of a tree so marked ([`shards_ntfs::LINUX_TREE`]).
//!
//! Its layers are a shards client's (`pack_changes`), applied as [`crate::apply_layer`]
//! applies one on Linux, a whiteout removing what went and an opaque one what its directory
//! held that the layer did not put there, with no owner kept (go-archive's NoLchown, as the
//! copy's walk sends none) and each extended attribute kept where NTFS can keep it
//! (BestEffortXattrs). But no name is reached through a link: each is opened relative to its
//! directory's handle, following no reparse point, so that no entry reaches past the tree,
//! and one beneath a symlink is refused, where Linux's would follow one inside the tree, as
//! a client's walk sends none.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::{AsHandle as _, BorrowedHandle, OwnedHandle};
use std::path::Path;

use shards_ntfs as ntfs;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_EA,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES,
    FILE_WRITE_EA,
};

use crate::error::{Error, quote};
use crate::gopath::posix;
use crate::tar::{
    Header, PAX_SCHILY_XATTR, Reader, TYPE_BLOCK, TYPE_CHAR, TYPE_DIR, TYPE_FIFO, TYPE_LINK, TYPE_REG,
    TYPE_SYMLINK, TYPE_XGLOBAL_HEADER, Time,
};
use crate::unpack::{bound, latest, trim_slashes};
use crate::whiteout::{WHITEOUT_META_PREFIX, WHITEOUT_OPAQUE_DIR, WHITEOUT_PREFIX};

const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFIFO: u32 = 0o010_000;
const S_IFCHR: u32 = 0o020_000;
const S_IFBLK: u32 = 0o060_000;

/// The mode of a directory a layer implies by an entry beneath it (go-archive's
/// ImpliedDirectoryMode).
const IMPLIED_DIR_MODE: u32 = 0o755;

/// What a directory's handle may do: list and open its names, and have its attributes and
/// EAs set.
const DIR_ACCESS: u32 = FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_WRITE_ATTRIBUTES | FILE_WRITE_EA;

/// ApplyUncompressedLayer of a shards client's layer, read from `input`, over the Linux tree
/// at `dest`, an existing directory, made such a tree first ([`shards_ntfs::make_linux_tree`]).
/// Returns the sum of its headers' sizes, as [`crate::apply_layer`] does.
pub fn apply_layer_lx(input: impl Read, dest: &Path) -> Result<i64, Error> {
    let root = std::fs::OpenOptions::new()
        .access_mode(DIR_ACCESS | FILE_READ_ATTRIBUTES | FILE_READ_EA)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dest)
        .map_err(|e| Error::path("open", dest.as_os_str().as_encoded_bytes(), &e))?;
    let root = OwnedHandle::from(root);
    ntfs::make_linux_tree(root.as_handle()).map_err(|e| at("make a Linux tree of", b".", e))?;
    let mut tree = Tree {
        root,
        open: Vec::new(),
    };
    let mut tr = Reader::new(input);
    let mut size: i64 = 0;
    // Directories' times, set once all beneath them is in place.
    let mut dirs: Vec<(Vec<Vec<u8>>, Time, Time)> = Vec::new();
    // What this layer wrote: an opaque whiteout keeps it.
    let mut unpacked: HashSet<Vec<u8>> = HashSet::new();
    while let Some(hdr) = tr.next_header()? {
        size = size.wrapping_add(hdr.size);
        if hdr.typeflag == TYPE_XGLOBAL_HEADER {
            continue;
        }
        let name = posix::clean(trim_slashes(&hdr.name));
        if name == b"." {
            continue;
        }
        if name == b".." || name.starts_with(b"../") {
            return Err(Error::breakout(format!(
                "invalid entry name {}",
                quote(&hdr.name)
            )));
        }
        // AUFS's metadata, which no client's layer holds, but an opaque whiteout.
        if name.starts_with(WHITEOUT_META_PREFIX) && name != WHITEOUT_OPAQUE_DIR {
            continue;
        }
        let mut path: Vec<Vec<u8>> = name.split(|&c| c == b'/').map(<[u8]>::to_vec).collect();
        let Some(base) = path.pop() else { continue };
        if base.starts_with(WHITEOUT_META_PREFIX) && base != WHITEOUT_OPAQUE_DIR {
            continue;
        }
        let dir = tree.dir(&path, true).map_err(|e| e.error(&name))?;
        if base == WHITEOUT_OPAQUE_DIR {
            let parent = path.join(&b'/');
            hide(dir, &parent, &unpacked).map_err(|e| at("RemoveAll", &parent, e))?;
            continue;
        }
        if let Some(gone) = base.strip_prefix(WHITEOUT_PREFIX) {
            remove(dir, &nt(gone)?).map_err(|e| at("RemoveAll", &name, e))?;
            continue;
        }
        let handle = create(&mut tree, &path, &base, &hdr, &mut tr).map_err(|e| e.error(&name))?;
        let made = handle.as_handle();
        if hdr.typeflag != TYPE_SYMLINK {
            for (key, value) in &hdr.pax {
                if let Some(attr) = key.strip_prefix(PAX_SCHILY_XATTR) {
                    // Kept where NTFS keeps it: a name past an EA's is dropped.
                    if let Ok(ea) = ntfs::xattr_ea(attr) {
                        let kept = [ntfs::XATTR_KEPT, value].concat();
                        let _ = ntfs::set_eas(made, &[(&ea, &kept)]);
                    }
                }
            }
        }
        let (atime, mtime) = (bound(latest(hdr.atime, hdr.mtime)), bound(hdr.mtime));
        if hdr.typeflag == TYPE_DIR {
            path.push(base);
            dirs.push((path, atime, mtime));
        } else {
            ntfs::set_times(made, Some(since(atime)), Some(since(mtime)))
                .map_err(|e| at("chtimes", &name, e))?;
        }
        unpacked.insert(name);
    }
    tree.open.clear();
    for (path, atime, mtime) in dirs {
        let name = path.join(&b'/');
        let dir = tree.dir(&path, false).map_err(|e| e.error(&name))?;
        ntfs::set_times(dir, Some(since(atime)), Some(since(mtime))).map_err(|e| at("chtimes", &name, e))?;
    }
    Ok(size)
}

/// An NT call's failure on the entry `name` in Go's words, `op name: cause`.
fn at(op: &str, name: &[u8], e: ntfs::Errno) -> Error {
    Error::path(op, name, &ntfs::io_error(e))
}

/// Why an entry could not be applied: a call's failure, or a name the tree may not hold.
#[derive(Debug)]
enum Failed {
    Call(&'static str, ntfs::Errno),
    Io(&'static str, io::Error),
    Refused(String),
}

impl Failed {
    fn error(self, name: &[u8]) -> Error {
        match self {
            Failed::Call(op, e) => at(op, name, e),
            Failed::Io(op, e) => Error::path(op, name, &e),
            Failed::Refused(why) => Error::breakout(format!("{}: {why}", quote(name))),
        }
    }
}

/// A Linux name, one component, as NTFS keeps it ([`ntfs::nt_name`]).
fn nt(name: &[u8]) -> Result<Vec<u16>, Error> {
    ntfs::nt_name(name).map_err(|e| at("name", name, e))
}

/// A time as seconds and nanoseconds since the Unix epoch, as NTFS's calls take it: one
/// before it is its start, as [`bound`] keeps a header's.
fn since(t: Time) -> (u64, u32) {
    (u64::try_from(t.sec).unwrap_or(0), t.nsec)
}

/// The tree a layer applies over: its root's handle, and the directories of the last
/// entry's parent still open, by their names, as a layer's entries come a directory at a
/// time.
struct Tree {
    root: OwnedHandle,
    open: Vec<(Vec<u8>, OwnedHandle)>,
}

impl Tree {
    /// The directory of the names `path` beneath the root, never through a link: each
    /// missing one made, where `make`, as a layer implies one (0755).
    fn dir(&mut self, path: &[Vec<u8>], make: bool) -> Result<BorrowedHandle<'_>, Failed> {
        let kept = self
            .open
            .iter()
            .zip(path)
            .take_while(|((open, _), name)| open == *name)
            .count();
        self.open.truncate(kept);
        for name in path.get(kept..).unwrap_or_default() {
            let parent = self
                .open
                .last()
                .map_or(self.root.as_handle(), |(_, h)| h.as_handle());
            let opened = open_dir(parent, name, make)?;
            self.open.push((name.clone(), opened));
        }
        Ok(self
            .open
            .last()
            .map_or(self.root.as_handle(), |(_, h)| h.as_handle()))
    }

    /// The directory of the names `path` beneath the root, opened apart from those kept
    /// open, never through a link: a hard link's target's.
    fn dir_apart(&self, path: &[Vec<u8>]) -> Result<OwnedHandle, Failed> {
        let mut at = self.root.try_clone().map_err(|e| Failed::Io("open", e))?;
        for name in path {
            at = open_dir(at.as_handle(), name, false)?;
        }
        Ok(at)
    }
}

/// The directory `name` in `parent`, made where missing and `make`; refused where it is a
/// link or anything else but a directory.
fn open_dir(parent: BorrowedHandle<'_>, name: &[u8], make: bool) -> Result<OwnedHandle, Failed> {
    let wide = ntfs::nt_name(name).map_err(|e| Failed::Call("name", e))?;
    match ntfs::open_at(parent, &wide, DIR_ACCESS, FILE_OPEN, FILE_DIRECTORY_FILE) {
        Ok(h) => {
            // Opened as itself: a junction's or an NTFS symlink's directory is a link.
            let st = ntfs::stat(h.as_handle()).map_err(|e| Failed::Call("lstat", e))?;
            if st.mode & S_IFMT != S_IFDIR {
                return Err(Failed::Refused("a name beneath a link".into()));
            }
            Ok(h)
        }
        Err(ntfs::ENOENT) if make => {
            let h = ntfs::mkdir_at(parent, &wide).map_err(|e| Failed::Call("mkdirat", e))?;
            ntfs::set_lx(h.as_handle(), None, None, Some(S_IFDIR | IMPLIED_DIR_MODE), None)
                .map_err(|e| Failed::Call("chmod", e))?;
            Ok(h)
        }
        // A symlink or a file where a directory is named: a name beneath it is refused.
        Err(ntfs::ENOTDIR) => Err(Failed::Refused("a name beneath a file or a link".into())),
        Err(e) => Err(Failed::Call("openat", e)),
    }
}

/// createTarFile of the entry `hdr` as `base` in the directory `path`, its data read from
/// `data`, over whatever was there but a directory a directory merges with; its handle,
/// its mode set, a symlink's too.
fn create(
    tree: &mut Tree,
    path: &[Vec<u8>],
    base: &[u8],
    hdr: &Header,
    data: &mut impl Read,
) -> Result<OwnedHandle, Failed> {
    let wide = ntfs::nt_name(base).map_err(|e| Failed::Call("name", e))?;
    // A hard link's target, its directory opened before anything of its own name goes.
    let target = if hdr.typeflag == TYPE_LINK {
        let cleaned = posix::clean(trim_slashes(&hdr.linkname));
        if cleaned == b"." || cleaned == b".." || cleaned.starts_with(b"../") {
            return Err(Failed::Refused(format!(
                "invalid hardlink target {}",
                quote(&hdr.linkname)
            )));
        }
        let mut parts: Vec<Vec<u8>> = cleaned.split(|&c| c == b'/').map(<[u8]>::to_vec).collect();
        let name = parts.pop().unwrap_or_default();
        let old = ntfs::nt_name(&name).map_err(|e| Failed::Call("name", e))?;
        Some((tree.dir_apart(&parts)?, old))
    } else {
        None
    };
    let dir = tree.dir(path, true)?;
    let perm = u32::try_from(hdr.mode & 0o7777).unwrap_or(0);
    let kept = ntfs::open_at(dir, &wide, FILE_READ_ATTRIBUTES, FILE_OPEN, 0);
    match kept {
        Ok(h) => {
            let is_dir = ntfs::stat(h.as_handle())
                .map_err(|e| Failed::Call("lstat", e))?
                .mode
                & S_IFMT
                == S_IFDIR;
            drop(h);
            if is_dir && hdr.typeflag == TYPE_DIR {
                let h = ntfs::open_at(dir, &wide, DIR_ACCESS, FILE_OPEN, FILE_DIRECTORY_FILE)
                    .map_err(|e| Failed::Call("openat", e))?;
                ntfs::set_lx(h.as_handle(), None, None, Some(S_IFDIR | perm), None)
                    .map_err(|e| Failed::Call("chmod", e))?;
                return Ok(h);
            }
            remove(dir, &wide).map_err(|e| Failed::Call("RemoveAll", e))?;
        }
        Err(ntfs::ENOENT) => {}
        Err(e) => return Err(Failed::Call("lstat", e)),
    }
    let mode = |kind: u32| Some(kind | perm);
    let made = match hdr.typeflag {
        TYPE_DIR => {
            let h = ntfs::mkdir_at(dir, &wide).map_err(|e| Failed::Call("mkdirat", e))?;
            ntfs::set_lx(h.as_handle(), None, None, mode(S_IFDIR), None)
                .map_err(|e| Failed::Call("chmod", e))?;
            h
        }
        TYPE_REG => {
            let h = ntfs::open_at(
                dir,
                &wide,
                FILE_GENERIC_WRITE,
                FILE_CREATE,
                FILE_NON_DIRECTORY_FILE,
            )
            .map_err(|e| Failed::Call("openat", e))?;
            let mut file = std::fs::File::from(h);
            copy(data, &mut file)?;
            let h = OwnedHandle::from(file);
            ntfs::set_lx(h.as_handle(), None, None, mode(S_IFREG), None)
                .map_err(|e| Failed::Call("chmod", e))?;
            h
        }
        TYPE_SYMLINK => {
            // Its mode its header's too, which WSL's metadata keeps of a symlink, where Linux
            // keeps every one 0777: a Windows client's are fsutil's 0755, and an agent made of
            // the copy was not the one the client made (tests/context.rs, at 41c3560d).
            let h = ntfs::symlink_at(dir, &wide, &hdr.linkname).map_err(|e| Failed::Call("symlinkat", e))?;
            ntfs::set_lx(h.as_handle(), None, None, mode(S_IFLNK), None)
                .map_err(|e| Failed::Call("chmod", e))?;
            h
        }
        TYPE_BLOCK | TYPE_CHAR | TYPE_FIFO => {
            let kind = match hdr.typeflag {
                TYPE_BLOCK => S_IFBLK,
                TYPE_CHAR => S_IFCHR,
                _ => S_IFIFO,
            };
            let (Ok(major), Ok(minor)) = (u32::try_from(hdr.devmajor), u32::try_from(hdr.devminor)) else {
                return Err(Failed::Refused(format!(
                    "device number {}:{} out of range: invalid archive",
                    hdr.devmajor, hdr.devminor
                )));
            };
            let h =
                ntfs::special_at(dir, &wide, kind, (major, minor)).map_err(|e| Failed::Call("mknodat", e))?;
            ntfs::set_lx(h.as_handle(), None, None, mode(kind), None)
                .map_err(|e| Failed::Call("chmod", e))?;
            h
        }
        TYPE_LINK => {
            let Some((odir, old)) = target else {
                return Err(Failed::Refused("a hard link with no target".into()));
            };
            ntfs::link_at(odir.as_handle(), &old, dir, &wide).map_err(|e| Failed::Call("linkat", e))?;
            let h = ntfs::open_at(dir, &wide, FILE_WRITE_ATTRIBUTES | FILE_WRITE_EA, FILE_OPEN, 0)
                .map_err(|e| Failed::Call("openat", e))?;
            let st = ntfs::stat(h.as_handle()).map_err(|e| Failed::Call("lstat", e))?;
            // handleLChmod: no mode for a hard link to a symlink.
            if st.mode & S_IFMT != S_IFLNK {
                ntfs::set_lx(h.as_handle(), None, None, Some((st.mode & S_IFMT) | perm), None)
                    .map_err(|e| Failed::Call("chmod", e))?;
            }
            h
        }
        flag => return Err(Failed::Refused(format!("unhandled tar header type {flag}"))),
    };
    Ok(made)
}

/// The entry's data written to its file whole.
fn copy(data: &mut impl Read, file: &mut std::fs::File) -> Result<(), Failed> {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        let n = match data.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Failed::Io("read", e)),
        };
        file.write_all(buf.get(..n).unwrap_or_default())
            .map_err(|e| Failed::Io("write", e))?;
    }
}

/// `name` in `dir` removed, a directory with all it holds, never through a link (a link is
/// removed itself): what a whiteout or a replaced entry takes. Nothing where it is not there.
fn remove(dir: BorrowedHandle<'_>, name: &[u16]) -> Result<(), ntfs::Errno> {
    let opened = match ntfs::open_at(dir, name, FILE_READ_ATTRIBUTES, FILE_OPEN, 0) {
        Ok(h) => h,
        Err(ntfs::ENOENT) => return Ok(()),
        Err(e) => return Err(e),
    };
    let is_dir = ntfs::stat(opened.as_handle())?.mode & S_IFMT == S_IFDIR;
    drop(opened);
    if !is_dir {
        return unlink(dir, name);
    }
    // Each directory beneath it open, with its name in the one before and the directories
    // it holds still to go, its other entries gone as it is listed: emptied deepest first,
    // no recursion to exhaust the stack.
    let mut stack = vec![emptying(dir, name)?];
    loop {
        let next = match stack.last_mut() {
            None => return Ok(()),
            Some(top) => match top.dirs.pop() {
                Some(sub) => Some(emptying(top.handle.as_handle(), &sub)?),
                None => None,
            },
        };
        match next {
            Some(frame) => stack.push(frame),
            None => {
                let Some(done) = stack.pop() else { return Ok(()) };
                drop(done.handle);
                let parent = stack.last().map_or(dir, |f| f.handle.as_handle());
                ntfs::unlink_at(parent, &done.name, true)?;
            }
        }
    }
}

/// `name` in `dir` removed, anything but a directory: a link to one too (a junction, an
/// NTFS symlink to a directory), which NT removes as a directory, itself and not what it
/// names.
fn unlink(dir: BorrowedHandle<'_>, name: &[u16]) -> Result<(), ntfs::Errno> {
    match ntfs::unlink_at(dir, name, false) {
        Err(ntfs::EISDIR) => ntfs::unlink_at(dir, name, true),
        r => r,
    }
}

/// A directory being emptied ([`remove`]): its handle, its name in its parent and the
/// directories in it still to go.
struct Emptying {
    handle: OwnedHandle,
    name: Vec<u16>,
    dirs: Vec<Vec<u16>>,
}

/// The directory `name` in `dir` opened and listed once, each entry but a directory removed.
fn emptying(dir: BorrowedHandle<'_>, name: &[u16]) -> Result<Emptying, ntfs::Errno> {
    let handle = ntfs::open_at(
        dir,
        name,
        FILE_LIST_DIRECTORY | FILE_TRAVERSE,
        FILE_OPEN,
        FILE_DIRECTORY_FILE,
    )?;
    let mut dirs = Vec::new();
    for entry in listed(handle.as_handle())? {
        let wide = ntfs::nt_name(&entry.name)?;
        if entry.kind == S_IFDIR {
            dirs.push(wide);
        } else {
            unlink(handle.as_handle(), &wide)?;
        }
    }
    Ok(Emptying {
        handle,
        name: name.to_vec(),
        dirs,
    })
}

/// Every entry of the directory `h` but `.` and `..`.
fn listed(h: BorrowedHandle<'_>) -> Result<Vec<ntfs::Listed>, ntfs::Errno> {
    let mut all = Vec::new();
    let mut restart = true;
    loop {
        let got = ntfs::list(h, restart)?;
        restart = false;
        if got.is_empty() {
            break;
        }
        all.extend(got.into_iter().filter(|e| e.name != b"." && e.name != b".."));
    }
    Ok(all)
}

/// The opaque whiteout's walk of the directory `dir` (`parent`, root-relative): whatever
/// this layer did not write is removed, what it wrote kept, directories it wrote walked too.
fn hide(dir: BorrowedHandle<'_>, parent: &[u8], unpacked: &HashSet<Vec<u8>>) -> Result<(), ntfs::Errno> {
    let mut pending: Vec<(Option<OwnedHandle>, Vec<u8>)> = vec![(None, parent.to_vec())];
    while let Some((held, at)) = pending.pop() {
        let here = held.as_ref().map_or(dir, |h| h.as_handle());
        for entry in listed(here)? {
            let rel = if at.is_empty() {
                entry.name.clone()
            } else {
                [at.as_slice(), b"/", &entry.name].concat()
            };
            let wide = ntfs::nt_name(&entry.name)?;
            if !unpacked.contains(&rel) {
                remove(here, &wide)?;
            } else if entry.kind == S_IFDIR {
                let h = ntfs::open_at(
                    here,
                    &wide,
                    FILE_LIST_DIRECTORY | FILE_TRAVERSE,
                    FILE_OPEN,
                    FILE_DIRECTORY_FILE,
                )?;
                pending.push((Some(h), rel));
            }
        }
    }
    Ok(())
}
