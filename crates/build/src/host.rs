//! The host's files as fsutil's sender reads them (stat.go, stat_unix.go, stat_windows.go
//! at 83cac42c1c52): Go's FileMode bits, the link and device numbers it sends, and
//! extended attributes, those under `com.apple.` aside. The OS's own calls are here
//! and nowhere else in the crate.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::copy::fm;

/// What fsutil's `mkstat` records of a path, before the sender resets its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    /// Go's FileMode bits.
    pub mode: u32,
    pub size: u64,
    pub mtime: (i64, u32),
    /// The inode and its link count, where hard links are told apart: on Windows, of a
    /// file of more than one name alone, its file ID (`shards_ntfs::links`).
    pub inode: Option<(u128, u64)>,
    pub devmajor: u32,
    pub devminor: u32,
    pub link: Vec<u8>,
    pub xattrs: BTreeMap<Vec<u8>, Vec<u8>>,
    /// A socket, which is sent as the empty file archive/tar can hold.
    pub socket: bool,
}

fn since_epoch(m: &fs::Metadata) -> (i64, u32) {
    match m.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => (i64::try_from(d.as_secs()).unwrap_or(i64::MAX), d.subsec_nanos()),
            Err(e) => {
                let d = e.duration();
                let secs = i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
                match d.subsec_nanos() {
                    0 => (-secs, 0),
                    n => (-secs - 1, 1_000_000_000 - n),
                }
            }
        },
        Err(_) => (0, 0),
    }
}

/// A directory's names, sorted, as `os.ReadDir` gives them.
pub fn read_dir(dir: &Path) -> io::Result<Vec<Vec<u8>>> {
    let mut names = Vec::new();
    for e in fs::read_dir(dir)? {
        names.push(name_bytes(&e?.file_name()));
    }
    names.sort();
    Ok(names)
}

#[cfg(unix)]
fn name_bytes(n: &std::ffi::OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    n.as_bytes().to_vec()
}

#[cfg(windows)]
fn name_bytes(n: &std::ffi::OsStr) -> Vec<u8> {
    n.to_string_lossy().into_owned().into_bytes()
}

/// A path below `root` from its slash-separated relative name.
#[cfg(unix)]
pub fn path(root: &Path, rel: &[u8]) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStrExt;
    root.join(std::ffi::OsStr::from_bytes(rel))
}

#[cfg(windows)]
pub fn path(root: &Path, rel: &[u8]) -> std::path::PathBuf {
    let mut p = root.to_path_buf();
    for part in String::from_utf8_lossy(rel).split('/') {
        p.push(part);
    }
    p
}

/// fsutil's mode (Go's FileMode bits) of a Linux `st_mode`, and whether it is a socket:
/// archive/tar cannot hold one, so fsutil sends it as what it then is, a file.
fn go_mode(st: u32) -> (u32, bool) {
    let mut mode = st & 0o777;
    mode |= match st & 0o170_000 {
        0o040_000 => fm::DIR,
        0o120_000 => fm::SYMLINK,
        0o060_000 => fm::DEVICE,
        0o020_000 => fm::DEVICE | fm::CHAR_DEVICE,
        0o010_000 => fm::NAMED_PIPE,
        0o140_000 => fm::SOCKET,
        _ => 0,
    };
    if st & 0o4000 != 0 {
        mode |= fm::SETUID;
    }
    if st & 0o2000 != 0 {
        mode |= fm::SETGID;
    }
    if st & 0o1000 != 0 {
        mode |= fm::STICKY;
    }
    (mode & !fm::SOCKET, mode & fm::SOCKET != 0)
}

/// `os.Lstat` and fsutil's mkstat.
#[cfg(unix)]
pub fn lstat(p: &Path) -> io::Result<Stat> {
    use std::os::unix::fs::MetadataExt;
    let m = fs::symlink_metadata(p)?;
    let st = m.mode();
    let (mode, socket) = go_mode(st);
    let dir = mode & fm::DIR != 0;
    let rdev = m.rdev();
    let device = st & 0o170_000 == 0o060_000 || st & 0o170_000 == 0o020_000;
    let link = if mode & fm::SYMLINK != 0 {
        use std::os::unix::ffi::OsStrExt;
        fs::read_link(p)?.as_os_str().as_bytes().to_vec()
    } else {
        Vec::new()
    };
    Ok(Stat {
        mode,
        size: if dir { 0 } else { m.size() },
        mtime: since_epoch(&m),
        inode: (!dir).then(|| (u128::from(m.ino()), m.nlink())),
        // fsutil's major and minor, Linux's encoding whatever the host's.
        devmajor: if device { ((rdev >> 8) & 0xfff) as u32 } else { 0 },
        devminor: if device {
            ((rdev & 0xff) | ((rdev >> 12) & 0xf_ff00)) as u32
        } else {
            0
        },
        link,
        xattrs: xattrs(p)?,
        socket,
    })
}

/// `os.Lstat` on Windows and fsutil's mkstat: Go's 0666, or 0444 read-only, made
/// executable and capped at 0755, as fsutil sends Windows files. And its hard links told
/// apart, as fsutil tells a Unix host's (stat_unix.go) and tells none of Windows' (its
/// Stat's inode unset there), where NTFS and ReFS keep them as Unix does: a Windows
/// client's context sends a file's other names as links (shards-archive's `pack_changes`,
/// D180), and the client's own walk of it is the one its copy's makes.
#[cfg(windows)]
pub fn lstat(p: &Path) -> io::Result<Stat> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT};
    // The file itself, opened for no access as std's lstat opens it, a reparse point not
    // followed: its metadata and its links asked of one handle. One std's lstat cannot open
    // (a sharing violation) it reads from its directory's entry, which says no links.
    let opened = fs::OpenOptions::new()
        .access_mode(0)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(p);
    let m = match &opened {
        Ok(f) => f.metadata()?,
        Err(_) => fs::symlink_metadata(p)?,
    };
    let mut mode = if m.permissions().readonly() { 0o444 } else { 0o666 };
    if m.is_dir() {
        mode |= fm::DIR | 0o111;
    }
    if m.file_type().is_symlink() {
        mode |= fm::SYMLINK;
    }
    let perm = ((mode & fm::PERM) | 0o111) & 0o755;
    mode = (mode & !fm::PERM) | perm;
    let link = if m.file_type().is_symlink() {
        fs::read_link(p)?
            .to_string_lossy()
            .replace('\\', "/")
            .into_bytes()
    } else {
        Vec::new()
    };
    // A file whose links cannot be asked goes as one of one name, as before.
    let inode = match &opened {
        Ok(f) if !m.is_dir() => shards_ntfs::links(f.as_handle())
            .ok()
            .flatten()
            .map(|l| (l.file, l.count)),
        _ => None,
    };
    Ok(Stat {
        mode,
        size: if m.is_dir() { 0 } else { m.len() },
        mtime: since_epoch(&m),
        inode,
        devmajor: 0,
        devminor: 0,
        link,
        xattrs: BTreeMap::new(),
        socket: false,
    })
}

/// fsutil's loadXattr: every attribute but `com.apple.*`; none where the file system
/// has none.
#[cfg(unix)]
fn xattrs(p: &Path) -> io::Result<BTreeMap<Vec<u8>, Vec<u8>>> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(p.as_os_str().as_bytes()).map_err(|_| io::Error::other("NUL in path"))?;
    let mut out = BTreeMap::new();
    let names = match list(&c) {
        Ok(n) => n,
        Err(e) if e.raw_os_error() == Some(libc::ENOTSUP) => return Ok(out),
        Err(e) => return Err(io::Error::other(format!("failed to xattr {}: {e}", p.display()))),
    };
    for name in names.split(|&b| b == 0).filter(|n| !n.is_empty()) {
        if name.starts_with(b"com.apple.") {
            continue;
        }
        let Ok(cname) = CString::new(name) else { continue };
        if let Ok(v) = get(&c, &cname) {
            out.insert(name.to_vec(), v);
        }
    }
    Ok(out)
}

/// Calls `f` for the size, then for the bytes, as long as the size keeps growing.
#[cfg(unix)]
fn sized(f: &dyn Fn(*mut libc::c_char, usize) -> isize) -> io::Result<Vec<u8>> {
    loop {
        let n = f(std::ptr::null_mut(), 0);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0u8; n as usize];
        let got = f(buf.as_mut_ptr().cast(), buf.len());
        if got < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::ERANGE) {
                continue;
            }
            return Err(e);
        }
        buf.truncate(got as usize);
        return Ok(buf);
    }
}

#[cfg(target_os = "linux")]
fn list(c: &std::ffi::CStr) -> io::Result<Vec<u8>> {
    // SAFETY: `c` is NUL-terminated and outlives the call; `buf` holds `len` bytes, or is
    // null with `len` 0, as llistxattr(2) takes them.
    sized(&|buf, len| unsafe { libc::llistxattr(c.as_ptr(), buf, len) })
}

#[cfg(target_os = "linux")]
fn get(c: &std::ffi::CStr, name: &std::ffi::CStr) -> io::Result<Vec<u8>> {
    // SAFETY: both names are NUL-terminated and outlive the call; `buf` holds `len` bytes,
    // or is null with `len` 0, as lgetxattr(2) takes them.
    sized(&|buf, len| unsafe { libc::lgetxattr(c.as_ptr(), name.as_ptr(), buf.cast(), len) })
}

#[cfg(target_os = "macos")]
fn list(c: &std::ffi::CStr) -> io::Result<Vec<u8>> {
    // SAFETY: `c` is NUL-terminated and outlives the call; `buf` holds `len` bytes, or is
    // null with `len` 0, as listxattr(2) takes them.
    sized(&|buf, len| unsafe { libc::listxattr(c.as_ptr(), buf, len, libc::XATTR_NOFOLLOW) })
}

#[cfg(target_os = "macos")]
fn get(c: &std::ffi::CStr, name: &std::ffi::CStr) -> io::Result<Vec<u8>> {
    // SAFETY: both names are NUL-terminated and outlive the call; `buf` holds `len` bytes,
    // or is null with `len` 0, as getxattr(2) takes them.
    sized(&|buf, len| unsafe {
        libc::getxattr(
            c.as_ptr(),
            name.as_ptr(),
            buf.cast(),
            len,
            0,
            libc::XATTR_NOFOLLOW,
        )
    })
}

/// What a snapshot of a regular file holds: the mode, size and time its bytes had when
/// taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Go's FileMode bits.
    pub mode: u32,
    pub size: u64,
    pub mtime: (i64, u32),
}

/// The smallest file a stage clones rather than packs. Measured on APFS
/// (docs/research/measurements/build-context/clone.py, platform-measurements.md M76): a
/// clone and its removal cost about 200 µs whatever the size, reading a file into the pack
/// 24 µs at 1 KiB and 98 µs at 256 KiB; at 1 MiB they meet, and at 4 MiB the clone is ten
/// times faster.
pub const CLONE_MIN: u64 = 1 << 20;

/// Whether a stage clones a directory at once ([`Stage::clone_dir`]): APFS's clonefile, on
/// macOS. Elsewhere each file is taken on its own, its system keeping writers off or
/// seeing them ([`settle`]).
pub const CLONES_DIRS: bool = cfg!(target_os = "macos");

/// Where a stage put a file's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taken {
    /// A clone, or copy, of its own.
    File(PathBuf),
    /// At this offset of the stage's pack.
    Pack(u64),
}

/// A directory private to one build that holds one version of each context file, which
/// no later edit of the file changes. Files under [`CLONE_MIN`] are appended to one pack
/// file, so taking one makes and removes no file; larger ones are copy-on-write clones
/// where the file system makes them (APFS's fclonefileat, Btrfs's and XFS's FICLONE),
/// else copies.
///
/// A source is held open while it is taken, never through a symlink put in its place nor
/// blocking on a FIFO, and what is taken is one version it had, or it is refused, as GNU
/// tar reports "file changed as we read it"; BuildKit's copy takes whatever it reads.
/// Reads and writes interleave on ext4 and APFS alike, so a read can take part of two
/// versions (PM M100): writers are kept off where the system lets a process do so, and
/// any write while the file is taken is seen where it does not ([`settle`]):
/// - Linux keeps them off with a read lease. Without one, the file's ctime shows any
///   write begun since it was looked at, once its tick has passed and a write under way
///   has finished.
/// - macOS lets no process but Apple's take a lease, and APFS stamps a write's times as
///   it ends: a write under way across the whole read leaves them as they were, which a
///   busy host's scheduler makes as likely for a small file as a large one. No read,
///   fsync, barrier or seek there waits out a write under way; a clone does, and is one
///   version of the file (PM M242). So a file is cloned into the stage first and taken
///   from its clone; or, in a directory the walk kept whole, read in place
///   ([`Stage::read`]) and the directory then cloned at once ([`Stage::clone_dir`]),
///   which waits out every write under way in it, whose end each file's times then show
///   ([`Stage::still`]): a clone's own pages are read from the disk, its source's from
///   memory. Where the file system makes no clone (another volume than the stage's, or
///   not APFS), the times decide, missing a write under way across the whole read.
/// - Windows: the source is opened sharing reads only, so no one writes it meanwhile.
#[derive(Debug)]
pub struct Stage {
    dir: PathBuf,
    pack: io::BufWriter<fs::File>,
    len: u64,
    next: u64,
    buf: Vec<u8>,
    /// Whether its files are a Linux tree's on NTFS ([`Stage::of_tree`]).
    #[cfg(windows)]
    linux: bool,
}

impl Stage {
    /// A stage in `dir`, which exists and is the build's own.
    pub fn new(dir: &Path) -> io::Result<Stage> {
        let pack = fs::File::options()
            .write(true)
            .create_new(true)
            .open(dir.join("pack"))?;
        Ok(Stage {
            dir: dir.to_path_buf(),
            pack: io::BufWriter::with_capacity(1 << 20, pack),
            len: 0,
            next: 0,
            buf: Vec::new(),
            #[cfg(windows)]
            linux: false,
        })
    }

    /// Has the stage take its files' modes as `tree` keeps them: a Linux tree's as WSL's
    /// metadata does ([`Tree`]).
    pub fn of_tree(&mut self, tree: &Tree) {
        #[cfg(windows)]
        {
            self.linux = tree.linux;
        }
        #[cfg(not(windows))]
        let _ = tree;
    }

    /// The snapshot of the regular file `f` that `meta` describes.
    fn snapshot(&self, f: &fs::File, meta: &fs::Metadata) -> io::Result<Snapshot> {
        #[cfg(windows)]
        if self.linux {
            return Ok(Snapshot {
                mode: lx::mode_of(f)?,
                size: meta.len(),
                mtime: since_epoch(meta),
            });
        }
        let _ = f;
        Ok(snapshot(meta))
    }

    /// The pack's path, to read it from.
    pub fn pack_path(&self) -> PathBuf {
        self.dir.join("pack")
    }

    /// Writes out what the pack holds, for it to be read.
    pub fn finish(mut self) -> io::Result<()> {
        io::Write::flush(&mut self.pack)
    }

    fn changed(src: &Path) -> io::Error {
        io::Error::other(format!("{}: file changed as the build read it", src.display()))
    }

    fn not_regular(src: &Path) -> io::Error {
        io::Error::other(format!("{}: not a regular file", src.display()))
    }

    /// Where the directories [`Stage::clone_dir`] clones go, each at its path in the
    /// context.
    pub fn tree(&self) -> PathBuf {
        Self::tree_in(&self.dir)
    }

    /// [`Stage::tree`] of a stage in `dir`, which nothing reads once its load is done.
    pub fn tree_in(dir: &Path) -> PathBuf {
        dir.join("tree")
    }

    /// Takes one version of the regular file at the path `src`, its last name never a link
    /// followed.
    pub fn take(&mut self, src: &Path) -> io::Result<(Snapshot, Taken)> {
        self.take_opened(open(src)?, src)
    }

    /// Takes one version of the regular file `f`, opened as [`Beneath::open`] opens one,
    /// which `src` names.
    pub fn take_opened(&mut self, f: fs::File, src: &Path) -> io::Result<(Snapshot, Taken)> {
        #[cfg(target_os = "macos")]
        if let Some(taken) = self.cloned(&f, src)? {
            return Ok(taken);
        }
        let before = settle(&f, src)?;
        let meta = &before.meta;
        if !meta.is_file() {
            return Err(Self::not_regular(src));
        }
        let size = meta.len();
        let taken = if size >= CLONE_MIN {
            let dst = self.dir.join(self.next.to_string());
            self.next += 1;
            if !clone_file(&f, &dst)? {
                copy(&f, &dst)?;
            }
            if !unchanged(&f, &before)? {
                fs::remove_file(&dst)?;
                return Err(Self::changed(src));
            }
            Taken::File(dst)
        } else {
            if !self.read_in(&f, size)? || !unchanged(&f, &before)? {
                return Err(Self::changed(src));
            }
            Taken::Pack(self.pack_read()?)
        };
        Ok((self.snapshot(&f, meta)?, taken))
    }

    /// Reads the regular file `src` in place into the pack: one version only if
    /// [`Stage::still`] says so of it once a write under way has ended, which the clone of
    /// its directory waits for ([`Stage::clone_dir`]).
    pub fn read(&mut self, src: &Path) -> io::Result<(Snapshot, Taken, Seen)> {
        self.read_opened(open(src)?, src)
    }

    /// [`Stage::read`] of the regular file `f`, opened as [`Beneath::open`] opens one,
    /// which `src` names.
    pub fn read_opened(&mut self, f: fs::File, src: &Path) -> io::Result<(Snapshot, Taken, Seen)> {
        let meta = f.metadata()?;
        if !meta.is_file() {
            return Err(Self::not_regular(src));
        }
        if !self.read_in(&f, meta.len())? {
            return Err(Self::changed(src));
        }
        let taken = Taken::Pack(self.pack_read()?);
        Ok((self.snapshot(&f, &meta)?, taken, Seen(Before { meta })))
    }

    /// Whether `src` is still what [`Stage::read`] saw: the same file, of its size, mtime
    /// and ctime.
    #[cfg(unix)]
    pub fn still(&self, src: &Path, seen: &Seen) -> io::Result<bool> {
        match fs::symlink_metadata(src) {
            Ok(now) => Ok(identity(&now) == identity(&seen.0.meta)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// On Windows no one wrote it while it was read (`open`): of its size and mtime still.
    #[cfg(windows)]
    pub fn still(&self, src: &Path, seen: &Seen) -> io::Result<bool> {
        let was = &seen.0.meta;
        match fs::symlink_metadata(src) {
            Ok(now) => Ok(now.len() == was.len() && now.modified()? == was.modified()?),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Reads `size` bytes of `f` into the buffer: false if it held more or fewer.
    fn read_in(&mut self, f: &fs::File, size: u64) -> io::Result<bool> {
        self.buf.clear();
        io::Read::read_to_end(&mut io::Read::take(f, size + 1), &mut self.buf)?;
        Ok(self.buf.len() as u64 == size)
    }

    /// Appends what the buffer holds to the pack: where it begins.
    fn pack_read(&mut self) -> io::Result<u64> {
        io::Write::write_all(&mut self.pack, &self.buf)?;
        let at = self.len;
        self.len += self.buf.len() as u64;
        Ok(at)
    }

    /// macOS: `f` cloned into the stage and taken from its clone, one version of it; none
    /// where the file system makes no clone of it. A packed file's clone is done with.
    #[cfg(target_os = "macos")]
    fn cloned(&mut self, f: &fs::File, src: &Path) -> io::Result<Option<(Snapshot, Taken)>> {
        if !f.metadata()?.is_file() {
            return Err(Self::not_regular(src));
        }
        let dst = self.dir.join(self.next.to_string());
        self.next += 1;
        if !clone_file(f, &dst)? {
            return Ok(None);
        }
        // What it holds, its mode and its mtime are its source's as it was cloned, and no
        // one writes it.
        let taken = (|| {
            let c = open(&dst)?;
            let meta = c.metadata()?;
            let taken = match meta.len() >= CLONE_MIN {
                true => Taken::File(dst.clone()),
                false if self.read_in(&c, meta.len())? => Taken::Pack(self.pack_read()?),
                false => return Err(Self::changed(src)),
            };
            Ok((snapshot(&meta), taken))
        })();
        if !matches!(taken, Ok((_, Taken::File(_)))) {
            remove_clone(&dst)?;
        }
        taken.map(Some)
    }

    /// Clones the directory `rel` of the tree `beneath` opens, all it holds, to `rel` in the
    /// stage's [`tree`](Self::tree) at once, as the barrier [`Stage::read`]'s files wait on:
    /// the clone of each file in it waits out a write under way, whose end its times then
    /// show (PM M242). False where no clone of it is made: on macOS's file systems but APFS
    /// or from another volume than the stage's (clonefile(2), ENOTSUP and EXDEV), with a file
    /// or directory in it this process may not read (EACCES, PM M242); what was made of it
    /// goes with the stage. A directory below the root is cloned from its parent's
    /// descriptor (clonefileat(2)), opened beneath the root as its files are, so no
    /// directory a link replaced since the walk is cloned through it.
    #[cfg(target_os = "macos")]
    pub fn clone_dir(&mut self, beneath: &mut Beneath, rel: &[u8]) -> io::Result<bool> {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        // <sys/clonefile.h>: links cloned as links; the caller's ownership, as a user's
        // clone is given anyway.
        const CLONE_NOFOLLOW: u32 = 0x0001;
        const CLONE_NOOWNERCOPY: u32 = 0x0002;
        // The root's is the tree itself: a path ending in `/`, which a join of nothing
        // makes, names no file clonefile makes.
        let dst = match rel.is_empty() {
            true => self.tree(),
            false => path(&self.tree(), rel),
        };
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        let c = |p: &[u8]| CString::new(p).map_err(|_| io::Error::other("NUL in path"));
        let d = c(dst.as_os_str().as_bytes())?;
        let flags = CLONE_NOFOLLOW | CLONE_NOOWNERCOPY;
        if rel.is_empty() {
            let s = c(beneath.root.as_os_str().as_bytes())?;
            // SAFETY: both paths are NUL-terminated.
            return Ok(unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), flags) } == 0);
        }
        let (dir, name) = beneath.parent(rel)?;
        let s = c(name)?;
        // SAFETY: a descriptor `beneath` holds open, and both names NUL-terminated.
        Ok(unsafe { libc::clonefileat(dir.as_raw_fd(), s.as_ptr(), libc::AT_FDCWD, d.as_ptr(), flags) } == 0)
    }

    /// Elsewhere no directory is cloned at once: its files are taken one at a time.
    #[cfg(not(target_os = "macos"))]
    pub fn clone_dir(&mut self, _: &mut Beneath, _: &[u8]) -> io::Result<bool> {
        Ok(false)
    }
}

/// Removes a clone the stage is done with: one of a file with flags that forbid it, an
/// immutable file's (`uchg`), which a clone keeps, cleared first.
#[cfg(target_os = "macos")]
fn remove_clone(p: &Path) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::macos::fs::MetadataExt;
    if fs::symlink_metadata(p)?.st_flags() & libc::UF_SETTABLE != 0 {
        let f = open(p)?;
        // SAFETY: the descriptor is open for the call.
        if unsafe { libc::fchflags(f.as_raw_fd(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    fs::remove_file(p)
}

#[cfg(unix)]
fn open(src: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(src)
}

/// `src` opened to be taken, never through a link put in its place since the walk, as
/// O_NOFOLLOW refuses one on Unix (shards_ntfs::open_unfollowed: a symbolic link refused
/// ERROR_STOPPED_ON_SYMLINK, a junction as a directory is, a cloud placeholder, deduplicated
/// or compressed file read as what it holds).
#[cfg(windows)]
fn open(src: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    // Others may read, not write or delete, while it is held.
    let mut options = fs::File::options();
    options.read(true).share_mode(FILE_SHARE_READ);
    shards_ntfs::open_unfollowed(&options, 0, src)
}

/// A tree's files opened beneath its root ([`Tree::beneath`]), through no link anywhere
/// below it: each directory under the root opened in the one above it, never following a
/// symlink (openat(2) O_NOFOLLOW|O_DIRECTORY; on Windows NtCreateFile relative to its
/// parent's handle, a link refused, `shards_ntfs::open_unfollowed_at`), and the file in the
/// last as [`Stage::take`] opens one (O_NOFOLLOW, Windows' read sharing). So a directory of
/// the context a link replaced after the walk is refused as a link, where a path's open
/// follows every name but its last (fsutil's own `fs.Open`, `os.Open(filepath.Join(root,
/// p))`, which buildx's `fsutil.NewFS` gives, follows them all). A directory renamed after
/// it was opened is the one the walk saw, wherever it went. The directory of the last file
/// opened is held, as the walk sends a directory's files together, so a file's open is one
/// lookup of one name; each name a plain one, never empty, `.` or `..`.
///
/// Neither openat2(2) (RESOLVE_BENEATH, Linux 5.6) nor macOS's O_NOFOLLOW_ANY is needed:
/// one openat of each directory's name, every kernel and both systems alike, keeps every
/// name of the walk to the root's own directories.
#[derive(Debug)]
pub struct Beneath {
    /// The root's path, which macOS clones the root's whole directory by.
    #[cfg(target_os = "macos")]
    root: PathBuf,
    dir: fs::File,
    /// The directory below the root the last file opened is in: its name relative to the
    /// root and its handle.
    held: Option<(Vec<u8>, fs::File)>,
    /// Whether its names are a Linux tree's on NTFS ([`Tree`]), Windows' alone.
    linux: bool,
}

impl Beneath {
    /// The directory `rel`'s last name is in, opened a name at a time from the root (or
    /// held from the last open), and that name.
    fn parent<'a>(&mut self, rel: &'a [u8]) -> io::Result<(&fs::File, &'a [u8])> {
        let (dir, last) = match rel.iter().rposition(|&c| c == b'/') {
            Some(i) => (
                rel.get(..i).unwrap_or_default(),
                rel.get(i + 1..).unwrap_or_default(),
            ),
            None => (b"".as_slice(), rel),
        };
        plain(last)?;
        if dir.is_empty() {
            return Ok((&self.dir, last));
        }
        if self.held.as_ref().is_none_or(|(held, _)| held.as_slice() != dir) {
            self.held = None;
            let mut at: Option<fs::File> = None;
            for name in dir.split(|&c| c == b'/') {
                plain(name)?;
                at = Some(open_dir_in(at.as_ref().unwrap_or(&self.dir), name, self.linux)?);
            }
            self.held = at.map(|f| (dir.to_vec(), f));
        }
        match &self.held {
            Some((_, f)) => Ok((f, last)),
            None => Ok((&self.dir, last)),
        }
    }

    /// The regular file `rel` (slash-separated, as the walk names it) opened beneath the
    /// root, to be taken ([`Stage::take_opened`]).
    pub fn open(&mut self, rel: &[u8]) -> io::Result<fs::File> {
        let linux = self.linux;
        let (dir, last) = self.parent(rel)?;
        open_in(dir, last, linux)
    }
}

/// `name`, if it names an entry of a directory and no more.
fn plain(name: &[u8]) -> io::Result<()> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{:?}: not a plain name", String::from_utf8_lossy(name)),
        ));
    }
    Ok(())
}

/// openat(2) of `name` in `dir`, retried where a signal interrupts it.
#[cfg(unix)]
fn openat(dir: &fs::File, name: &[u8], flags: libc::c_int) -> io::Result<fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let c = std::ffi::CString::new(name).map_err(|_| io::Error::other("NUL in name"))?;
    loop {
        // SAFETY: a NUL-terminated name and a descriptor `dir` holds open.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd >= 0 {
            // SAFETY: a descriptor openat just opened, owned by nothing else.
            return Ok(fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) }));
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e);
        }
    }
}

/// The directory `name` in `dir`, never through a symlink: one there is refused, ELOOP or
/// ENOTDIR (open(2): O_NOFOLLOW, O_DIRECTORY).
#[cfg(unix)]
fn open_dir_in(dir: &fs::File, name: &[u8], _linux: bool) -> io::Result<fs::File> {
    openat(dir, name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW)
}

/// The file `name` in `dir`, as [`open`] opens a path's last name.
#[cfg(unix)]
fn open_in(dir: &fs::File, name: &[u8], _linux: bool) -> io::Result<fs::File> {
    openat(dir, name, libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
}

/// `name` as NTFS keeps it: a Linux tree's as [`shards_ntfs::nt_name`] escapes it, a host
/// tree's as the walk read it ([`name_bytes`]).
#[cfg(windows)]
fn nt_name(name: &[u8], linux: bool) -> io::Result<Vec<u16>> {
    if linux {
        return shards_ntfs::nt_name(name).map_err(shards_ntfs::io_error);
    }
    Ok(String::from_utf8_lossy(name).encode_utf16().collect())
}

/// The directory `name` in `dir`, relative to its handle and through no link: one there
/// refused, ERROR_STOPPED_ON_SYMLINK (`shards_ntfs::open_unfollowed_at`).
#[cfg(windows)]
fn open_dir_in(dir: &fs::File, name: &[u8], linux: bool) -> io::Result<fs::File> {
    use std::os::windows::io::AsHandle as _;
    use windows_sys::Wdk::Storage::FileSystem::{FILE_DIRECTORY_FILE, FILE_OPEN};
    use windows_sys::Win32::Storage::FileSystem::{FILE_LIST_DIRECTORY, FILE_TRAVERSE};
    shards_ntfs::open_unfollowed_at(
        dir.as_handle(),
        &nt_name(name, linux)?,
        FILE_LIST_DIRECTORY | FILE_TRAVERSE,
        shards_ntfs::SHARE_ALL,
        FILE_OPEN,
        FILE_DIRECTORY_FILE,
    )
}

/// The file `name` in `dir`, as [`open`] opens a path's last name: through no link,
/// sharing reads alone, a directory refused as CreateFileW refuses one opened as a file.
#[cfg(windows)]
fn open_in(dir: &fs::File, name: &[u8], linux: bool) -> io::Result<fs::File> {
    use std::os::windows::io::AsHandle as _;
    use windows_sys::Wdk::Storage::FileSystem::{FILE_NON_DIRECTORY_FILE, FILE_OPEN};
    use windows_sys::Win32::Storage::FileSystem::{FILE_GENERIC_READ, FILE_SHARE_READ};
    shards_ntfs::open_unfollowed_at(
        dir.as_handle(),
        &nt_name(name, linux)?,
        FILE_GENERIC_READ,
        FILE_SHARE_READ,
        FILE_OPEN,
        FILE_NON_DIRECTORY_FILE,
    )
}

/// What a source was as its taking began, which it must still be once taken.
#[derive(Debug)]
struct Before {
    meta: fs::Metadata,
}

/// What [`Stage::read`] saw of a file, for [`Stage::still`].
#[derive(Debug)]
pub struct Seen(Before);

/// The snapshot of a regular file `meta` describes.
fn snapshot(meta: &fs::Metadata) -> Snapshot {
    Snapshot {
        mode: mode_of(meta),
        size: meta.len(),
        mtime: since_epoch(meta),
    }
}

/// Readies `f` to be taken, and says what it was. A read lease, which Linux grants while
/// no one has the file open for writing (fs/locks.c, lease_open_conflict), makes one who
/// opens it for writing, or truncates it, wait until the lease goes with `f` (fs/open.c,
/// break_lease): what is read is one version.
///
/// Without one, a write while `f` is taken must show in its ctime, which Linux stamps as
/// a write begins, holding the inode's lock until its last byte is in place (fs/ext4/
/// file.c, ext4_buffered_write_iter and ext4_write_checks). A ctime still in the tick of
/// the coarse clock it is stamped from (fs/inode.c, current_time) is waited past, as a
/// write stamped in that tick too would leave it as it was; and lseek(2) SEEK_DATA, which
/// takes the inode's lock shared on ext4, Btrfs and tmpfs (and overlayfs's, theirs),
/// waits out a write under way, as XFS's reads take its own. A file written again while
/// its tick is waited past is refused.
#[cfg(target_os = "linux")]
fn settle(f: &fs::File, src: &Path) -> io::Result<Before> {
    use std::os::unix::fs::MetadataExt;
    if lease(f) {
        return Ok(Before { meta: f.metadata()? });
    }
    for _ in 0..2 {
        let now = coarse_now()?;
        let meta = f.metadata()?;
        let (ctime, grain) = (nanos(meta.ctime(), meta.ctime_nsec()), grain(meta.ctime_nsec()));
        match unseen_until(ctime, grain, now) {
            None => {
                barrier(f)?;
                return Ok(Before { meta });
            }
            Some(after) => {
                while coarse_now()? < after {
                    std::thread::sleep(std::time::Duration::from_nanos(
                        u64::try_from(after - coarse_now()?).unwrap_or(0),
                    ));
                }
            }
        }
    }
    Err(Stage::changed(src))
}

/// F_SETSIG, the signal a lease's break is told with (include/uapi/asm-generic/fcntl.h,
/// which x86_64 and arm64 take): the libc crate has it for none of shards' targets.
#[cfg(target_os = "linux")]
const F_SETSIG: libc::c_int = 10;

/// Takes a read lease on `f`, if Linux grants one. Its break is told with SIGURG, which
/// a process ignores unless it handles it, rather than SIGIO, which ends one that does
/// not: the lease goes as soon as the file is taken, and the writer waiting with it.
#[cfg(target_os = "linux")]
fn lease(f: &fs::File) -> bool {
    use std::os::fd::AsRawFd;
    let fd = f.as_raw_fd();
    // SAFETY: fcntl(2) on a descriptor `f` holds open.
    unsafe {
        libc::fcntl(fd, F_SETSIG, libc::SIGURG) == 0 && libc::fcntl(fd, libc::F_SETLEASE, libc::F_RDLCK) == 0
    }
}

/// Waits out a write of `f` under way: lseek(2) SEEK_DATA, under the inode's lock, which
/// the write holds; the offset is put back.
#[cfg(target_os = "linux")]
fn barrier(f: &fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd = f.as_raw_fd();
    // SAFETY: lseek(2) on a descriptor `f` holds open. What SEEK_DATA finds, an offset or
    // ENXIO in a file of holes alone, is not wanted.
    unsafe {
        libc::lseek(fd, 0, libc::SEEK_DATA);
        if libc::lseek(fd, 0, libc::SEEK_SET) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// CLOCK_REALTIME_COARSE, the clock Linux stamps file times from, in nanoseconds.
#[cfg(target_os = "linux")]
fn coarse_now() -> io::Result<i128> {
    // SAFETY: an all-zero timespec is valid; clock_gettime(2) fills it.
    let mut t: libc::timespec = unsafe { std::mem::zeroed() };
    // SAFETY: as above, into a timespec of ours.
    if unsafe { libc::clock_gettime(libc::CLOCK_REALTIME_COARSE, &mut t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(nanos(t.tv_sec, t.tv_nsec))
}

#[cfg(target_os = "linux")]
fn nanos(sec: i64, nsec: i64) -> i128 {
    i128::from(sec) * 1_000_000_000 + i128::from(nsec)
}

/// The coarsest stamp a file system could have given a time of `nsec`: the largest power
/// of ten that divides it, a second where it is 0. ext4's 128-byte inodes keep seconds
/// alone, FAT's ctime hundredths (fs/ext4/super.c, fs/fat/inode.c, s_time_gran).
#[cfg(target_os = "linux")]
fn grain(nsec: i64) -> i128 {
    if nsec == 0 {
        return 1_000_000_000;
    }
    let (mut n, mut g) = (nsec, 1i128);
    while n % 10 == 0 {
        n /= 10;
        g *= 10;
    }
    g
}

/// When a write could leave a ctime of `ctime`, stamped in steps of `grain`, as it was,
/// the coarse clock being at `now`: until the clock leaves its step, if it is in it or
/// within a second ahead of it (a step back of the clock leaves times further ahead,
/// which a write now would change); `None` where a write now changes it.
#[cfg(target_os = "linux")]
fn unseen_until(ctime: i128, grain: i128, now: i128) -> Option<i128> {
    let step = now - now.rem_euclid(grain);
    (ctime >= step && ctime - step <= 1_000_000_000).then(|| ctime - ctime.rem_euclid(grain) + grain)
}

/// macOS keeps leases for Apple's own processes (bsd/vfs/vfs_subr.c, vnode_setlease: the
/// private entitlement com.apple.private.vfs.file-leases): what the file was is all.
#[cfg(target_os = "macos")]
fn settle(f: &fs::File, _: &Path) -> io::Result<Before> {
    Ok(Before { meta: f.metadata()? })
}

/// On Windows no one writes it while it is open (`open`).
#[cfg(windows)]
fn settle(f: &fs::File, _: &Path) -> io::Result<Before> {
    Ok(Before { meta: f.metadata()? })
}

/// Whether `f` is as `before` was: its identity, size, mtime and ctime.
#[cfg(unix)]
fn unchanged(f: &fs::File, before: &Before) -> io::Result<bool> {
    Ok(identity(&before.meta) == identity(&f.metadata()?))
}

/// A file's identity, size, mtime and ctime.
#[cfg(unix)]
fn identity(m: &fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    use std::os::unix::fs::MetadataExt;
    (
        m.dev(),
        m.ino(),
        m.size(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    )
}

/// On Windows no one could write it meanwhile.
#[cfg(windows)]
fn unchanged(_: &fs::File, _: &Before) -> io::Result<bool> {
    Ok(true)
}

/// Go's FileMode permission and set-ID bits of a regular file.
#[cfg(unix)]
fn mode_of(m: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    let st = m.mode();
    let mut mode = st & 0o777;
    if st & 0o4000 != 0 {
        mode |= fm::SETUID;
    }
    if st & 0o2000 != 0 {
        mode |= fm::SETGID;
    }
    if st & 0o1000 != 0 {
        mode |= fm::STICKY;
    }
    mode
}

/// Go's 0666, or 0444 read-only, made executable and capped at 0755, as fsutil sends
/// Windows files.
#[cfg(windows)]
fn mode_of(m: &fs::Metadata) -> u32 {
    let mode = if m.permissions().readonly() { 0o444 } else { 0o666 };
    ((mode & fm::PERM) | 0o111) & 0o755
}

/// Copies what `f` holds to a new file `dst`, from its start.
fn copy(f: &fs::File, dst: &Path) -> io::Result<()> {
    use std::io::{Seek, SeekFrom};
    let mut src = f;
    src.seek(SeekFrom::Start(0))?;
    let mut out = fs::File::options().write(true).create_new(true).open(dst)?;
    io::copy(&mut src, &mut out)?;
    Ok(())
}

/// A copy-on-write clone of `f` at `dst`, a new file; false where the file system makes
/// none, which leaves nothing at `dst`. The daemon's restores clone their snapshots'
/// writable layers with it too (D190).
#[cfg(target_os = "macos")]
pub fn clone_file(f: &fs::File, dst: &Path) -> io::Result<bool> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(dst.as_os_str().as_bytes()).map_err(|_| io::Error::other("NUL in path"))?;
    // SAFETY: the descriptor is open for the call and `c` is NUL-terminated.
    let r = unsafe { libc::fclonefileat(f.as_raw_fd(), libc::AT_FDCWD, c.as_ptr(), 0) };
    if r == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOTSUP | libc::EXDEV) => Ok(false),
        _ => Err(e),
    }
}

#[cfg(target_os = "linux")]
pub fn clone_file(f: &fs::File, dst: &Path) -> io::Result<bool> {
    use std::os::fd::AsRawFd;
    let out = fs::File::options().write(true).create_new(true).open(dst)?;
    // SAFETY: both descriptors are open for the call; FICLONE reads the source's.
    let r = unsafe { libc::ioctl(out.as_raw_fd(), libc::FICLONE, f.as_raw_fd()) };
    if r == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    drop(out);
    fs::remove_file(dst)?;
    match e.raw_os_error() {
        Some(libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL | libc::ENOTTY) => Ok(false),
        _ => Err(e),
    }
}

/// Windows has no clone shards makes yet: a copy.
#[cfg(windows)]
pub fn clone_file(_: &fs::File, _: &Path) -> io::Result<bool> {
    Ok(false)
}

/// A tree a walk reads ([`crate::context`]): the host's own files, as fsutil reads them, or
/// on Windows a Linux tree kept on NTFS as WSL keeps one, whole, by shards
/// (`shards_ntfs::LINUX_TREE`, shards-archive's `apply_layer_lx`: the copy a Windows machine
/// keeps of a client's context, D180), read as fsutil reads a Linux host's: its names, modes,
/// links, devices, links' counts and extended attributes Linux's. A tree of the host's own is
/// read as fsutil reads it on Windows, though WSL kept some of its files' metadata, as
/// Docker's builds of it are.
#[derive(Debug, Clone)]
pub struct Tree {
    root: PathBuf,
    /// Whether it is a Linux tree on NTFS.
    #[cfg(windows)]
    linux: bool,
}

impl Tree {
    /// The tree at `root`: on Windows a Linux tree where it is marked one, its root then
    /// taken as its verbatim path, which Windows normalizes no name of.
    pub fn at(root: &Path) -> Tree {
        #[cfg(windows)]
        if lx::is_tree(root)
            && let Ok(verbatim) = fs::canonicalize(root)
        {
            return Tree {
                root: verbatim,
                linux: true,
            };
        }
        Tree {
            root: root.to_path_buf(),
            #[cfg(windows)]
            linux: false,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Its files opened beneath its root, held open from now ([`Beneath`]). The root itself
    /// is followed as given, as `eval_symlinks` resolved it before the walk.
    pub fn beneath(&self) -> io::Result<Beneath> {
        let mut options = fs::File::options();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_DIRECTORY);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
            options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
        }
        Ok(Beneath {
            #[cfg(target_os = "macos")]
            root: self.root.clone(),
            dir: options.open(&self.root)?,
            held: None,
            #[cfg(windows)]
            linux: self.linux,
            #[cfg(not(windows))]
            linux: false,
        })
    }

    /// A path below the root from its slash-separated relative name.
    pub fn path(&self, rel: &[u8]) -> PathBuf {
        #[cfg(windows)]
        if self.linux {
            return lx::path(&self.root, rel);
        }
        path(&self.root, rel)
    }

    /// [`lstat`] of the relative name `rel`.
    pub fn lstat(&self, rel: &[u8]) -> io::Result<Stat> {
        #[cfg(windows)]
        if self.linux {
            return lx::lstat(&self.path(rel));
        }
        lstat(&self.path(rel))
    }

    /// [`read_dir`] of the relative name `rel`, the root's where it is empty.
    pub fn read_dir(&self, rel: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        #[cfg(windows)]
        if self.linux {
            return lx::read_dir(&self.path(rel));
        }
        read_dir(&self.path(rel))
    }
}

/// A Linux tree on NTFS ([`Tree`]), read through shards-ntfs.
#[cfg(windows)]
mod lx {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsHandle as _;

    use shards_ntfs as ntfs;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    use super::*;

    /// `p` itself, a link not followed, opened to read it, its attributes, EAs and reparse
    /// point.
    fn open(p: &Path) -> io::Result<fs::File> {
        fs::OpenOptions::new()
            .access_mode(FILE_GENERIC_READ)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(p)
    }

    /// Whether `root` is a Linux tree's root.
    pub(super) fn is_tree(root: &Path) -> bool {
        open(root).is_ok_and(|f| ntfs::is_linux_tree(f.as_handle()).unwrap_or(false))
    }

    /// The path below `root` of the slash-separated Linux name `rel`, each name as NTFS
    /// keeps it ([`ntfs::nt_name`]); one NTFS keeps none of (not UTF-8) as itself, which
    /// names nothing there.
    pub(super) fn path(root: &Path, rel: &[u8]) -> PathBuf {
        let mut wide: Vec<u16> = root.as_os_str().encode_wide().collect();
        for part in rel.split(|&c| c == b'/').filter(|p| !p.is_empty()) {
            wide.push(u16::from(b'\\'));
            match ntfs::nt_name(part) {
                Ok(name) => wide.extend(name),
                Err(_) => wide.extend(String::from_utf8_lossy(part).encode_utf16()),
            }
        }
        PathBuf::from(OsString::from_wide(&wide))
    }

    /// fsutil's mkstat of what `p` is on Linux: its kind, mode and device's numbers WSL's
    /// metadata, a symlink's target its LX reparse point's, its file's ID and links' count
    /// its inode, its extended attributes those kept as EAs (`com.apple.*` aside).
    pub(super) fn lstat(p: &Path) -> io::Result<Stat> {
        let f = open(p)?;
        let h = f.as_handle();
        let st = ntfs::stat(h).map_err(ntfs::io_error)?;
        let (mode, socket) = go_mode(st.mode);
        let dir = mode & fm::DIR != 0;
        let link = if mode & fm::SYMLINK != 0 {
            ntfs::readlink(h).map_err(ntfs::io_error)?
        } else {
            Vec::new()
        };
        let device = matches!(st.mode & 0o170_000, 0o060_000 | 0o020_000);
        let rdev = st.rdev;
        let xattrs = ntfs::eas(h)
            .map_err(ntfs::io_error)?
            .into_iter()
            .filter_map(|(ea, value)| {
                let name = ntfs::ea_xattr(&ea)?;
                let value = value.strip_prefix(ntfs::XATTR_KEPT)?.to_vec();
                (!name.starts_with(b"com.apple.")).then_some((name, value))
            })
            .collect();
        Ok(Stat {
            mode,
            size: if dir { 0 } else { st.size },
            mtime: (i64::try_from(st.mtime.0).unwrap_or(i64::MAX), st.mtime.1),
            inode: (!dir).then_some((u128::from(st.ino), u64::from(st.nlink))),
            // fsutil's major and minor, Linux's encoding.
            devmajor: if device {
                u32::try_from((rdev >> 8) & 0xfff).unwrap_or(0)
            } else {
                0
            },
            devminor: if device {
                u32::try_from((rdev & 0xff) | ((rdev >> 12) & 0xf_ff00)).unwrap_or(0)
            } else {
                0
            },
            link,
            xattrs,
            socket,
        })
    }

    /// The Linux names in the directory `dir`, sorted, as `os.ReadDir` gives them.
    pub(super) fn read_dir(dir: &Path) -> io::Result<Vec<Vec<u8>>> {
        let mut names = Vec::new();
        for e in fs::read_dir(dir)? {
            let wide: Vec<u16> = e?.file_name().encode_wide().collect();
            names.push(ntfs::guest_name(&wide));
        }
        names.sort();
        Ok(names)
    }

    /// Go's FileMode permission and set-ID bits of the open regular file `f`, as WSL's
    /// metadata keeps them.
    pub(super) fn mode_of(f: &fs::File) -> io::Result<u32> {
        let st = ntfs::stat(f.as_handle()).map_err(ntfs::io_error)?;
        Ok(go_mode(st.mode & 0o7777).0)
    }
}

/// `filepath.EvalSymlinks`, as fsutil.NewFS resolves the context's directory.
pub fn eval_symlinks(p: &Path) -> io::Result<std::path::PathBuf> {
    fs::canonicalize(p)
}

#[cfg(test)]
#[cfg(target_os = "linux")]
mod tests {
    use super::*;

    /// A file of `bytes` in a directory of the test's own.
    fn file(name: &str, bytes: &[u8]) -> (shards_testdir::TempDir, PathBuf) {
        let dir = shards_testdir::TempDir::new(&format!("host-{name}")).unwrap();
        let path = dir.join("f");
        fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn a_times_grain_is_the_power_of_ten_it_could_have_been_stamped_in() {
        assert_eq!(grain(0), 1_000_000_000);
        assert_eq!(grain(123_456_789), 1);
        assert_eq!(grain(120_000_000), 10_000_000);
        assert_eq!(grain(500_000_000), 100_000_000);
    }

    /// A ctime is waited past while a write now could leave it as it is: in the coarse
    /// clock's step, or a little ahead of it; not once the clock is past its step, nor
    /// far ahead of the clock, as after the clock was set back.
    #[test]
    fn a_ctime_is_waited_past_only_while_a_write_could_leave_it() {
        let s = 1_000_000_000i128;
        // Nanosecond stamps: unseen until the clock passes it.
        assert_eq!(unseen_until(100 * s + 5, 1, 100 * s + 5), Some(100 * s + 6));
        assert_eq!(unseen_until(100 * s + 5, 1, 100 * s + 4), Some(100 * s + 6));
        assert_eq!(unseen_until(100 * s + 5, 1, 100 * s + 6), None);
        // Second stamps: unseen until the clock's second is past it.
        assert_eq!(unseen_until(100 * s, s, 100 * s + 999), Some(101 * s));
        assert_eq!(unseen_until(100 * s, s, 101 * s), None);
        // Far ahead of the clock: a write now stamps an earlier time.
        assert_eq!(unseen_until(200 * s, 1, 100 * s), None);
    }

    /// A read lease is granted where no one has the file open for writing, and one who
    /// opens it for writing meanwhile waits until it goes; none is granted while one has.
    #[test]
    fn a_lease_keeps_writers_off_while_it_is_held() {
        let (_dir, path) = file("lease", b"one");
        let f = open(&path).unwrap();
        assert!(lease(&f), "{}", io::Error::last_os_error());
        let held = std::time::Duration::from_millis(200);
        // Timed from before the writer starts: the lease goes no sooner than `held` after
        // it, where the writer's own start may come later.
        let start = std::time::Instant::now();
        let opened = std::thread::scope(|s| {
            let writer = s.spawn(|| {
                let w = fs::File::options().write(true).open(&path).unwrap();
                (start.elapsed(), w)
            });
            std::thread::sleep(held);
            drop(f);
            writer.join().unwrap()
        });
        assert!(opened.0 >= held, "the writer opened it in {:?}", opened.0);
        let f = open(&path).unwrap();
        assert!(!lease(&f), "granted while a writer has it open");
        drop(opened.1);
        assert!(lease(&f), "{}", io::Error::last_os_error());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }
}
