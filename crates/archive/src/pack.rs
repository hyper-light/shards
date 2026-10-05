//! go-archive's TarWithOptions (archive.go: Tarballer.Do, tarAppender.addTarFile,
//! FileInfoHeader), with tarheader.FileInfoHeaderNoLookups and Go's tar.FileInfoHeader:
//! the files under a directory, walked in lexical order as filepath.WalkDir walks them,
//! each named relative to it with `/` separators and a trailing `/` for directories,
//! owners as numbers without names, modification times in whole seconds, no access or
//! change times, hard links to the first name of an inode, and security.capability as a
//! PAX record.
//!
//! Where go-archive logs an error and goes on, so does this, skipping the file: one that
//! vanished, a socket. Where go-archive would leave a broken archive behind a log line (a
//! file that shrank or could not be opened after its header was written, an unreadable
//! source directory, a pattern that cannot be matched), this fails instead.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::Path;

use crate::error::Error;
use crate::gopath::{self, NATIVE, Os};
use crate::patterns::{MatchInfo, PatternMatcher};
use crate::sys::{self, FileKind, Stat};
use crate::tar::{
    Format, Header, PAX_SCHILY_XATTR, TYPE_BLOCK, TYPE_CHAR, TYPE_DIR, TYPE_FIFO, TYPE_LINK, TYPE_REG,
    TYPE_SYMLINK, Time, Writer,
};
use crate::whiteout::{self, Overlay, WhiteoutFormat};

/// TarOptions' fields that making an archive reads.
#[derive(Debug, Clone, Default)]
pub struct PackOptions {
    /// IncludeFiles: paths under the source to archive, `.` for all of it (the default).
    pub include_files: Vec<Vec<u8>>,
    /// ExcludePatterns, as moby/patternmatcher reads them; `!` makes an exception.
    pub exclude_patterns: Vec<Vec<u8>>,
    /// ChownOpts: the owner every header gets instead of the file's.
    pub chown: Option<(i64, i64)>,
    /// IncludeSourceDir: the source directory itself is an entry, `./` with `.` included.
    pub include_source_dir: bool,
    /// RebaseNames: for an include, what its first occurrence in each name becomes.
    pub rebase_names: BTreeMap<Vec<u8>, Vec<u8>>,
    /// WhiteoutFormat: with Overlay, an overlayfs upper directory's whiteouts and opaque
    /// directories are archived as `.wh.` files.
    pub whiteout: WhiteoutFormat,
}

/// Writes the archive of `src` to `out`: TarWithOptions. A source that is not a directory
/// is archived as itself, under its base name.
pub fn pack<W: Write>(src: &Path, opts: &PackOptions, out: W) -> Result<W, Error> {
    let pm = PatternMatcher::new(&opts.exclude_patterns)?;
    let mut src_path = sys::path_bytes(src);
    let mut includes = opts.include_files.clone();
    let st = sys::lstat(&src_path).map_err(|e| Error::path("lstat", &src_path, &e))?;
    if st.kind != FileKind::Dir {
        let (dir, base) = crate::copy::split_path_dir_entry_os(NATIVE, &src_path);
        src_path = dir;
        includes = vec![base];
    }
    if includes.is_empty() {
        includes = vec![b".".to_vec()];
    }
    let mut packer = Packer {
        tw: Writer::new(out),
        seen_inodes: HashMap::new(),
        chown: opts.chown,
        whiteout: whiteout::converter(opts.whiteout),
    };
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let sep = NATIVE.sep();
    for include in &includes {
        let rebase = opts.rebase_names.get(include).filter(|r| !r.is_empty());
        let mut parent_dirs: Vec<Vec<u8>> = Vec::new();
        let mut parent_info: Vec<MatchInfo> = Vec::new();
        let walk_root = walk_root(&src_path, include);
        walk_dir(&walk_root, &mut |file_path: &[u8],
                                   is_dir: bool|
         -> Result<Flow, Error> {
            let Some(mut rel) = gopath::rel(NATIVE, &src_path, file_path) else {
                return Ok(Flow::Continue);
            };
            if !opts.include_source_dir && rel == b"." && is_dir {
                return Ok(Flow::Continue);
            }
            if opts.include_source_dir && include == b"." && rel != b"." {
                rel = [b".".as_slice(), &[sep], &rel].concat();
            }
            let mut skip = false;
            if *include != rel {
                while let Some(last) = parent_dirs.last() {
                    if rel.starts_with(&[last.as_slice(), &[sep]].concat()) {
                        break;
                    }
                    parent_dirs.pop();
                    parent_info.pop();
                }
                let parent = parent_info.last().cloned().unwrap_or_default();
                let (excluded, info) = pm.matches_using_parent_results(&rel, &parent)?;
                skip = excluded;
                if is_dir {
                    parent_dirs.push(rel.clone());
                    parent_info.push(info);
                }
            }
            if skip {
                if !is_dir {
                    return Ok(Flow::Continue);
                }
                if !pm.exclusions() {
                    return Ok(Flow::SkipDir);
                }
                let dir_slash = [rel.as_slice(), &[sep]].concat();
                for pat in pm.patterns() {
                    if pat.exclusion() && [pat.text(), &[sep]].concat().starts_with(&dir_slash) {
                        return Ok(Flow::Continue);
                    }
                }
                return Ok(Flow::SkipDir);
            }
            if !seen.insert(rel.clone()) {
                return Ok(Flow::Continue);
            }
            if let Some(rebase) = rebase {
                let replacement: &[u8] = if rebase.as_slice() == [sep] { b"" } else { rebase };
                rel = replace_first(&rel, include, replacement);
            }
            packer.add(file_path, &rel)?;
            Ok(Flow::Continue)
        })?;
    }
    packer.tw.finish()
}

/// What the walk does after a file: go on, or skip the rest of a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    SkipDir,
}

/// A directory being walked, and its entries left to visit.
type Listing = (Vec<u8>, std::vec::IntoIter<(Vec<u8>, bool)>);

/// filepath.WalkDir: `root`, then, for a directory, its entries in lexical order, never
/// following symlinks. Errors listing or stating a file skip it, as go-archive's callback
/// logs them and goes on.
fn walk_dir(root: &[u8], f: &mut impl FnMut(&[u8], bool) -> Result<Flow, Error>) -> Result<(), Error> {
    let Ok(st) = sys::lstat(root) else {
        return Ok(());
    };
    // Directories still to list, deepest last, each with the entries left to visit.
    let is_dir = st.kind == FileKind::Dir;
    if f(root, is_dir)? == Flow::SkipDir || !is_dir {
        return Ok(());
    }
    let mut stack: Vec<Listing> = Vec::new();
    stack.push((root.to_vec(), sys::read_dir(root).unwrap_or_default().into_iter()));
    while let Some((dir, entries)) = stack.last_mut() {
        let Some((name, is_dir)) = entries.next() else {
            stack.pop();
            continue;
        };
        let path = gopath::join(NATIVE, &[dir, &name]);
        match f(&path, is_dir)? {
            Flow::SkipDir if !is_dir => {
                // A file's SkipDir skips the rest of its directory.
                stack.pop();
            }
            Flow::SkipDir => {}
            Flow::Continue if is_dir => {
                let entries = sys::read_dir(&path).unwrap_or_default();
                stack.push((path, entries.into_iter()));
            }
            Flow::Continue => {}
        }
    }
    Ok(())
}

/// getWalkRoot: on Unix the source with one trailing separator trimmed, a separator and
/// the include, uncleaned, so a trailing `.` or `/` survives; on Windows joined.
fn walk_root(src: &[u8], include: &[u8]) -> Vec<u8> {
    match NATIVE {
        Os::Unix => {
            let src = src.strip_suffix(b"/").unwrap_or(src);
            [src, b"/", include].concat()
        }
        Os::Windows => gopath::join(Os::Windows, &[src, include]),
    }
}

/// strings.Replace(s, old, new, 1).
fn replace_first(s: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    if old.is_empty() {
        return [new, s].concat();
    }
    match s.windows(old.len()).position(|w| w == old) {
        Some(i) => [
            s.get(..i).unwrap_or_default(),
            new,
            s.get(i + old.len()..).unwrap_or_default(),
        ]
        .concat(),
        None => s.to_vec(),
    }
}

/// tarAppender.
struct Packer<W: Write> {
    tw: Writer<W>,
    /// The first name of each inode with more than one link. Keyed by device and inode,
    /// where go-archive keys by inode alone and so links files of two filesystems that
    /// share an inode number; within one filesystem the archives are the same.
    seen_inodes: HashMap<(u64, u64), Vec<u8>>,
    chown: Option<(i64, i64)>,
    whiteout: Option<Overlay>,
}

impl<W: Write> Packer<W> {
    /// addTarFile: `src` as `name`. A file that cannot be stated, read as a link, or put in
    /// a header is skipped, as go-archive logs it and goes on; writing errors end it.
    fn add(&mut self, src: &[u8], name: &[u8]) -> Result<(), Error> {
        let name = gopath::to_slash(NATIVE, name);
        let Ok(st) = sys::lstat(src) else {
            return Ok(());
        };
        let mut link = Vec::new();
        if st.kind == FileKind::Symlink {
            let Ok(target) = sys::readlink(src) else {
                return Ok(());
            };
            link = target;
        }
        let Some(mut hdr) = file_info_header(&name, &st, &link) else {
            return Ok(());
        };
        if let Some(mut cap) = sys::capability(src) {
            // VFS_CAP_REVISION_3 becomes revision 2, its root id dropped: it means nothing
            // outside the user namespace the archive was made in.
            if cap.get(3) == Some(&3) {
                if let Some(v) = cap.get_mut(3) {
                    *v = 2;
                }
                cap.truncate(20);
            }
            hdr.pax
                .insert([PAX_SCHILY_XATTR, b"security.capability"].concat(), cap);
        }
        if st.kind != FileKind::Dir && has_hardlinks(&st) {
            match self.seen_inodes.get(&(st.dev, st.ino)) {
                Some(first) => {
                    hdr.typeflag = TYPE_LINK;
                    hdr.linkname = first.clone();
                    hdr.size = 0;
                }
                None => {
                    self.seen_inodes.insert((st.dev, st.ino), hdr.name.clone());
                }
            }
        }
        if let Some((uid, gid)) = self.chown {
            hdr.uid = uid;
            hdr.gid = gid;
        }
        if let Some(conv) = &self.whiteout {
            // A converted directory's header goes first, its opaque marker after it.
            let Ok(marker) = conv.convert_write(&mut hdr, src, &st) else {
                return Ok(());
            };
            if let Some(marker) = marker {
                // go-archive's "tar: cannot use whiteout for non-empty file", logged.
                if hdr.typeflag == TYPE_REG && hdr.size > 0 {
                    return Ok(());
                }
                if crate::tar::allowed(&hdr).is_none() {
                    return Ok(());
                }
                self.tw.write_header(&hdr)?;
                hdr = marker;
            }
        }
        if crate::tar::allowed(&hdr).is_none() {
            return Ok(());
        }
        self.tw.write_header(&hdr)?;
        if hdr.typeflag == TYPE_REG && hdr.size > 0 {
            let file = std::fs::File::open(sys::os_path(src)).map_err(|e| Error::path("open", src, &e))?;
            self.tw.copy_from(file)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn has_hardlinks(st: &Stat) -> bool {
    st.nlink > 1
}

/// changes_windows.go: no hard links.
#[cfg(windows)]
fn has_hardlinks(_: &Stat) -> bool {
    false
}

/// FileInfoHeader: Go's tar.FileInfoHeader without name lookups, then go-archive's
/// format, times, mode and name. None where Go errs: a socket, an irregular file.
pub(crate) fn file_info_header(name: &[u8], st: &Stat, link: &[u8]) -> Option<Header> {
    let mut hdr = Header {
        mode: sys::perm(st),
        mtime: Time {
            sec: st.mtime.sec,
            nsec: 0,
        },
        format: Format::PAX,
        ..Header::default()
    };
    match st.kind {
        FileKind::File => {
            hdr.typeflag = TYPE_REG;
            hdr.size = i64::try_from(st.size).ok()?;
        }
        FileKind::Dir => hdr.typeflag = TYPE_DIR,
        FileKind::Symlink => {
            hdr.typeflag = TYPE_SYMLINK;
            hdr.linkname = link.to_vec();
        }
        FileKind::Char => hdr.typeflag = TYPE_CHAR,
        FileKind::Block => hdr.typeflag = TYPE_BLOCK,
        FileKind::Fifo => hdr.typeflag = TYPE_FIFO,
        FileKind::Socket | FileKind::Other => return None,
    }
    sys_stat(st, &mut hdr);
    hdr.mode = chmod_tar_entry(hdr.mode);
    hdr.name = name.to_vec();
    if st.kind == FileKind::Dir && !hdr.name.ends_with(b"/") {
        hdr.name.push(b'/');
    }
    Some(hdr)
}

/// tarheader's sysStat (tarheader_unix.go): owner ids, and device numbers when
/// `st_mode` has either S_IFBLK or S_IFCHR bit, which go-archive tests bit by bit, so
/// directories and symlinks take theirs from `st_rdev` too.
#[cfg(unix)]
fn sys_stat(st: &Stat, hdr: &mut Header) {
    hdr.uid = i64::from(st.uid);
    hdr.gid = i64::from(st.gid);
    if st.mode & sys::S_IFBLK != 0 || st.mode & sys::S_IFCHR != 0 {
        let (major, minor) = sys::major_minor(st.rdev);
        hdr.devmajor = major;
        hdr.devminor = minor;
    }
}

#[cfg(windows)]
fn sys_stat(_: &Stat, _: &mut Header) {}

/// chmodTarEntry: Unix modes as they are.
#[cfg(unix)]
fn chmod_tar_entry(mode: i64) -> i64 {
    mode
}

/// chmodTarEntry (archive_windows.go): no group or other write, and everything
/// executable.
#[cfg(windows)]
fn chmod_tar_entry(mode: i64) -> i64 {
    (mode & 0o755) | 0o111
}
