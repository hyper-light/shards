//! The files beneath, on Windows, where shards is only `docker cp`'s client: Go's
//! FileInfo modes for packing (go1.26.1 src/os/types_windows.go), and the destination of
//! an unpack walked as Go's os.Root walks it (root.go, root_windows.go), one component at
//! a time, every symlink or junction spliced in and checked, `..` cleaned first.
//!
//! Go walks with handles opened relative to their parents; here each step is a path
//! checked before it is used, so a local process racing the unpack could swap a
//! directory for a link between check and use. The archive alone cannot: its entries are
//! applied one after another. As go-archive does on Windows, owners, modes, devices and
//! extended attributes are not applied.

use std::ffi::OsString;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileTimesExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_WRITE_ATTRIBUTES, FindClose,
    FindFirstFileW, WIN32_FIND_DATAW,
};

use super::{FileKind, Stat};
use crate::gopath::{self, Os};
use crate::root::{self, Step, WalkError};
use crate::tar::Time;

const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
const IO_REPARSE_TAG_AF_UNIX: u32 = 0x8000_0023;
const IO_REPARSE_TAG_DEDUP: u32 = 0x8000_0013;
/// ERROR_DIRECTORY: the name is not a directory.
const ERROR_DIRECTORY: i32 = 267;

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

/// os.Lstat's FileMode on Windows: 0444 or 0666 by the read-only attribute, a directory
/// 0111 more; symlinks by their reparse tag, and other reparse points (junctions among
/// them) irregular.
pub(crate) fn lstat(p: &[u8]) -> io::Result<Stat> {
    let path = os_path(p);
    let md = fs::symlink_metadata(&path)?;
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
        match reparse_tag(&path) {
            IO_REPARSE_TAG_SYMLINK => kind = FileKind::Symlink,
            IO_REPARSE_TAG_AF_UNIX => kind = FileKind::Socket,
            IO_REPARSE_TAG_DEDUP => {}
            _ => kind = FileKind::Other,
        }
    }
    Ok(Stat {
        kind,
        mode,
        uid: 0,
        gid: 0,
        size: md.len(),
        mtime: unix_time(md.modified()),
        ino: 0,
        dev: 0,
        nlink: 1,
        rdev: 0,
    })
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

/// No security.capability on Windows (xattr_unsupported.go).
pub(crate) fn capability(_: &[u8]) -> Option<Vec<u8>> {
    None
}

/// go-archive's chmod of an implied directory changes nothing Windows keeps.
pub(crate) fn fix_implied_dir(_: &Root, _: &[u8], _: u32) -> Result<(), crate::Error> {
    Ok(())
}

fn join(dir: &[u8], name: &[u8]) -> Vec<u8> {
    [dir, b"\\", name].concat()
}

/// rootOpenDir, by path: a directory steps on, a symlink or junction is spliced in.
fn open_dir(parent: &[u8], name: &[u8]) -> io::Result<Step<Vec<u8>>> {
    let p = join(parent, name);
    let md = fs::symlink_metadata(os_path(&p))?;
    if md.file_type().is_symlink() {
        return Ok(Step::Link(readlink(&p)?));
    }
    if !md.is_dir() {
        return Err(io::Error::from_raw_os_error(ERROR_DIRECTORY));
    }
    Ok(Step::Done(p))
}

/// The final component: a symlink or junction is spliced in, anything else is given to
/// `f`.
fn follow<T>(parent: &[u8], name: &[u8], f: impl FnOnce(&[u8]) -> io::Result<T>) -> io::Result<Step<T>> {
    let p = join(parent, name);
    match fs::symlink_metadata(os_path(&p)) {
        Ok(md) if md.file_type().is_symlink() => Ok(Step::Link(readlink(&p)?)),
        _ => f(&p).map(Step::Done),
    }
}

fn system_time(t: Time) -> SystemTime {
    let d = Duration::new(u64::try_from(t.sec).unwrap_or(0), t.nsec);
    UNIX_EPOCH.checked_add(d).unwrap_or(UNIX_EPOCH)
}

/// The destination of an unpack.
#[derive(Debug)]
pub(crate) struct Root {
    name: Vec<u8>,
}

impl Root {
    pub(crate) fn open(name: &[u8]) -> Result<Root, crate::Error> {
        let md = fs::metadata(os_path(name)).map_err(|e| crate::Error::path("open", name, &e))?;
        if !md.is_dir() {
            return Err(crate::Error::other(format!(
                "open {}: not a directory",
                String::from_utf8_lossy(name)
            )));
        }
        Ok(Root { name: name.to_vec() })
    }

    pub(crate) fn name(&self) -> &[u8] {
        &self.name
    }

    fn walk<T>(
        &self,
        name: &[u8],
        mut f: impl FnMut(&[u8], &[u8]) -> io::Result<Step<T>>,
    ) -> Result<T, WalkError> {
        let root = self.name.clone();
        root::walk(
            Os::Windows,
            || root.clone(),
            name,
            |p: &Vec<u8>, n: &[u8]| open_dir(p, n),
            |dir, last| f(dir, last),
        )
    }

    pub(crate) fn lstat(&self, name: &[u8]) -> Result<Stat, WalkError> {
        self.walk(name, |dir, last| lstat(&join(dir, last)).map(Step::Done))
    }

    pub(crate) fn stat(&self, name: &[u8]) -> Result<Stat, WalkError> {
        self.walk(name, |dir, last| follow(dir, last, lstat))
    }

    pub(crate) fn mkdir(&self, name: &[u8], _: u32) -> Result<(), WalkError> {
        self.walk(name, |dir, last| {
            fs::create_dir(os_path(&join(dir, last))).map(Step::Done)
        })
    }

    pub(crate) fn create(&self, name: &[u8], _: u32) -> Result<File, WalkError> {
        self.walk(name, |dir, last| {
            follow(dir, last, |p| {
                OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(os_path(p))
            })
        })
    }

    pub(crate) fn lchown(&self, _: &[u8], _: i64, _: i64) -> Result<(), WalkError> {
        Ok(())
    }

    /// chtimes (time_windows.go): access and modification times, and the creation time
    /// set to the modification time, never through a reparse point.
    pub(crate) fn chtimes(&self, name: &[u8], atime: Time, mtime: Time) -> Result<(), WalkError> {
        self.walk(name, |dir, last| {
            follow(dir, last, |p| {
                let f = OpenOptions::new()
                    .access_mode(FILE_WRITE_ATTRIBUTES)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                    .open(os_path(p))?;
                let times = FileTimes::new()
                    .set_accessed(system_time(atime))
                    .set_modified(system_time(mtime))
                    .set_created(system_time(mtime));
                f.set_times(times)
            })
        })
    }

    pub(crate) fn lchtimes(&self, _: &[u8], _: &[u8], _: Time, _: Time) -> Result<io::Result<()>, WalkError> {
        Ok(Ok(()))
    }

    /// rootSymlink (root_windows.go): a directory link when a relative target names a
    /// directory in the root, a file link otherwise; the target stored as it is.
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
        let t = os_path(target);
        self.walk(name, |dir, last| {
            let p = os_path(&join(dir, last));
            let r = if is_dir {
                std::os::windows::fs::symlink_dir(&t, &p)
            } else {
                std::os::windows::fs::symlink_file(&t, &p)
            };
            r.map(Step::Done)
        })
    }

    pub(crate) fn link(&self, old: &[u8], new: &[u8]) -> Result<(), WalkError> {
        let old_path = self.walk(old, |dir, last| Ok(Step::Done(join(dir, last))))?;
        self.walk(new, |dir, last| {
            fs::hard_link(os_path(&old_path), os_path(&join(dir, last))).map(Step::Done)
        })
    }

    /// RemoveAll: links themselves removed, never followed.
    pub(crate) fn remove_all(&self, name: &[u8]) -> Result<(), WalkError> {
        let r = self.walk(name, |dir, last| {
            let p = os_path(&join(dir, last));
            let md = match fs::symlink_metadata(&p) {
                Ok(md) => md,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Step::Done(())),
                Err(e) => return Err(e),
            };
            let r = if md.file_type().is_symlink() {
                if md.file_attributes() & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    fs::remove_dir(&p)
                } else {
                    fs::remove_file(&p)
                }
            } else if md.is_dir() {
                fs::remove_dir_all(&p)
            } else {
                fs::remove_file(&p)
            };
            r.map(Step::Done)
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
