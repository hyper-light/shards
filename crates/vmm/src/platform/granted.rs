//! Files this process was handed open rather than let open by path. A VM in App Sandbox
//! on macOS reaches what it reads only through descriptors its spawner opened, read-only
//! where it only reads (PM M70; shards `grant`). Every open of a VM's input goes through
//! [`open_input`], which uses the descriptor granted for its path if there is one, and
//! the path otherwise (Linux, where Landlock allows the paths themselves).
//!
//! A duplicated descriptor shares its file's offset with the one granted: each open
//! starts it at the beginning, and a VM opens each input once, as it sets up; reads and
//! writes after that are positioned (`read_at`, `write_at`, mappings).

use std::fs::File;
use std::io::{self, Seek as _};
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// A path granted: the descriptor opened for it, or `None` where its spawner found
/// nothing there.
struct Granted {
    /// The path as asked for.
    asked: PathBuf,
    /// The path of the file opened, as the OS names it (links resolved): a VM's snapshot
    /// records its files so, and its devices open them so (`vm::machine_config`).
    real: Option<PathBuf>,
    fd: Option<OwnedFd>,
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
pub fn grant_input(path: PathBuf, fd: Option<OwnedFd>) {
    // A descriptor whose path cannot be had is still reached by the path asked for.
    let real = fd.as_ref().and_then(|fd| opened_path(fd).ok());
    let mut granted = granted();
    granted.retain(|g| !g.is(&path) && real.as_deref().is_none_or(|real| !g.is(real)));
    granted.push(Granted {
        asked: path,
        real,
        fd,
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

/// `path`'s metadata: the granted descriptor's, or the file's at `path`.
pub fn input_metadata(path: &Path) -> io::Result<std::fs::Metadata> {
    match given(path, false)? {
        Some(file) => file.metadata(),
        None => std::fs::metadata(path),
    }
}

/// `path` resolved, links and all: the path of the file granted for it, as the OS named
/// that open file when it was granted, or else `path` canonicalized. A VM in App Sandbox
/// cannot look up the path it was granted a descriptor for.
pub fn input_path(path: &Path) -> io::Result<PathBuf> {
    match granted().iter().find(|g| g.is(path)) {
        Some(Granted { real: Some(real), .. }) => Ok(real.clone()),
        Some(Granted { fd: None, .. }) => Err(not_there()),
        _ => std::fs::canonicalize(path),
    }
}

fn not_there() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "not there when granted")
}

/// The path of the file `file` has open (fcntl(2) `F_GETPATH`).
#[cfg(target_os = "macos")]
fn opened_path(file: &OwnedFd) -> io::Result<PathBuf> {
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
#[cfg(not(target_os = "macos"))]
fn opened_path(file: &OwnedFd) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

fn given(path: &Path, write: bool) -> io::Result<Option<File>> {
    let granted = granted();
    let Some(Granted { fd, .. }) = granted.iter().find(|g| g.is(path)) else {
        return Ok(None);
    };
    let Some(fd) = fd else {
        return Err(not_there());
    };
    if write {
        // SAFETY: fcntl(2) F_GETFL on a descriptor this registry owns.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if flags & libc::O_ACCMODE != libc::O_RDWR {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "granted for reading alone",
            ));
        }
    }
    let mut file = File::from(fd.try_clone()?);
    file.rewind()?;
    Ok(Some(file))
}

/// Sockets handed this process bound at the paths they are for, not yet listening: a VM's
/// vsock device's, which App Sandbox will not let it bind (PM M67).
static LISTENERS: Mutex<Vec<(PathBuf, OwnedFd)>> = Mutex::new(Vec::new());

/// Takes `fd`, a socket bound at `path`, for the vsock device there to listen on.
pub fn grant_listener(path: PathBuf, fd: OwnedFd) {
    let mut listeners = LISTENERS.lock().unwrap_or_else(PoisonError::into_inner);
    listeners.retain(|(p, _)| *p != path);
    listeners.push((path, fd));
}

/// A socket listening at `path`: the one granted for it, taken and listened on now, or one
/// bound now. Until then, as where none is bound yet, a client is refused.
pub fn listen_unix(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    let mut listeners = LISTENERS.lock().unwrap_or_else(PoisonError::into_inner);
    match listeners.iter().position(|(p, _)| p == path) {
        Some(i) => {
            let socket = listeners.swap_remove(i).1;
            // SAFETY: listen(2) on a bound socket this process owns.
            if unsafe { libc::listen(socket.as_raw_fd(), libc::SOMAXCONN) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(std::os::unix::net::UnixListener::from(socket))
        }
        None => std::os::unix::net::UnixListener::bind(path),
    }
}

/// Dials Unix sockets for this process: a VM's guest connections to host ports, which App
/// Sandbox will not let it dial (PM M67), go through its spawner.
type Dialer = Box<dyn Fn(&Path) -> io::Result<std::os::unix::net::UnixStream> + Send + Sync>;

static DIALER: std::sync::OnceLock<Dialer> = std::sync::OnceLock::new();

/// Has `dialer` dial every Unix socket this process connects to from here on. Once.
pub fn set_dialer(dialer: Dialer) -> Result<(), String> {
    DIALER
        .set(dialer)
        .map_err(|_| "a dialer was set already".to_string())
}

/// A connection to the socket at `path` through the dialer set, if one is.
pub fn dial_unix(path: &Path) -> Option<io::Result<std::os::unix::net::UnixStream>> {
    DIALER.get().map(|dial| dial(path))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::{Read as _, Write as _};

    use super::*;

    /// A path granted a descriptor opens as that descriptor, from its start each time, and
    /// one granted read-only refuses writing; other paths open as files.
    #[test]
    fn granted_paths_open_as_their_descriptors() {
        let dir = std::env::temp_dir().join(format!("shards-granted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real");
        std::fs::write(&real, b"granted").unwrap();
        let named = dir.join("named-not-there");
        grant_input(named.clone(), Some(OwnedFd::from(File::open(&real).unwrap())));
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
        grant_input(
            dir.join("asked"),
            Some(OwnedFd::from(File::open(&resolved).unwrap())),
        );
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
        grant_input(
            writable.clone(),
            Some(OwnedFd::from(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&real)
                    .unwrap(),
            )),
        );
        open_input(&writable, true).unwrap().write_all(b"G").unwrap();
        assert_eq!(std::fs::read(&real).unwrap(), b"Granted");
        assert!(
            open_input(&dir.join("absent"), false).is_err(),
            "an ungranted path is the file's"
        );
        // One its spawner found nothing at is not found, though a file appears there since.
        let gone = dir.join("gone");
        grant_input(gone.clone(), None);
        std::fs::write(&gone, b"later").unwrap();
        assert_eq!(
            open_input(&gone, false).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
