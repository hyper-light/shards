//! A step's root, and a mounted tree, as their own contents see them, from outside them:
//! every path is resolved by the kernel inside the root (openat2(2) `RESOLVE_IN_ROOT`,
//! Linux 5.6), so that no symlink, planted by an earlier step or by the step itself, leads
//! the builder past it. runc resolves a container's paths so for the same reason
//! (CVE-2019-19921, CVE-2021-30465: filepath-securejoin, then openat2), as BuildKit
//! resolves a step's working directory and its mounts' sources (fs.RootPath).
//!
//! A path resolved here is used through its descriptor: created relative to its resolved
//! parent (`mkdirat`, `openat`, `mknodat`, `symlinkat`, `unlinkat`), and mounted on through
//! `/proc/self/fd/N`, as runc mounts on its targets.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;

/// A directory every path is resolved in.
#[derive(Debug)]
pub struct Root(OwnedFd);

/// Symlinks resolved as if the root were `/`, and no magic links (`/proc/self/fd/N`)
/// followed out of it.
const RESOLVE: u64 = libc::RESOLVE_IN_ROOT | libc::RESOLVE_NO_MAGICLINKS;

/// `path` without its leading slashes: what it names relative to a root.
fn relative(path: &[u8]) -> &[u8] {
    let start = path.iter().position(|&b| b != b'/').unwrap_or(path.len());
    path.get(start..).unwrap_or_default()
}

fn cstr(b: &[u8]) -> io::Result<CString> {
    CString::new(b).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path holds NUL"))
}

/// The path a descriptor is reached by, for calls that take no descriptor (mount(2)).
pub fn path(fd: &OwnedFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

/// What Go's syscall error says of an errno, as runc's messages carry it.
fn go_text(e: &io::Error) -> String {
    e.raw_os_error()
        .map_or_else(|| e.to_string(), shards_cmdline::go::linux_error)
}

/// A Go `*PathError`: `op path: reason`, for a path of the root, shown from it.
pub fn path_error(op: &str, path: &[u8], e: &io::Error) -> io::Error {
    io::Error::new(
        e.kind(),
        format!(
            "{op} /{}: {}",
            String::from_utf8_lossy(relative(path)),
            go_text(e)
        ),
    )
}

/// A directory and the last name of `path`, both relative to the root.
fn split(path: &[u8]) -> (&[u8], &[u8]) {
    let rel = relative(path);
    let rel = rel
        .get(..rel.len() - rel.iter().rev().take_while(|&&b| b == b'/').count())
        .unwrap_or(rel);
    match rel.iter().rposition(|&b| b == b'/') {
        Some(i) => (
            rel.get(..i).unwrap_or_default(),
            rel.get(i + 1..).unwrap_or_default(),
        ),
        None => (&[], rel),
    }
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl Root {
    /// The directory at `dir`, a path of the builder's own.
    pub fn open(dir: &std::path::Path) -> io::Result<Root> {
        use std::os::unix::ffi::OsStrExt;
        let c = cstr(dir.as_os_str().as_bytes())?;
        // SAFETY: open(2) of a NUL-terminated path.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) };
        check(fd)?;
        // SAFETY: a descriptor just opened, ours alone.
        Ok(Root(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// `path` of the root, opened with `flags`, its symlinks resolved in the root.
    pub fn open_at(&self, path: &[u8], flags: libc::c_int) -> io::Result<OwnedFd> {
        let rel = relative(path);
        let c = cstr(if rel.is_empty() { b"." } else { rel })?;
        // SAFETY: open_how is plain data; zeroed is its every field unset.
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags = (flags | libc::O_CLOEXEC) as u64;
        how.resolve = RESOLVE;
        // SAFETY: openat2(2) with a NUL-terminated path and an open_how of the size given.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.0.as_raw_fd(),
                c.as_ptr(),
                &how as *const libc::open_how,
                std::mem::size_of::<libc::open_how>(),
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = libc::c_int::try_from(fd).map_err(|_| io::Error::other("openat2 returned no descriptor"))?;
        // SAFETY: a descriptor just opened, ours alone.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// The directory at `path`, made with each missing parent as Go's `os.MkdirAll` makes
    /// them (`mode`, then each owned `owner` if given), as runc and BuildKit make a
    /// working directory or a mount point; its errors are Go's, `mkdir /a: not a
    /// directory`.
    pub fn mkdir_all(&self, path: &[u8], mode: u32, owner: Option<(u32, u32)>) -> io::Result<OwnedFd> {
        // A directory there is the answer, and anything else there an error at once.
        if let Ok(fd) = self.open_at(path, libc::O_PATH) {
            if is_dir(&fd)? {
                return Ok(fd);
            }
            return Err(path_error(
                "mkdir",
                path,
                &io::Error::from_raw_os_error(libc::ENOTDIR),
            ));
        }
        let (parent, name) = split(path);
        if name.is_empty() {
            return self.open_at(b".", libc::O_PATH | libc::O_DIRECTORY);
        }
        let dir = if parent.is_empty() {
            self.open_at(b".", libc::O_PATH | libc::O_DIRECTORY)?
        } else {
            self.mkdir_all(parent, mode, owner)?
        };
        let c = cstr(name)?;
        // SAFETY: mkdirat(2) relative to a descriptor of ours, of a NUL-terminated name.
        if let Err(e) = check(unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), mode) }) {
            // Made meanwhile, as Go double-checks "foo/.".
            return match self.open_at(path, libc::O_PATH | libc::O_DIRECTORY) {
                Ok(fd) => Ok(fd),
                Err(_) => Err(path_error("mkdir", path, &e)),
            };
        }
        if let Some((uid, gid)) = owner {
            // SAFETY: fchownat(2) of a name just made, not following it.
            check(unsafe {
                libc::fchownat(dir.as_raw_fd(), c.as_ptr(), uid, gid, libc::AT_SYMLINK_NOFOLLOW)
            })
            .map_err(|e| path_error("lchown", path, &e))?;
        }
        // mkdirat applies the umask; the mode is whole, as MkdirAllAndChown leaves it.
        // SAFETY: fchmodat(2) of a directory just made.
        check(unsafe { libc::fchmodat(dir.as_raw_fd(), c.as_ptr(), mode, 0) })
            .map_err(|e| path_error("chmod", path, &e))?;
        self.open_at(path, libc::O_PATH | libc::O_DIRECTORY)
    }

    /// What is at `path`, or a new empty file of `mode` there, its parents made: a bind
    /// mount's target, as runc makes one for a file (createIfNotExists).
    pub fn file(&self, path: &[u8], mode: u32) -> io::Result<OwnedFd> {
        if let Ok(fd) = self.open_at(path, libc::O_PATH) {
            return Ok(fd);
        }
        let (parent, name) = split(path);
        let dir = self.mkdir_all(parent, 0o755, None)?;
        let c = cstr(name)?;
        // SAFETY: openat(2) relative to a descriptor of ours, made new, never followed.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode,
            )
        };
        check(fd).map_err(|e| path_error("open", path, &e))?;
        // SAFETY: a descriptor just opened, ours alone; closed at once.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
        self.open_at(path, libc::O_PATH)
    }

    /// The path of the root that `path` names, as BuildKit's fs.RootPath and runc's
    /// securejoin find it: each symlink followed in the root (an absolute one from it, a
    /// relative one from where it is, `..` never above it), and from the first name not
    /// there, the rest as it is written. A working directory or a mount point is made
    /// there. A file where a directory is needed ends the walk as a name not there
    /// (securejoin's IsNotExist), unless `strict`: then it is an error, as fs.RootPath
    /// lstats each name.
    pub fn resolve(&self, path: &[u8], strict: bool) -> io::Result<Vec<u8>> {
        // filepath-securejoin's maxSymlinkLimit.
        const MAX_LINKS: usize = 255;
        let mut todo: std::collections::VecDeque<Vec<u8>> = components(path).map(<[u8]>::to_vec).collect();
        let mut done: Vec<Vec<u8>> = Vec::new();
        let mut links = 0usize;
        while let Some(name) = todo.pop_front() {
            if name == b".." {
                done.pop();
                continue;
            }
            let dir = done.join(&b'/');
            let target = match self.open_at(&dir, libc::O_PATH | libc::O_DIRECTORY) {
                Ok(d) => readlinkat(&d, &name),
                Err(e) if strict && e.raw_os_error() == Some(libc::ENOTDIR) => {
                    let at = [dir.as_slice(), b"/", name.as_slice()].concat();
                    return Err(path_error("lstat", &at, &e));
                }
                Err(_) => None,
            };
            match target {
                Some(target) => {
                    links += 1;
                    if links > MAX_LINKS {
                        return Err(path_error(
                            "resolve",
                            path,
                            &io::Error::from_raw_os_error(libc::ELOOP),
                        ));
                    }
                    if target.starts_with(b"/") {
                        done.clear();
                    }
                    for c in components(&target).rev() {
                        todo.push_front(c.to_vec());
                    }
                }
                None => done.push(name),
            }
        }
        Ok([&b"/"[..], &done.join(&b'/')].concat())
    }

    /// The directory `dir` of the root, and the name of `path` in it, unresolved: for what
    /// acts on a name itself (unlinkat, mknodat, symlinkat).
    pub fn parent(&self, path: &[u8]) -> io::Result<(OwnedFd, CString)> {
        let (dir, name) = split(path);
        if name.is_empty() {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        Ok((self.open_at(dir, libc::O_PATH | libc::O_DIRECTORY)?, cstr(name)?))
    }
}

/// The names of `path`, less empty ones and `.`.
fn components(path: &[u8]) -> impl DoubleEndedIterator<Item = &[u8]> {
    path.split(|&b| b == b'/').filter(|c| !c.is_empty() && *c != b".")
}

/// What symlink `name` in directory `dir` points to, if it is one.
fn readlinkat(dir: &OwnedFd, name: &[u8]) -> Option<Vec<u8>> {
    let c = cstr(name).ok()?;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: readlinkat(2) into a buffer of ours, of the length given.
    let n = unsafe { libc::readlinkat(dir.as_raw_fd(), c.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    let n = usize::try_from(n).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// Whether `fd` is a directory.
pub fn is_dir(fd: &OwnedFd) -> io::Result<bool> {
    // SAFETY: a zeroed stat is valid for fstat to fill.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat(2) of a descriptor of ours into a stat of ours.
    check(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
    Ok(st.st_mode & libc::S_IFMT == libc::S_IFDIR)
}

/// A device node `name` in directory `dir`.
pub fn mknodat(dir: &OwnedFd, name: &str, mode: u32, major: u32, minor: u32) -> io::Result<()> {
    let c = cstr(name.as_bytes())?;
    // SAFETY: mknodat(2) relative to a descriptor of ours.
    check(unsafe { libc::mknodat(dir.as_raw_fd(), c.as_ptr(), mode, libc::makedev(major, minor)) })
        .map_err(|e| io::Error::new(e.kind(), format!("making /dev/{name}: {e}")))
}

/// A symlink `name` in directory `dir`, to `target`.
pub fn symlinkat(target: &str, dir: &OwnedFd, name: &str) -> io::Result<()> {
    let (t, n) = (cstr(target.as_bytes())?, cstr(name.as_bytes())?);
    // SAFETY: symlinkat(2) relative to a descriptor of ours.
    check(unsafe { libc::symlinkat(t.as_ptr(), dir.as_raw_fd(), n.as_ptr()) })
}

/// A directory `name` in directory `dir`.
pub fn mkdirat(dir: &OwnedFd, name: &str, mode: u32) -> io::Result<()> {
    let c = cstr(name.as_bytes())?;
    // SAFETY: mkdirat(2) relative to a descriptor of ours.
    check(unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), mode) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_split_into_their_directory_and_name() {
        assert_eq!(split(b"/etc/hosts"), (&b"etc"[..], &b"hosts"[..]));
        assert_eq!(split(b"hosts"), (&b""[..], &b"hosts"[..]));
        assert_eq!(split(b"/a/b/c/"), (&b"a/b"[..], &b"c"[..]));
        assert_eq!(split(b"/"), (&b""[..], &b""[..]));
        assert_eq!(relative(b"//a"), b"a");
    }
}
