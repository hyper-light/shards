//! Files this process was handed open rather than let open by path. A VM in App Sandbox
//! on macOS reaches what it reads only through descriptors its spawner opened, read-only
//! where it only reads (PM M70; shards `grant`); one in an AppContainer on Windows, through
//! handles its spawner opened and duplicated into it (D141, PM M201). Every open of a VM's
//! input goes through [`open_input`], which uses the file granted for its path if there is
//! one, and the path otherwise (Linux, where Landlock allows the paths themselves).
//!
//! A duplicated descriptor shares its file's offset with the one granted: each open
//! starts it at the beginning, and a VM opens each input once, as it sets up; reads and
//! writes after that are positioned (`read_at`, `write_at`, mappings).

use std::fs::File;
use std::io::{self, Seek as _};
#[cfg(unix)]
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// A path granted: the file opened for it, or `None` where its spawner found nothing
/// there.
struct Granted {
    /// The path as asked for.
    asked: PathBuf,
    /// The path of the file opened, as the OS names it (links resolved): a VM's snapshot
    /// records its files so, and its devices open them so (`vm::machine_config`).
    real: Option<PathBuf>,
    file: Option<File>,
    /// Whether it was opened for writing as well as reading.
    writable: bool,
}

impl Granted {
    fn is(&self, path: &Path) -> bool {
        self.asked == path || self.real.as_deref() == Some(path)
    }
}

/// The paths granted, each with its descriptor: one process's, as its VM is.
static GRANTED: Mutex<Vec<Granted>> = Mutex::new(Vec::new());

fn granted() -> MutexGuard<'static, Vec<Granted>> {
    GRANTED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Takes `fd` as `path`'s: from here on, what opens of `path` get, and with `None`,
/// "not found".
#[cfg(unix)]
pub fn grant_input(path: PathBuf, fd: Option<OwnedFd>) {
    let writable = fd.as_ref().is_some_and(|fd| {
        // SAFETY: fcntl(2) F_GETFL on a descriptor given us.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        flags >= 0 && flags & libc::O_ACCMODE == libc::O_RDWR
    });
    let file = fd.map(File::from);
    // A descriptor whose path cannot be had is still reached by the path asked for.
    let real = file.as_ref().and_then(|f| opened_path(f).ok());
    grant(path, real, file, writable);
}

/// Takes `file` as `path`'s, opened for writing too where `writable`, with the path its
/// spawner found it at, links resolved: from here on, what opens of `path` or that path
/// get, and with `None`, "not found". A handle does not say what it was opened for, and an
/// AppContainer may not ask where it is (PM M203), so its spawner says both.
#[cfg(windows)]
pub fn grant_input(path: PathBuf, file: Option<(File, PathBuf)>, writable: bool) {
    let (file, real) = file.unzip();
    grant(path, real, file, writable);
}

fn grant(path: PathBuf, real: Option<PathBuf>, file: Option<File>, writable: bool) {
    let mut granted = granted();
    granted.retain(|g| !g.is(&path) && real.as_deref().is_none_or(|real| !g.is(real)));
    granted.push(Granted {
        asked: path,
        real,
        file,
        writable,
    });
}

/// `path` for reading, and with `write` for writing too: the descriptor granted for it,
/// duplicated and at its start, or else the file at `path`. A descriptor granted
/// read-only is refused for writing.
pub fn open_input(path: &Path, write: bool) -> io::Result<File> {
    match given(path, write)? {
        Some(file) => Ok(file),
        None => std::fs::OpenOptions::new().read(true).write(write).open(path),
    }
}

/// The regular file `rel` names beneath `root`, for reading, as a snapshot's files are
/// read (PM M165): the descriptor granted for its path, its spawner having opened it so
/// (shards `grant_answer`), or else [`super::open_beneath`]'s.
pub fn open_beneath_input(root: &Path, rel: &[&str]) -> io::Result<File> {
    let path = rel.iter().fold(root.to_path_buf(), |p, name| p.join(name));
    match given(&path, false)? {
        Some(file) => {
            if !file.metadata()?.file_type().is_file() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "not a regular file"));
            }
            Ok(file)
        }
        None => super::open_beneath(root, rel),
    }
}

/// The regular file `rel` names beneath `root`, its newest version, for reading, as a
/// snapshot's pointer and working set are read (platform `write_in`): the descriptor granted
/// for its path, its spawner having opened the newest so (shards `grant_answer`), or else
/// [`super::open_newest_beneath`]'s.
pub fn open_newest_beneath_input(root: &Path, rel: &[&str]) -> io::Result<File> {
    let path = rel.iter().fold(root.to_path_buf(), |p, name| p.join(name));
    match given(&path, false)? {
        Some(file) => {
            if !file.metadata()?.file_type().is_file() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "not a regular file"));
            }
            Ok(file)
        }
        None => super::open_newest_beneath(root, rel),
    }
}

/// `path`'s metadata: the granted descriptor's, or the file's at `path`.
pub fn input_metadata(path: &Path) -> io::Result<std::fs::Metadata> {
    match given(path, false)? {
        Some(file) => file.metadata(),
        None => std::fs::metadata(path),
    }
}

/// Which file `path` is (windows.rs `file_id`): the granted handle's, or the file's at
/// `path` opened for no access, as [`input_metadata`] asks of one not granted.
#[cfg(windows)]
pub fn input_file_id(path: &Path) -> io::Result<(u64, u64)> {
    match given(path, false)? {
        Some(file) => super::file_id(&file),
        None => super::file_id(&super::open_for_metadata(path)?),
    }
}

/// `path` resolved, links and all: the path of the file granted for it, as the OS named
/// that open file when it was granted, or else `path` canonicalized. A VM in App Sandbox
/// cannot look up the path it was granted a descriptor for.
pub fn input_path(path: &Path) -> io::Result<PathBuf> {
    match granted().iter().find(|g| g.is(path)) {
        Some(Granted { real: Some(real), .. }) => Ok(real.clone()),
        Some(Granted { file: None, .. }) => Err(not_there()),
        _ => std::fs::canonicalize(path),
    }
}

fn not_there() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "not there when granted")
}

/// The path of the file `file` has open (fcntl(2) `F_GETPATH`).
#[cfg(target_os = "macos")]
fn opened_path(file: &File) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;
    // F_GETPATH fills a buffer of MAXPATHLEN (sys/param.h: PATH_MAX) bytes.
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: fcntl(2) F_GETPATH on an open descriptor, into a buffer of the size it fills.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    buf.truncate(len);
    Ok(PathBuf::from(std::ffi::OsString::from_vec(buf)))
}

/// The path of the file `file` has open, as proc(5) names it.
#[cfg(target_os = "linux")]
fn opened_path(file: &File) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn given(path: &Path, write: bool) -> io::Result<Option<File>> {
    let granted = granted();
    let Some(Granted { file, writable, .. }) = granted.iter().find(|g| g.is(path)) else {
        return Ok(None);
    };
    let Some(file) = file else {
        return Err(not_there());
    };
    if write && !writable {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "granted for reading alone",
        ));
    }
    let mut file = file.try_clone()?;
    file.rewind()?;
    Ok(Some(file))
}

/// A socket handed this process bound at the path it is for, not yet listening: a
/// descriptor on Unix, a socket shared with it on Windows (PM M203).
#[cfg(unix)]
type Bound = OwnedFd;
#[cfg(windows)]
type Bound = super::UnixListener;

/// Sockets handed this process bound at the paths they are for, not yet listening: a VM's
/// vsock device's, which App Sandbox will not let it bind (PM M67), nor an AppContainer
/// in a directory that does not grant it (PM M201).
static LISTENERS: Mutex<Vec<(PathBuf, Bound)>> = Mutex::new(Vec::new());

/// Takes `socket`, bound at `path`, for the vsock device there to listen on.
pub fn grant_listener(path: PathBuf, socket: Bound) {
    let mut listeners = LISTENERS.lock().unwrap_or_else(PoisonError::into_inner);
    listeners.retain(|(p, _)| *p != path);
    listeners.push((path, socket));
}

/// A socket listening at `path`: the one granted for it, taken and listened on now, or one
/// bound now. Until then, as where none is bound yet, a client is refused.
pub fn listen_unix(path: &Path) -> io::Result<super::UnixListener> {
    let mut listeners = LISTENERS.lock().unwrap_or_else(PoisonError::into_inner);
    match listeners.iter().position(|(p, _)| p == path) {
        Some(i) => listen(listeners.swap_remove(i).1),
        None => super::UnixListener::bind(path),
    }
}

#[cfg(unix)]
fn listen(socket: OwnedFd) -> io::Result<super::UnixListener> {
    // SAFETY: listen(2) on a bound socket this process owns.
    if unsafe { libc::listen(socket.as_raw_fd(), libc::SOMAXCONN) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(super::UnixListener::from(socket))
}

#[cfg(windows)]
fn listen(socket: super::UnixListener) -> io::Result<super::UnixListener> {
    socket.listen()?;
    Ok(socket)
}

/// Dials Unix sockets for this process: a VM's guest connections to host ports, which App
/// Sandbox will not let it dial (PM M67), nor an AppContainer (PM M201), go through its
/// spawner.
type Dialer = Box<dyn Fn(&Path) -> io::Result<super::UnixStream> + Send + Sync>;

static DIALER: std::sync::OnceLock<Dialer> = std::sync::OnceLock::new();

/// Has `dialer` dial every Unix socket this process connects to from here on. Once.
pub fn set_dialer(dialer: Dialer) -> Result<(), String> {
    DIALER
        .set(dialer)
        .map_err(|_| "a dialer was set already".to_string())
}

/// A connection to the socket at `path` through the dialer set, if one is.
pub fn dial_unix(path: &Path) -> Option<io::Result<super::UnixStream>> {
    DIALER.get().map(|dial| dial(path))
}

/// What a generation keeps beside its machine's memory and state, which its spawner stages
/// with them: its writable layer's head and file system (D190), its DAX windows' heads
/// (D189).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Kept {
    pub layer: bool,
    pub window_heads: bool,
}

impl Kept {
    /// As a `STAGE` carries it: a byte, 1 its layer, 2 its windows' heads.
    pub fn bits(self) -> u8 {
        u8::from(self.layer) | (u8::from(self.window_heads) << 1)
    }

    /// What a `STAGE`'s byte says, if it says nothing else.
    pub fn from_bits(bits: u8) -> Option<Kept> {
        (bits & !3 == 0).then_some(Kept {
            layer: bits & 1 != 0,
            window_heads: bits & 2 != 0,
        })
    }

    /// How many files a generation of these is staged with: its memory and state, its
    /// layer's head and file system, its windows' heads.
    pub fn files(self) -> usize {
        2 + 2 * usize::from(self.layer) + usize::from(self.window_heads)
    }
}

/// What stages this process's snapshots where it writes in no directory of its own, an
/// AppContainer's on Windows (D141): its spawner, which makes each generation in the
/// directory it granted, hands this process the generation's files to fill, and makes it
/// durable and the one in use on this process's word (D196).
pub trait Stager: Send + Sync + std::fmt::Debug {
    /// A new generation in `dir`: its name, then its files, empty and open for writing: its
    /// memory and state, then what it keeps beside them (`kept`), in [`Kept::files`]'s
    /// order.
    fn stage(&self, dir: &Path, kept: Kept) -> io::Result<(String, Vec<File>)>;
    /// Makes generation `name` of `dir`, filled, durable and the one in use.
    fn commit(&self, dir: &Path, name: &str) -> io::Result<()>;
}

static STAGER: std::sync::OnceLock<Box<dyn Stager>> = std::sync::OnceLock::new();

/// Has `stager` stage every snapshot this process writes from here on. Once.
pub fn set_stager(stager: Box<dyn Stager>) -> Result<(), String> {
    STAGER
        .set(stager)
        .map_err(|_| "a stager was set already".to_string())
}

/// The stager set ([`set_stager`]), if one is.
pub fn stager() -> Option<&'static dyn Stager> {
    STAGER.get().map(|s| &**s)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::{Read as _, Write as _};

    use super::*;

    /// What a generation keeps goes in a `STAGE`'s byte and comes back as it was, with its
    /// files counted; a byte that says anything else is refused.
    #[test]
    fn a_stages_byte_says_what_its_generation_keeps() {
        for layer in [false, true] {
            for window_heads in [false, true] {
                let kept = Kept { layer, window_heads };
                assert_eq!(Kept::from_bits(kept.bits()), Some(kept));
                assert_eq!(
                    kept.files(),
                    2 + 2 * usize::from(layer) + usize::from(window_heads)
                );
            }
        }
        assert_eq!(Kept::from_bits(4), None);
        assert_eq!(Kept::from_bits(0xff), None);
    }

    /// `file` granted as `path`'s, as each host grants one: a descriptor that says what it
    /// was opened for, or a handle whose spawner says it (`writable`).
    fn give(path: PathBuf, file: Option<File>, writable: bool) {
        #[cfg(unix)]
        {
            let _ = writable;
            grant_input(path, file.map(OwnedFd::from));
        }
        #[cfg(windows)]
        {
            let real = file.as_ref().map(|f| super::super::final_path(f).unwrap());
            grant_input(path, file.zip(real), writable);
        }
    }

    /// A path granted a descriptor opens as that descriptor, from its start each time, and
    /// one granted read-only refuses writing; other paths open as files.
    #[test]
    fn granted_paths_open_as_their_descriptors() {
        let dir = shards_testdir::TempDir::new("granted").unwrap();
        let real = dir.join("real");
        std::fs::write(&real, b"granted").unwrap();
        let named = dir.join("named-not-there");
        give(named.clone(), Some(File::open(&real).unwrap()), false);
        for _ in 0..2 {
            let mut got = String::new();
            open_input(&named, false)
                .unwrap()
                .read_to_string(&mut got)
                .unwrap();
            assert_eq!(got, "granted");
        }
        assert_eq!(input_metadata(&named).unwrap().len(), 7);
        // A granted path resolves to the file its descriptor has open, which need not be
        // there by that name; others resolve where they are.
        let canonical = std::fs::canonicalize(&real).unwrap();
        assert_eq!(input_path(&named).unwrap(), canonical);
        assert_eq!(input_path(&real).unwrap(), canonical);
        // And opens by that path reach the descriptor, as a machine's devices open their
        // files by the paths resolved: gone from the directory, the file is still there.
        let resolved = dir.join("resolved");
        std::fs::write(&resolved, b"by its real path").unwrap();
        let resolved_canonical = std::fs::canonicalize(&resolved).unwrap();
        give(dir.join("asked"), Some(File::open(&resolved).unwrap()), false);
        std::fs::remove_file(&resolved).unwrap();
        let mut got = String::new();
        open_input(&resolved_canonical, false)
            .unwrap()
            .read_to_string(&mut got)
            .unwrap();
        assert_eq!(got, "by its real path");
        assert_eq!(
            open_input(&named, true).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let writable = dir.join("writable");
        give(
            writable.clone(),
            Some(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&real)
                    .unwrap(),
            ),
            true,
        );
        open_input(&writable, true).unwrap().write_all(b"G").unwrap();
        assert_eq!(std::fs::read(&real).unwrap(), b"Granted");
        assert!(
            open_input(&dir.join("absent"), false).is_err(),
            "an ungranted path is the file's"
        );
        // One its spawner found nothing at is not found, though a file appears there since.
        let gone = dir.join("gone");
        give(gone.clone(), None, false);
        std::fs::write(&gone, b"later").unwrap();
        assert_eq!(
            open_input(&gone, false).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
