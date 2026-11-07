//! The files beneath, on Windows, where shards is only `docker cp`'s client: Go's
//! FileInfo modes for packing (go1.26.1 src/os/types_windows.go), and the destination of
//! an unpack walked as Go's os.Root walks it (root.go, root_windows.go), one component at
//! a time, every symlink or junction spliced in and checked, `..` cleaned first.
//!
//! As Go walks, the walk holds a handle of each directory and opens the next name relative
//! to it (NtCreateFile's RootDirectory, `shards_ntfs::create_at`), as itself
//! (FILE_OPEN_REPARSE_POINT): a link met is never followed but spliced in, its target read
//! from its own handle, and every operation on the last name is an operation in its
//! directory's handle. A directory a local process swaps for a link after the walk opened
//! it is the directory the walk opened still, and a link put in a name's place is a link
//! the walk splices, so nothing is reached outside the root through either. As go-archive
//! does on Windows, owners, modes, devices and extended attributes are not applied.

use std::ffi::OsString;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileTimesExt, MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsHandle as _, BorrowedHandle};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use shards_ntfs::{Reparse, SHARE_ALL};
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
    FILE_OPEN_REPARSE_POINT,
};
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_WRITE_ATTRIBUTES, FindClose, FindFirstFileW,
    WIN32_FIND_DATAW,
};

use super::{FileKind, Stat};
use crate::gopath::{self, Os};
use crate::root::{self, Step, WalkError};
use crate::tar::Time;

const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const IO_REPARSE_TAG_AF_UNIX: u32 = 0x8000_0023;
const IO_REPARSE_TAG_DEDUP: u32 = 0x8000_0013;
/// A symbolic link's flag that its target is relative to its directory (MS-FSCC 2.1.2.4).
const SYMLINK_FLAG_RELATIVE: u32 = 1;

pub(crate) const S_IFBLK: u32 = 0o060000;
pub(crate) const S_IFCHR: u32 = 0o020000;
pub(crate) const S_IFIFO: u32 = 0o010000;
/// Never matched: attributes are not set on Windows.
pub(crate) const ENOTSUP: i32 = -1;
pub(crate) const EPERM: i32 = -1;

pub(crate) fn os_path(p: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from(String::from_utf8_lossy(p).into_owned()))
}

pub(crate) fn os_path_buf(p: &[u8]) -> PathBuf {
    os_path(p)
}

pub(crate) fn path_bytes(p: &Path) -> Vec<u8> {
    p.to_string_lossy().into_owned().into_bytes()
}

/// The reparse tag of a reparse point, as FindFirstFile reports it.
fn reparse_tag(p: &Path) -> u32 {
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: WIN32_FIND_DATAW is plain data, valid zeroed.
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    // SAFETY: wide is NUL-terminated; data is valid for writes.
    let h = unsafe { FindFirstFileW(wide.as_ptr(), &mut data) };
    if h.is_null() || h as isize == -1 {
        return 0;
    }
    // SAFETY: h is a search handle FindFirstFileW returned.
    unsafe { FindClose(h) };
    data.dwReserved0
}

fn unix_time(t: io::Result<SystemTime>) -> Time {
    match t {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => Time::unix(
                i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                i64::from(d.subsec_nanos()),
            ),
            Err(e) => {
                let d = e.duration();
                Time::unix(
                    -i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                    -i64::from(d.subsec_nanos()),
                )
            }
        },
        Err(_) => Time::unix(0, 0),
    }
}

/// os.Lstat's FileMode on Windows of what `md` says and the reparse tag `tag`: 0444 or
/// 0666 by the read-only attribute, a directory 0111 more; symlinks by their reparse tag,
/// and other reparse points (junctions among them) irregular. One name of one file.
fn stat_from(md: &fs::Metadata, tag: u32) -> Stat {
    let attrs = md.file_attributes();
    let mut mode = if attrs & FILE_ATTRIBUTE_READONLY != 0 {
        0o444
    } else {
        0o666
    };
    let surrogate = md.file_type().is_symlink();
    let mut kind = FileKind::File;
    if !surrogate && attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        mode |= 0o111;
        kind = FileKind::Dir;
    }
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        match tag {
            IO_REPARSE_TAG_SYMLINK => kind = FileKind::Symlink,
            IO_REPARSE_TAG_AF_UNIX => kind = FileKind::Socket,
            IO_REPARSE_TAG_DEDUP => {}
            _ => kind = FileKind::Other,
        }
    }
    Stat {
        kind,
        mode,
        uid: 0,
        gid: 0,
        size: md.len(),
        mtime: unix_time(md.modified()),
        ino: 0,
        dev: 0,
        nlink: 1,
        major: 0,
        minor: 0,
    }
}

/// [`stat_from`] of the file `f` holds, a reparse point's own: its metadata, reparse tag
/// and, of what is not a directory, its links as Unix's lstat says them
/// (`shards_ntfs::links`), which go-archive does not ask on Windows ([`crate::pack`]). A
/// file whose links cannot be asked is one of one name.
fn stat_of(f: &File) -> io::Result<Stat> {
    let md = f.metadata()?;
    let (_, tag) = shards_ntfs::attribute_tag(f.as_handle())?;
    let mut st = stat_from(&md, tag);
    if st.kind != FileKind::Dir
        && let Some(l) = shards_ntfs::links(f.as_handle()).ok().flatten()
    {
        (st.ino, st.dev, st.nlink) = (l.file, l.volume, l.count);
    }
    Ok(st)
}

/// os.Lstat, for packing. The file itself, opened for no access as std's lstat opens it, a
/// reparse point not followed: its metadata, tag and links asked of one handle. One std's
/// lstat cannot open (a sharing violation) it reads from its directory's entry, which says
/// no links.
pub(crate) fn lstat(p: &[u8]) -> io::Result<Stat> {
    let path = os_path(p);
    let opened = OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&path);
    match opened {
        Ok(f) => stat_of(&f),
        Err(_) => Ok(stat_from(&fs::symlink_metadata(&path)?, reparse_tag(&path))),
    }
}

/// os.Stat.
pub(crate) fn stat(p: &[u8]) -> io::Result<Stat> {
    let md = fs::metadata(os_path(p))?;
    let mut st = lstat(p)?;
    if st.kind == FileKind::Symlink {
        st.kind = if md.is_dir() {
            FileKind::Dir
        } else {
            FileKind::File
        };
        st.size = md.len();
    }
    Ok(st)
}

pub(crate) fn readlink(p: &[u8]) -> io::Result<Vec<u8>> {
    Ok(path_bytes(&fs::read_link(os_path(p))?))
}

pub(crate) fn perm(st: &Stat) -> i64 {
    i64::from(st.mode)
}

/// os.ReadDir: names sorted, and whether each is a directory (not a link to one).
pub(crate) fn read_dir(p: &[u8]) -> io::Result<Vec<(Vec<u8>, bool)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(os_path(p))? {
        let Ok(entry) = entry else {
            break;
        };
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        out.push((path_bytes(Path::new(&entry.file_name())), is_dir));
    }
    out.sort();
    Ok(out)
}

/// No attributes on Windows (xattr_unsupported.go).
pub(crate) fn lgetxattr(_: &[u8], _: &[u8]) -> io::Result<Option<Vec<u8>>> {
    Ok(None)
}

/// go-archive's chmod of an implied directory changes nothing Windows keeps.
pub(crate) fn fix_implied_dir(_: &Root, _: &[u8], _: u32) -> Result<(), crate::Error> {
    Ok(())
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    [dir, b"\\", name].concat()
}

/// A directory of the walk: the root's own handle, or one the walk opened beneath it.
#[derive(Debug)]
pub(crate) enum Dir<'a> {
    Root(BorrowedHandle<'a>),
    Owned(File),
}

impl Dir<'_> {
    fn handle(&self) -> BorrowedHandle<'_> {
        match self {
            Dir::Root(h) => *h,
            Dir::Owned(f) => f.as_handle(),
        }
    }
}

/// A name of the walk as NTFS keeps it, UTF-16, as [`os_path`] makes a path of one; `.`,
/// the directory itself, no name, which opens a directory's own file relative to its handle.
fn wide(name: &[u8]) -> Vec<u16> {
    if name == b"." {
        return Vec::new();
    }
    String::from_utf8_lossy(name).encode_utf16().collect()
}

/// `name` in `dir`, opened as itself, a link not followed, with `access` beside its
/// attributes ([`shards_ntfs::create_at`]).
fn itself(dir: &Dir<'_>, name: &[u8], access: u32) -> io::Result<File> {
    shards_ntfs::create_at(
        dir.handle(),
        &wide(name),
        access,
        SHARE_ALL,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT,
    )
}

/// What the link `h` holds open, as itself, is to, as std's `read_link` reads one
/// (library/std/src/sys/fs/windows.rs, readlink): a symbolic link's or a junction's
/// substitute name (MS-FSCC 2.1.2.4, 2.1.2.5), an absolute one's `\??\` made `\\?\`. Another
/// reparse point, WSL's symlink among them, std reads no target of: refused as a link,
/// ERROR_STOPPED_ON_SYMLINK.
fn link_target(h: BorrowedHandle<'_>) -> io::Result<Vec<u8>> {
    let refused = || io::Error::from_raw_os_error(shards_ntfs::STOPPED_ON_LINK);
    let Some((tag, data)) = shards_ntfs::reparse(h).map_err(shards_ntfs::io_error)? else {
        return Err(refused());
    };
    let half = |at: usize| -> Option<usize> {
        let b = data.get(at..at.checked_add(2)?)?;
        Some(usize::from(u16::from_le_bytes(<[u8; 2]>::try_from(b).ok()?)))
    };
    // A symbolic link's names follow its flags; a junction's, which has none, its lengths.
    let (names, relative) = match tag {
        IO_REPARSE_TAG_SYMLINK => {
            let flags = data
                .get(8..12)
                .and_then(|b| <[u8; 4]>::try_from(b).ok())
                .map(u32::from_le_bytes)
                .ok_or_else(refused)?;
            (12, flags & SYMLINK_FLAG_RELATIVE != 0)
        }
        IO_REPARSE_TAG_MOUNT_POINT => (8, false),
        _ => return Err(refused()),
    };
    let start = names + half(0).ok_or_else(refused)?;
    let end = start + half(2).ok_or_else(refused)?;
    let units: Vec<u16> = data
        .get(start..end)
        .ok_or_else(refused)?
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let target = String::from_utf16_lossy(&units);
    match target.strip_prefix(r"\??\") {
        Some(rest) if !relative => Ok(format!(r"\\?\{rest}").into_bytes()),
        _ => Ok(target.into_bytes()),
    }
}

/// What `name` in `dir` is a link to, where it is one; None where it is none.
fn link_at(dir: &Dir<'_>, name: &[u8]) -> io::Result<Option<Vec<u8>>> {
    let f = itself(dir, name, 0)?;
    match shards_ntfs::reparse_of(f.as_handle())? {
        Reparse::Link => link_target(f.as_handle()).map(Some),
        _ => Ok(None),
    }
}

/// `name` in `dir` opened with `access`, `disposition` and `options`, through no link
/// (`shards_ntfs::open_unfollowed_at`): a link there is a step to its target, as Go's openat
/// splices one its O_NOFOLLOW_ANY open meets (root_windows.go, openat and
/// readReparseLinkAt), whether it was refused as a link or as a directory's link opened as
/// a file or a file's as a directory. Any other error stands.
fn opened(dir: &Dir<'_>, name: &[u8], access: u32, disposition: u32, options: u32) -> io::Result<Step<File>> {
    match shards_ntfs::open_unfollowed_at(dir.handle(), &wide(name), access, SHARE_ALL, disposition, options)
    {
        Ok(f) => Ok(Step::Done(f)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Err(e),
        Err(e) => match link_at(dir, name) {
            Ok(Some(target)) => Ok(Step::Link(target)),
            _ => Err(e),
        },
    }
}

/// rootOpenDir: a directory relative to its parent's handle, through no link; a link is
/// spliced in, anything else refused as Windows refuses it (ERROR_DIRECTORY).
fn open_dir<'a>(dir: &Dir<'a>, name: &[u8]) -> io::Result<Step<Dir<'a>>> {
    Ok(
        match opened(dir, name, FILE_LIST_DIRECTORY, FILE_OPEN, FILE_DIRECTORY_FILE)? {
            Step::Done(f) => Step::Done(Dir::Owned(f)),
            Step::Link(target) => Step::Link(target),
        },
    )
}

/// Root.OpenFile(name, O_CREATE|O_WRONLY|O_TRUNC, perm)'s open of the last name in its
/// directory: opened, or made, through no link, its attributes readable, which the open asks
/// of its handle. [`Root::create`] empties it.
fn create_in(dir: &Dir<'_>, name: &[u8]) -> io::Result<Step<File>> {
    opened(
        dir,
        name,
        FILE_GENERIC_WRITE | FILE_READ_ATTRIBUTES,
        FILE_OPEN_IF,
        FILE_NON_DIRECTORY_FILE,
    )
}

/// The names a symbolic link to `target` keeps, as CreateSymbolicLinkW keeps them: a
/// target of no volume (relative, or from its volume's root) as it is, both names, the link
/// relative, as Go's rootSymlink marks one (os/root_windows.go: "CreateSymbolicLinkW appears
/// to set the relative flag iff the target does not contain a volume name"); any other its
/// full path, a volume-relative one made absolute as Go and CreateSymbolicLinkW make it,
/// substituted in NT's namespace (`\??\`, a share's `\??\UNC\`) and shown as it is.
fn link_names(target: &[u8]) -> (Vec<u16>, Vec<u16>, bool) {
    let utf16 = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    let given = String::from_utf8_lossy(target).into_owned();
    if gopath::volume_name_len(Os::Windows, target) == 0 {
        return (utf16(&given), utf16(&given), true);
    }
    let full = match gopath::is_abs(Os::Windows, target) {
        true => given,
        false => std::path::absolute(&given).map_or(given, |p| p.to_string_lossy().into_owned()),
    };
    let nt = if let Some(rest) = full.strip_prefix(r"\\?\").or_else(|| full.strip_prefix(r"\??\")) {
        format!(r"\??\{rest}")
    } else if let Some(rest) = full.strip_prefix(r"\\") {
        format!(r"\??\UNC\{rest}")
    } else {
        format!(r"\??\{full}")
    };
    (utf16(&nt), utf16(&full), false)
}

/// removeAllFrom (go1.26.1 os/removeall_at.go) in `dir`'s handle: `name` opened as itself
/// to be removed; a directory, no link, emptied first, each of its names removed relative to
/// a handle of its own (opened by no name relative to the first, the same directory however
/// it was moved), then removed itself; a link or anything else removed itself. Nothing
/// followed; a name already gone is none.
fn remove_all_at(dir: BorrowedHandle<'_>, name: &[u16]) -> io::Result<()> {
    if name.is_empty() || name == [0x2e, 0x2e] {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let gone = match shards_ntfs::create_at(dir, name, DELETE, SHARE_ALL, FILE_OPEN, FILE_OPEN_REPARSE_POINT)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let (attributes, _) = shards_ntfs::attribute_tag(gone.as_handle())?;
    if attributes & FILE_ATTRIBUTE_DIRECTORY != 0
        && shards_ntfs::reparse_of(gone.as_handle())? != Reparse::Link
    {
        let listed = shards_ntfs::create_at(
            gone.as_handle(),
            &[],
            FILE_LIST_DIRECTORY,
            SHARE_ALL,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
        )?;
        for entry in shards_ntfs::names(listed.as_handle())? {
            remove_all_at(listed.as_handle(), &entry)?;
        }
    }
    shards_ntfs::delete(gone.as_handle())
}

fn system_time(t: Time) -> SystemTime {
    let d = Duration::new(u64::try_from(t.sec).unwrap_or(0), t.nsec);
    UNIX_EPOCH.checked_add(d).unwrap_or(UNIX_EPOCH)
}

/// The destination of an unpack: its name and its directory's handle, which every walk
/// starts from (Go's os.Root).
#[derive(Debug)]
pub(crate) struct Root {
    name: Vec<u8>,
    dir: File,
}

impl Root {
    /// os.OpenRoot: the directory at `name`, followed as given, held open.
    pub(crate) fn open(name: &[u8]) -> Result<Root, crate::Error> {
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(os_path(name))
            .map_err(|e| crate::Error::path("open", name, &e))?;
        let md = dir.metadata().map_err(|e| crate::Error::path("open", name, &e))?;
        if !md.is_dir() {
            return Err(crate::Error::other(format!(
                "open {}: not a directory",
                String::from_utf8_lossy(name)
            )));
        }
        Ok(Root {
            name: name.to_vec(),
            dir,
        })
    }

    pub(crate) fn name(&self) -> &[u8] {
        &self.name
    }

    fn walk<T>(
        &self,
        name: &[u8],
        mut f: impl FnMut(&Dir<'_>, &[u8]) -> io::Result<Step<T>>,
    ) -> Result<T, WalkError> {
        root::walk(
            Os::Windows,
            || Dir::Root(self.dir.as_handle()),
            name,
            open_dir,
            |dir, last| f(dir, last),
        )
    }

    pub(crate) fn lstat(&self, name: &[u8]) -> Result<Stat, WalkError> {
        self.walk(name, |dir, last| stat_of(&itself(dir, last, 0)?).map(Step::Done))
    }

    /// rootStat: a final link followed inside the root.
    pub(crate) fn stat(&self, name: &[u8]) -> Result<Stat, WalkError> {
        self.walk(name, |dir, last| {
            let f = itself(dir, last, 0)?;
            if shards_ntfs::reparse_of(f.as_handle())? == Reparse::Link {
                return link_target(f.as_handle()).map(Step::Link);
            }
            stat_of(&f).map(Step::Done)
        })
    }

    /// Mkdirat: made in its parent's handle.
    pub(crate) fn mkdir(&self, name: &[u8], _: u32) -> Result<(), WalkError> {
        self.walk(name, |dir, last| {
            shards_ntfs::create_at(
                dir.handle(),
                &wide(last),
                FILE_GENERIC_READ,
                SHARE_ALL,
                FILE_CREATE,
                FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT,
            )
            .map(|_| Step::Done(()))
        })
    }

    /// Root.Open: read only, a final symlink followed inside the root.
    pub(crate) fn open_read(&self, name: &[u8]) -> Result<File, WalkError> {
        self.walk(name, |dir, last| {
            opened(dir, last, FILE_GENERIC_READ, FILE_OPEN, FILE_NON_DIRECTORY_FILE)
        })
    }

    /// Root.OpenFile(name, O_CREATE|O_WRONLY|O_TRUNC, perm): opened, or made, then emptied,
    /// as Go's Openat empties it (internal/syscall/windows/at_windows.go), the open never
    /// truncating what may be a link ([`create_in`]).
    pub(crate) fn create(&self, name: &[u8], _: u32) -> Result<File, WalkError> {
        let file = self.walk(name, create_in)?;
        file.set_len(0)?;
        Ok(file)
    }

    pub(crate) fn lchown(&self, _: &[u8], _: i64, _: i64) -> Result<(), WalkError> {
        Ok(())
    }

    /// chtimesat (root_windows.go): access and modification times, and the creation time
    /// set to the modification time (time_windows.go), on the name itself; a final link
    /// followed inside the root.
    pub(crate) fn chtimes(&self, name: &[u8], atime: Time, mtime: Time) -> Result<(), WalkError> {
        self.walk(name, |dir, last| {
            let f = itself(dir, last, FILE_WRITE_ATTRIBUTES)?;
            if shards_ntfs::reparse_of(f.as_handle())? == Reparse::Link {
                return link_target(f.as_handle()).map(Step::Link);
            }
            let times = FileTimes::new()
                .set_accessed(system_time(atime))
                .set_modified(system_time(mtime))
                .set_created(system_time(mtime));
            f.set_times(times).map(Step::Done)
        })
    }

    pub(crate) fn lchtimes(&self, _: &[u8], _: &[u8], _: Time, _: Time) -> Result<io::Result<()>, WalkError> {
        Ok(Ok(()))
    }

    /// rootSymlink (root_windows.go): a directory link when a relative target names a
    /// directory in the root, a file link otherwise, made in its parent's handle
    /// (`shards_ntfs::host_symlink_at`, Go's Symlinkat); its names as CreateSymbolicLinkW
    /// keeps them ([`link_names`]).
    pub(crate) fn symlink(&self, target: &[u8], name: &[u8]) -> Result<(), WalkError> {
        let mut is_dir = false;
        if gopath::volume_name_len(Os::Windows, target) == 0
            && !target.first().is_some_and(|&c| Os::Windows.is_sep(c))
        {
            let parent = gopath::dir(Os::Windows, name);
            let dest = if parent == b"." {
                target.to_vec()
            } else {
                join(&parent, target)
            };
            is_dir = self.stat(&dest).is_ok_and(|st| st.kind == FileKind::Dir);
        }
        let (substitute, print, relative) = link_names(target);
        self.walk(name, |dir, last| {
            shards_ntfs::host_symlink_at(dir.handle(), &wide(last), &substitute, &print, is_dir, relative)
                .map(Step::Done)
        })
    }

    /// Root.Link: both names walked inside the root; a final link of `old` is linked
    /// itself (Go's Linkat, FILE_OPEN_REPARSE_POINT).
    pub(crate) fn link(&self, old: &[u8], new: &[u8]) -> Result<(), WalkError> {
        self.walk(old, |odir, olast| {
            let r = self.walk(new, |ndir, nlast| {
                shards_ntfs::hard_link_at(odir.handle(), &wide(olast), ndir.handle(), &wide(nlast))
                    .map(Step::Done)
            });
            Ok(Step::Done(r))
        })?
    }

    /// The names in the directory `name` (the root itself for `.`), never following it,
    /// read through its handle.
    pub(crate) fn read_dir(&self, name: &[u8]) -> Result<Vec<Vec<u8>>, WalkError> {
        let list = |h: BorrowedHandle<'_>| -> io::Result<Vec<Vec<u8>>> {
            Ok(shards_ntfs::names(h)?
                .iter()
                .map(|n| String::from_utf16_lossy(n).into_bytes())
                .collect())
        };
        if name.is_empty() || name == b"." {
            return Ok(list(self.dir.as_handle())?);
        }
        self.walk(name, |dir, last| {
            let d = shards_ntfs::open_unfollowed_at(
                dir.handle(),
                &wide(last),
                FILE_LIST_DIRECTORY,
                SHARE_ALL,
                FILE_OPEN,
                FILE_DIRECTORY_FILE,
            )?;
            list(d.as_handle()).map(Step::Done)
        })
    }

    /// Root.RemoveAll: links themselves removed, never followed ([`remove_all_at`]); a
    /// missing name is no error.
    pub(crate) fn remove_all(&self, name: &[u8]) -> Result<(), WalkError> {
        let r = self.walk(name, |dir, last| {
            remove_all_at(dir.handle(), &wide(last)).map(Step::Done)
        });
        match r {
            Err(e) if e.is_not_found() => Ok(()),
            r => r,
        }
    }

    pub(crate) fn mknod(
        &self,
        _: &[u8],
        _: &[u8],
        _: u32,
        _: u32,
        _: u32,
    ) -> Result<io::Result<()>, WalkError> {
        Ok(Ok(()))
    }

    pub(crate) fn chmod_nofollow(&self, _: &[u8], _: &[u8], _: u32) -> Result<io::Result<()>, WalkError> {
        Ok(Ok(()))
    }

    pub(crate) fn set_xattr_dir(&self, _: &[u8], _: &[u8], _: &[u8]) -> Result<io::Result<()>, WalkError> {
        Ok(Ok(()))
    }

    pub(crate) fn lchown_in(&self, _: &[u8], _: &[u8], _: i64, _: i64) -> Result<io::Result<()>, WalkError> {
        Ok(Ok(()))
    }

    pub(crate) fn set_xattr(
        &self,
        _: &[u8],
        _: &[u8],
        _: &[u8],
        _: &[u8],
    ) -> Result<io::Result<()>, WalkError> {
        Ok(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::os::windows::ffi::OsStrExt as _;

    use super::*;

    /// A junction at `at` to the directory `to`, which Windows lets any user make: an empty
    /// directory given a MountPointReparseBuffer, its NT and print names' offsets and
    /// lengths, then each name NUL-terminated (MS-FSCC 2.1.2.5).
    fn junction(at: &Path, to: &Path) {
        fs::create_dir(at).unwrap();
        let f = OpenOptions::new()
            .access_mode(FILE_GENERIC_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(at)
            .unwrap();
        let full = fs::canonicalize(to).unwrap();
        // Its verbatim prefix, \\?\, left off.
        let print: Vec<u16> = full.as_os_str().encode_wide().skip(4).collect();
        let nt: Vec<u16> = r"\??\".encode_utf16().chain(print.iter().copied()).collect();
        let bytes = |n: &[u16]| u16::try_from(n.len() * 2).unwrap();
        let mut data = Vec::new();
        for word in [0, bytes(&nt), bytes(&nt) + 2, bytes(&print)] {
            data.extend(word.to_le_bytes());
        }
        for c in nt.iter().chain([&0]).chain(&print).chain([&0]) {
            data.extend(c.to_le_bytes());
        }
        shards_ntfs::set_reparse(f.as_handle(), IO_REPARSE_TAG_MOUNT_POINT, &data).unwrap();
    }

    /// A directory of the unpack swapped for a junction out of the root after the walk
    /// opened it, before the file below it is made: the file is made in the directory the
    /// walk opened, wherever it was moved, and nothing outside the root; an entry walked
    /// after the swap meets the junction, whose absolute target escapes the root, and is
    /// refused. A path's check before its use, as this module walked before, followed it.
    #[test]
    fn a_directory_swapped_for_a_link_mid_unpack_leads_nowhere_outside() {
        let tmp = shards_testdir::TempDir::new("archive-swap").unwrap();
        let (dest, outside) = (tmp.join("root"), tmp.join("outside"));
        fs::create_dir_all(dest.join("a")).unwrap();
        fs::create_dir(&outside).unwrap();
        let root = Root::open(&path_bytes(&dest)).unwrap();
        let swapped = Cell::new(false);
        let mut file = root::walk(
            Os::Windows,
            || Dir::Root(root.dir.as_handle()),
            b"a\\f",
            |dir, name| {
                let step = open_dir(dir, name);
                if !swapped.replace(true) {
                    fs::rename(dest.join("a"), dest.join("moved")).unwrap();
                    junction(&dest.join("a"), &outside);
                }
                step
            },
            create_in,
        )
        .unwrap();
        io::Write::write_all(&mut file, b"in").unwrap();
        drop(file);
        assert!(swapped.get());
        assert_eq!(fs::read(dest.join("moved").join("f")).unwrap(), b"in");
        assert_eq!(
            fs::read_dir(&outside).unwrap().count(),
            0,
            "written outside the root"
        );

        let e = root.create(b"a\\g", 0o644).unwrap_err();
        assert!(e.escapes(), "{e:?}");
        assert!(root.mkdir(b"a\\d", 0o755).unwrap_err().escapes());
        assert_eq!(
            fs::read_dir(&outside).unwrap().count(),
            0,
            "written outside the root"
        );
        // The junction itself goes, never what it leads to.
        fs::write(outside.join("kept"), b"kept").unwrap();
        root.remove_all(b"a").unwrap();
        assert!(fs::symlink_metadata(dest.join("a")).is_err());
        assert_eq!(fs::read(outside.join("kept")).unwrap(), b"kept");
    }

    /// A link within the root is still spliced in as go-archive's walk splices it: a file
    /// made through a junction to a directory of the root is made there; a removal of
    /// a tree holding a junction removes the junction alone.
    #[test]
    fn a_link_within_the_root_is_followed_inside_it() {
        let tmp = shards_testdir::TempDir::new("archive-inside").unwrap();
        let dest = tmp.join("root");
        fs::create_dir_all(dest.join("real")).unwrap();
        let root = Root::open(&path_bytes(&dest)).unwrap();
        root.symlink(b"real", b"rel").map_or_else(
            // ERROR_PRIVILEGE_NOT_HELD: no Developer Mode, not elevated.
            |e| {
                assert!(
                    matches!(&e, WalkError::Os(o) if o.raw_os_error() == Some(1314)),
                    "{e:?}"
                )
            },
            |()| {
                let mut f = root.create(b"rel\\f", 0o644).unwrap();
                io::Write::write_all(&mut f, b"x").unwrap();
                drop(f);
                assert_eq!(fs::read(dest.join("real").join("f")).unwrap(), b"x");
                assert_eq!(root.lstat(b"rel").unwrap().kind, FileKind::Symlink);
                assert_eq!(root.stat(b"rel").unwrap().kind, FileKind::Dir);
            },
        );
        fs::create_dir(dest.join("tree")).unwrap();
        junction(&dest.join("tree").join("j"), &dest.join("real"));
        fs::write(dest.join("real").join("kept"), b"kept").unwrap();
        root.remove_all(b"tree").unwrap();
        assert!(fs::symlink_metadata(dest.join("tree")).is_err());
        assert_eq!(fs::read(dest.join("real").join("kept")).unwrap(), b"kept");
    }
}
