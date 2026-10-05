//! go-archive's Unpack (archive.go: Unpack, createTarFile, resolveArchivePath,
//! resolveHardlinkTarget, createImpliedDirectories; rootpath.go: resolveFSRootPath;
//! archive_unix.go: handleTarTypeBlockCharFifo, handleLChmod): an archive written into a
//! directory through Go's os.Root ([`crate::sys::Root`]), so no name, symlink or hard link
//! in it reaches outside the directory.
//!
//! Names are cleaned with any leading `/` dropped; a name that would leave the
//! destination is an error. A symlink inside the destination may be followed by later
//! entries as long as it stays inside; an absolute one is resolved as if the destination
//! were `/`. Existing files are replaced, existing directories merged; ownership, modes,
//! extended attributes and times are applied as go-archive applies them, directories'
//! times last.
//!
//! Where go-archive differs on Windows, so does this: no owners, modes, devices or
//! extended attributes, and names Windows cannot hold skipped. Devices and FIFOs, which
//! go-archive accepts there and then fails on setting their times, are skipped too.

use std::io::{self, Read, Write};
use std::path::Path;

use crate::error::{Error, Kind, quote};
use crate::gopath::{self, NATIVE, Os, posix};
use crate::root::WalkError;
use crate::sys::{self, FileKind, Root};
use crate::tar::{
    Header, PAX_SCHILY_XATTR, Reader, TYPE_BLOCK, TYPE_CHAR, TYPE_DIR, TYPE_FIFO, TYPE_LINK, TYPE_REG,
    TYPE_SYMLINK, TYPE_XGLOBAL_HEADER, Time,
};

/// ImpliedDirectoryMode: directories made for entries whose parents have none.
const IMPLIED_DIR_MODE: u32 = 0o755;

/// TarOptions' fields that unpacking reads.
#[derive(Debug, Clone, Default)]
pub struct UnpackOptions {
    /// NoLchown: leave everything owned by whoever unpacks.
    pub no_lchown: bool,
    /// ChownOpts: the owner of every entry instead of the archive's.
    pub chown: Option<(i64, i64)>,
    /// NoOverwriteDirNonDir: an entry may not replace a directory with a non-directory,
    /// or the reverse.
    pub no_overwrite_dir_non_dir: bool,
    /// BestEffortXattrs: attributes the filesystem does not support are dropped.
    pub best_effort_xattrs: bool,
    /// ExcludePatterns as Unpack applies them: prefixes of cleaned names.
    pub exclude_patterns: Vec<Vec<u8>>,
}

/// Untar: `dest` cleaned, then [`unpack`]. Archives must be uncompressed.
pub fn untar(input: impl Read, dest: &Path, opts: &UnpackOptions) -> Result<(), Error> {
    let dest = gopath::clean(NATIVE, &sys::path_bytes(dest));
    unpack(input, sys::os_path_buf(&dest).as_path(), opts)
}

/// Unpack: the archive read from `input`, written into the directory `dest`.
pub fn unpack(input: impl Read, dest: &Path, opts: &UnpackOptions) -> Result<(), Error> {
    let dest = sys::path_bytes(dest);
    let root = Root::open(&dest)?;
    let mut tr = Reader::new(input);
    let mut dirs: Vec<(Header, Vec<u8>)> = Vec::new();
    'entries: while let Some(mut hdr) = tr.next_header()? {
        if hdr.typeflag == TYPE_XGLOBAL_HEADER {
            continue;
        }
        let name = posix::clean(trim_slashes(&hdr.name));
        if name == b"." {
            continue;
        }
        if !gopath::is_local(NATIVE, &name) {
            return Err(Error::breakout(format!(
                "invalid entry name {}",
                quote(&hdr.name)
            )));
        }
        for exclude in &opts.exclude_patterns {
            if name.starts_with(exclude) {
                continue 'entries;
            }
        }
        hdr.name = name;
        if unrepresentable(&hdr) {
            continue;
        }
        let dst = resolve_archive_path(&root, &gopath::from_slash(NATIVE, &hdr.name))?;
        if let Ok(fi) = root.lstat(&dst) {
            let is_dir = fi.kind == FileKind::Dir;
            if opts.no_overwrite_dir_non_dir && is_dir && hdr.typeflag != TYPE_DIR {
                return Err(Error::other(format!(
                    "cannot overwrite directory {} with non-directory {}",
                    quote(&hdr.name),
                    quote(&dest)
                )));
            }
            if opts.no_overwrite_dir_non_dir && !is_dir && hdr.typeflag == TYPE_DIR {
                return Err(Error::other(format!(
                    "cannot overwrite non-directory {} with directory {}",
                    quote(&hdr.name),
                    quote(&dest)
                )));
            }
            if !is_dir || hdr.typeflag != TYPE_DIR {
                root.remove_all(&dst).map_err(|e| e.error("RemoveAll", &dst))?;
            }
        }
        implied_directories(&root, &dst, opts)?;
        create(&root, &dst, &hdr, &mut tr, opts)?;
        if hdr.typeflag == TYPE_DIR {
            dirs.push((hdr, dst));
        }
    }
    for (hdr, dst) in dirs {
        let atime = bound(latest(hdr.atime, hdr.mtime));
        root.chtimes(&dst, atime, bound(hdr.mtime))
            .map_err(|e| e.error("chtimesat", &dst))?;
    }
    Ok(())
}

fn trim_slashes(name: &[u8]) -> &[u8] {
    let start = name.iter().position(|&c| c != b'/').unwrap_or(name.len());
    name.get(start..).unwrap_or_default()
}

/// unrepresentableOnWindows: `:` and `\` in a name, or in a hard link's target, cannot
/// be a Windows path's; and on Windows devices and FIFOs are skipped (see the module).
fn unrepresentable(hdr: &Header) -> bool {
    if NATIVE != Os::Windows {
        return false;
    }
    let bad = |s: &[u8]| s.iter().any(|&c| c == b':' || c == b'\\');
    bad(&hdr.name)
        || (hdr.typeflag == TYPE_LINK && bad(&hdr.linkname))
        || matches!(hdr.typeflag, TYPE_CHAR | TYPE_BLOCK | TYPE_FIFO)
}

/// The latest of two times.
fn latest(a: Time, b: Time) -> Time {
    if a < b { b } else { a }
}

/// boundTime: a time os.Chtimes cannot set (before 1970, after 2262) becomes 1970.
fn bound(t: Time) -> Time {
    let min = Time::unix(0, 0);
    let max = Time::unix(0, i64::MAX);
    if t < min || t > max { min } else { t }
}

/// resolveArchivePath: `name` with its parent's symlinks resolved as if the root were
/// `/` where os.Root alone would refuse an absolute one, or where a parent is missing.
/// A relative symlink that leaves the root stays refused.
fn resolve_archive_path(root: &Root, name: &[u8]) -> Result<Vec<u8>, Error> {
    let (parent, base) = gopath::split(NATIVE, name);
    if parent.is_empty() {
        return Ok(name.to_vec());
    }
    let parent = gopath::clean(NATIVE, parent);
    let stat_err = match root.stat(&parent) {
        Ok(_) => return Ok(name.to_vec()),
        Err(e) if !e.is_not_found() && !e.escapes() => return Err(e.error("statat", &parent)),
        Err(e) => e,
    };
    let resolved = resolve_fs_root_path(root.name(), &parent)?;
    if stat_err.escapes() && (!resolved.followed_absolute || resolved.relative_escape_first) {
        return Err(stat_err.error("statat", &parent));
    }
    let Some(rel_parent) = gopath::rel(NATIVE, root.name(), &resolved.path) else {
        return Err(Error::breakout(format!(
            "could not make resolved parent {} relative to root {}: Rel: can't make {} relative to {}",
            quote(&resolved.path),
            quote(root.name()),
            String::from_utf8_lossy(&resolved.path),
            String::from_utf8_lossy(root.name()),
        )));
    };
    if rel_parent != b"." && !gopath::is_local(NATIVE, &rel_parent) {
        return Err(Error::breakout(format!(
            "resolved parent {} escapes root {}",
            quote(&resolved.path),
            quote(root.name())
        )));
    }
    Ok(gopath::join(NATIVE, &[&rel_parent, base]))
}

/// resolveHardlinkTarget: a target cleaned, an absolute one taken from the root, and
/// refused unless it stays inside.
fn resolve_hardlink_target(root: &Root, linkname: &[u8]) -> Result<Vec<u8>, Error> {
    let mut cleaned = posix::clean(linkname);
    if cleaned.starts_with(b"/") {
        cleaned = posix::clean(trim_slashes(linkname));
    }
    if cleaned == b"." || !gopath::is_local(NATIVE, &cleaned) {
        return Err(Error::breakout(format!(
            "invalid hardlink target {}",
            quote(linkname)
        )));
    }
    resolve_archive_path(root, &gopath::from_slash(NATIVE, &cleaned))
}

/// What resolveFSRootPath found.
struct Resolved {
    path: Vec<u8>,
    followed_absolute: bool,
    relative_escape_first: bool,
}

/// resolveFSRootPath (rootpath.go, after containerd's fs.RootPath): `path` under `root`
/// with every symlink resolved as if `root` were `/`. Paths only: os.Root checks what is
/// done with the result.
fn resolve_fs_root_path(root: &[u8], path: &[u8]) -> Result<Resolved, Error> {
    let mut r = Resolved {
        path: root.to_vec(),
        followed_absolute: false,
        relative_escape_first: false,
    };
    if path.is_empty() {
        return Ok(r);
    }
    let sep = [NATIVE.sep()];
    let mut path = path.to_vec();
    let mut links = 0usize;
    loop {
        let before = links;
        let new = walk_links(root, &path, &mut links, &mut r)?;
        path = new;
        if before == links {
            let rooted = gopath::join(NATIVE, &[&sep, &path]);
            if path == rooted {
                r.path = gopath::join(NATIVE, &[root, &rooted]);
                return Ok(r);
            }
            path = rooted;
        }
    }
}

/// walkLink: one component's symlink, if it is one.
fn walk_link(
    root: &[u8],
    path: &[u8],
    links: &mut usize,
    r: &mut Resolved,
) -> Result<(Vec<u8>, bool), Error> {
    if *links > 255 {
        return Err(Error::other("too many links"));
    }
    let sep = [NATIVE.sep()];
    let path = gopath::join(NATIVE, &[&sep, path]);
    if path == sep {
        return Ok((path, false));
    }
    let real = gopath::join(NATIVE, &[root, &path]);
    let st = match sys::lstat(&real) {
        Ok(st) => st,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((path, false)),
        Err(e) => return Err(Error::path("lstat", &real, &e)),
    };
    if st.kind != FileKind::Symlink {
        return Ok((path, false));
    }
    let target = sys::readlink(&real).map_err(|e| Error::path("readlink", &real, &e))?;
    if gopath::is_abs(NATIVE, &target) {
        r.followed_absolute = true;
    } else if !r.followed_absolute {
        let rel_dir = gopath::rel(NATIVE, &sep, &gopath::dir(NATIVE, &path)).unwrap_or_default();
        let resolved = gopath::join(NATIVE, &[&rel_dir, &target]);
        if resolved != b"." && !gopath::is_local(NATIVE, &resolved) {
            r.relative_escape_first = true;
        }
    }
    *links += 1;
    Ok((target, true))
}

/// walkLinks, without its recursion: a name of many components must not exhaust the
/// stack. The first component is resolved first, each later one in what came before.
fn walk_links(root: &[u8], path: &[u8], links: &mut usize, r: &mut Resolved) -> Result<Vec<u8>, Error> {
    let sep = NATIVE.sep();
    let mut rest: Vec<&[u8]> = Vec::new();
    let mut p = path;
    let mut dir = loop {
        let (d, file) = gopath::split(NATIVE, p);
        if d.is_empty() {
            break walk_link(root, file, links, r)?.0;
        }
        if file.is_empty() {
            if d.last().is_some_and(|&c| NATIVE.is_sep(c)) {
                if d == [sep] {
                    break d.to_vec();
                }
                p = d.get(..d.len() - 1).unwrap_or_default();
                continue;
            }
            break walk_link(root, d, links, r)?.0;
        }
        rest.push(file);
        p = d;
    };
    while let Some(file) = rest.pop() {
        let (new, is_link) = walk_link(root, &gopath::join(NATIVE, &[&dir, file]), links, r)?;
        dir = if !is_link || gopath::is_abs(NATIVE, &new) {
            new
        } else {
            gopath::join(NATIVE, &[&dir, &new])
        };
    }
    Ok(dir)
}

/// createImpliedDirectories: the missing parents of `dst`, mode 0755 and the root's
/// owner, as an archive without their entries implies them.
fn implied_directories(root: &Root, dst: &[u8], opts: &UnpackOptions) -> Result<(), Error> {
    let parent = gopath::dir(NATIVE, dst);
    if parent == b"." || parent.is_empty() {
        return Ok(());
    }
    match root.lstat(&parent) {
        Ok(_) => return Ok(()),
        Err(e) if !e.is_not_found() => return Err(e.error("statat", &parent)),
        Err(_) => {}
    }
    let mut cur: Vec<u8> = Vec::new();
    for c in parent.split(|&c| c == NATIVE.sep()) {
        if c.is_empty() {
            continue;
        }
        cur = gopath::join(NATIVE, &[&cur, c]);
        match root.mkdir(&cur, IMPLIED_DIR_MODE) {
            Ok(()) => {}
            Err(WalkError::Os(e)) if e.kind() == io::ErrorKind::AlreadyExists => {
                let fi = root.stat(&cur).map_err(|e| e.error("statat", &cur))?;
                if fi.kind == FileKind::Dir {
                    continue;
                }
                return Err(Error::other(format!(
                    "mkdir {}: not a directory",
                    String::from_utf8_lossy(&cur)
                )));
            }
            Err(e) => return Err(e.error("mkdirat", &cur)),
        }
        if opts.no_lchown {
            continue;
        }
        // Mkdir's mode is under the umask; the mode is set again so it is not.
        sys::fix_implied_dir(root, &cur, IMPLIED_DIR_MODE)?;
    }
    Ok(())
}

/// createTarFile: one entry, then its owner, attributes, mode and times.
fn create(
    root: &Root,
    dst: &[u8],
    hdr: &Header,
    data: &mut impl Read,
    opts: &UnpackOptions,
) -> Result<(), Error> {
    let target = if hdr.typeflag == TYPE_LINK {
        resolve_hardlink_target(root, &hdr.linkname)?
    } else {
        Vec::new()
    };
    let perm = (hdr.mode & 0o777) as u32;
    match hdr.typeflag {
        TYPE_DIR => {
            if !root.lstat(dst).is_ok_and(|fi| fi.kind == FileKind::Dir) {
                root.mkdir(dst, perm).map_err(|e| e.error("mkdirat", dst))?;
            }
        }
        TYPE_REG => {
            let mut file = root.create(dst, perm).map_err(|e| e.error("openat", dst))?;
            copy_data(data, &mut file, &join_path(root.name(), dst))?;
        }
        TYPE_BLOCK | TYPE_CHAR | TYPE_FIFO => device(root, dst, hdr)?,
        TYPE_LINK => root
            .link(&target, dst)
            .map_err(|e| link_error("linkat", &target, dst, e))?,
        TYPE_SYMLINK => root
            .symlink(&hdr.linkname, dst)
            .map_err(|e| link_error("symlinkat", &hdr.linkname, dst, e))?,
        TYPE_XGLOBAL_HEADER => return Ok(()),
        flag => return Err(Error::other(format!("unhandled tar header type {flag}"))),
    }

    if !opts.no_lchown && NATIVE != Os::Windows {
        let (uid, gid) = opts.chown.unwrap_or((hdr.uid, hdr.gid));
        root.lchown(dst, uid, gid).map_err(|e| {
            e.error("lchownat", dst).wrap(&format!(
                "failed to Lchown {} for UID {}, GID {}",
                quote(dst),
                hdr.uid,
                hdr.gid
            ))
        })?;
    }

    let (dir, base) = parent_and_base(dst);
    for (key, value) in &hdr.pax {
        let Some(attr) = key.strip_prefix(PAX_SCHILY_XATTR) else {
            continue;
        };
        if NATIVE == Os::Windows {
            continue;
        }
        let r = root
            .set_xattr(&dir, &base, attr, value)
            .map_err(|e| e.error("openat", &dir))?;
        if let Err(e) = r {
            let skip = (opts.best_effort_xattrs && is_errno(&e, sys::ENOTSUP)) || is_errno(&e, sys::EPERM);
            if !skip {
                return Err(Error::new(
                    Kind::Other,
                    format!(
                        "lsetxattr {}: xattr {}: {}",
                        String::from_utf8_lossy(&join_path(root.name(), dst)),
                        quote(attr),
                        crate::error::errno_text(&e)
                    ),
                ));
            }
        }
    }

    // handleLChmod: no mode for a symlink, nor for a hard link to one.
    if NATIVE != Os::Windows {
        let chmod = match hdr.typeflag {
            TYPE_SYMLINK => false,
            TYPE_LINK => root.lstat(&target).is_ok_and(|fi| fi.kind != FileKind::Symlink),
            _ => true,
        };
        if chmod {
            let mode = (hdr.mode & 0o7777) as u32;
            root.chmod_nofollow(&dir, &base, mode)
                .map_err(|e| e.error("openat", &dir))?
                .map_err(|e| Error::path("fchmodat2", dst, &e))?;
        }
    }

    let atime = bound(latest(hdr.atime, hdr.mtime));
    let mtime = bound(hdr.mtime);
    match hdr.typeflag {
        TYPE_SYMLINK => {
            if NATIVE != Os::Windows {
                root.lchtimes(&dir, &base, atime, mtime)
                    .map_err(|e| e.error("openat", &dir))?
                    .map_err(|e| Error::path("lchtimes", dst, &e))?;
            }
        }
        TYPE_LINK => {
            if root.lstat(&target).is_ok_and(|fi| fi.kind != FileKind::Symlink) {
                root.chtimes(dst, atime, mtime)
                    .map_err(|e| e.error("chtimesat", dst))?;
            }
        }
        _ => root
            .chtimes(dst, atime, mtime)
            .map_err(|e| e.error("chtimesat", dst))?,
    }
    Ok(())
}

/// handleTarTypeBlockCharFifo: the node made in its parent, its device numbers checked
/// to fit the system's, as the header is untrusted.
fn device(root: &Root, dst: &[u8], hdr: &Header) -> Result<(), Error> {
    let fmt = match hdr.typeflag {
        TYPE_BLOCK => sys::S_IFBLK,
        TYPE_CHAR => sys::S_IFCHR,
        _ => sys::S_IFIFO,
    };
    let mode = (hdr.mode & 0o7777) as u32 | fmt;
    let (Ok(major), Ok(minor)) = (u32::try_from(hdr.devmajor), u32::try_from(hdr.devminor)) else {
        return Err(Error::other(format!(
            "device number {}:{} for {} out of range: invalid archive",
            hdr.devmajor,
            hdr.devminor,
            quote(&hdr.name)
        )));
    };
    let (dir, base) = parent_and_base(dst);
    root.mknod(&dir, &base, mode, major, minor)
        .map_err(|e| e.error("openat", &dir))?
        .map_err(|e| Error::io(&e))
}

/// filepath.Dir and filepath.Base of a root-relative name.
fn parent_and_base(name: &[u8]) -> (Vec<u8>, Vec<u8>) {
    (gopath::dir(NATIVE, name), gopath::base(NATIVE, name))
}

/// os.Root's joinPath: how an open file names itself.
fn join_path(dir: &[u8], name: &[u8]) -> Vec<u8> {
    if dir.last().is_some_and(|&c| NATIVE.is_sep(c)) {
        return [dir, name].concat();
    }
    [dir, b"/", name].concat()
}

fn link_error(op: &str, old: &[u8], new: &[u8], e: WalkError) -> Error {
    e.link_error(op, old, new)
}

fn is_errno(e: &io::Error, n: i32) -> bool {
    e.raw_os_error() == Some(n)
}

/// copyWithBuffer: the entry's data into the file. The archive's errors are its own;
/// the file's are Go's PathError of `write`.
fn copy_data(data: &mut impl Read, file: &mut impl Write, name: &[u8]) -> Result<(), Error> {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        let n = match data.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(crate::tar::read_error(&e)),
        };
        file.write_all(buf.get(..n).unwrap_or_default())
            .map_err(|e| Error::path("write", name, &e))?;
    }
}
