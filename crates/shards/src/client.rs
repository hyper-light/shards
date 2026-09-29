//! The client side of `shards run` (docs/design/architecture.md D26). It asks the daemon
//! for the run, starting the daemon if there is none, and passes it this process's stdout
//! and stderr, which the command then writes to itself. The command's stdin is /dev/null,
//! or with `-i` a pipe the client fills from its own stdin: then the command's stdin ends
//! when the client does, as `docker run -i`'s does when its client goes (StdinOnce), and
//! only the client reads its terminal. The client forwards the signals it gets, as
//! `docker run` does, and exits with the command's status.
//!
//! It needs only `shards_ipc` and the OS, so it can move into a binary of its own.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use shards_ipc::{Run, kind};

/// `docker run`'s status when it could not run the command at all.
const NOT_RUN: u8 = 125;
/// How long a started daemon may take to listen.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// The daemon's socket for `home`: `daemon.sock` there, or, when that path is too long
/// for a Unix socket's address (`sun_path`: 104 bytes on macOS, 108 on Linux, NUL
/// included), `shards-HASH.sock` in this user's runtime directory, HASH naming the home.
pub fn socket(home: &Path) -> Result<PathBuf, String> {
    use sha2::{Digest as _, Sha256};
    // SAFETY: an all-zero sockaddr_un is valid; only its field's size is read.
    let room = unsafe { std::mem::zeroed::<libc::sockaddr_un>() }.sun_path.len() - 1;
    let inside = home.join("daemon.sock");
    if inside.as_os_str().len() <= room {
        return Ok(inside);
    }
    let hash: String = Sha256::digest(home.as_os_str().as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    let outside = runtime_dir()?.join(format!("shards-{hash}.sock"));
    if outside.as_os_str().len() > room {
        return Err(format!(
            "no socket path short enough for {}; set a shorter SHARDS_HOME",
            home.display()
        ));
    }
    Ok(outside)
}

/// This user's private directory for sockets: the per-user cache directory macOS makes
/// (confstr(3), `_CS_DARWIN_USER_CACHE_DIR`). The per-user temporary directory is cleaned
/// of files not accessed for 3 days, and connecting to a socket does not count as access
/// (docs/research/warm-pool-daemon.md §2.6). confstr falls back to shared directories when
/// the per-user one cannot be made, so this one must prove to be private.
#[cfg(target_vendor = "apple")]
fn runtime_dir() -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    let mut buf = vec![0u8; 1024];
    // SAFETY: confstr(3) writes at most buf.len() bytes, its NUL included.
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_CACHE_DIR, buf.as_mut_ptr().cast(), buf.len()) };
    if n == 0 || n > buf.len() {
        return Err(format!(
            "this user's cache directory: {}",
            io::Error::last_os_error()
        ));
    }
    buf.truncate(n - 1);
    let dir = PathBuf::from(std::ffi::OsString::from_vec(buf));
    private(&dir)?;
    Ok(dir)
}

/// This user's private directory for sockets: $XDG_RUNTIME_DIR, which the XDG Base
/// Directory Specification gives each user for sockets, owned by the user and mode 0700.
/// Without it, a directory in /tmp that must be this user's own and private, since anyone
/// may take a name in /tmp first.
#[cfg(not(target_vendor = "apple"))]
fn runtime_dir() -> Result<PathBuf, String> {
    use std::os::unix::fs::DirBuilderExt;
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
    {
        return Ok(dir);
    }
    // SAFETY: getuid(2) cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir = PathBuf::from(format!("/tmp/shards-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    }
    private(&dir)?;
    Ok(dir)
}

/// Whether `dir` is a directory of this user's that no one else may enter.
fn private(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid(2) cannot fail.
    let uid = unsafe { libc::getuid() };
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(format!("{} is not this user's private directory", dir.display()));
    }
    Ok(())
}

/// Where a daemon the client starts writes its messages.
pub fn log(home: &Path) -> PathBuf {
    home.join("daemon.log")
}

/// Runs `request` through the daemon of `home`, whose binary is `daemon`.
pub fn run(home: &Path, daemon: &Path, request: &Run) -> ExitCode {
    // SAFETY: isatty(3) on this process's stdin.
    let reads_terminal = request.interactive && unsafe { libc::isatty(0) } == 1;
    let current: Arc<Mutex<Option<UnixStream>>> = Arc::default();
    if let Err(e) = forward_signals(current.clone(), reads_terminal) {
        return failed(&e);
    }
    let stdin = match command_stdin(request.interactive) {
        Ok(stdin) => stdin,
        Err(e) => return failed(&e),
    };
    // A daemon from another build answers RESTART once it has stepped aside.
    for _ in 0..2 {
        let conn = match connect(home, daemon) {
            Ok(conn) => conn,
            Err(e) => return failed(&e),
        };
        let [out, err] = [1, 2].map(|fd| {
            // SAFETY: this process's standard descriptors stay open while it runs.
            unsafe { BorrowedFd::borrow_raw(fd) }
        });
        let stdio = [stdin.as_fd(), out, err];
        if let Err(e) = shards_ipc::send(&conn, kind::START, &request.encode(), &stdio) {
            return failed(&format!("asking the daemon: {e}"));
        }
        match conn.try_clone() {
            Ok(signals) => *current.lock().unwrap_or_else(PoisonError::into_inner) = Some(signals),
            Err(e) => return failed(&format!("the daemon's connection: {e}")),
        }
        loop {
            match shards_ipc::recv(&conn) {
                Ok(Some(m)) if m.kind == kind::EXIT => {
                    let Some((&status, timing)) = m.payload.split_first() else {
                        return failed("the command's microVM sent no status");
                    };
                    if !timing.is_empty() {
                        let _ = writeln!(
                            io::stderr(),
                            "shards-timing {}",
                            String::from_utf8_lossy(timing)
                        );
                    }
                    return ExitCode::from(status);
                }
                Ok(Some(m)) if m.kind == kind::RESTART => break,
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => {
                    return failed(&format!(
                        "the command's microVM stopped before the command ended; see {}",
                        log(home).display()
                    ));
                }
            }
        }
    }
    failed("the daemon kept asking for a restart")
}

/// How long `stop` waits for the daemon to hand over the runs in hand and exit.
const STOP_TIMEOUT: Duration = Duration::from_secs(60);

/// Asks the daemon of `home` to exit once the runs in hand are handed over, and waits
/// until it has: the daemon holds this connection open until it exits.
pub fn stop(home: &Path) -> ExitCode {
    let path = match socket(home) {
        Ok(path) => path,
        Err(e) => return failed(&e),
    };
    match UnixStream::connect(&path) {
        Ok(conn) => {
            if let Err(e) = shards_ipc::send(&conn, kind::STOP, &[], &[]) {
                return failed(&format!("asking the daemon to stop: {e}"));
            }
            if let Err(e) = conn.set_read_timeout(Some(STOP_TIMEOUT)) {
                return failed(&format!("the daemon's connection: {e}"));
            }
            match shards_ipc::recv(&conn) {
                Ok(None) => ExitCode::SUCCESS,
                Err(e) if !matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    ExitCode::SUCCESS
                }
                Ok(Some(_)) | Err(_) => failed(&format!("the daemon did not exit within {STOP_TIMEOUT:?}")),
            }
        }
        // No daemon: nothing to stop.
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            ExitCode::SUCCESS
        }
        Err(e) => failed(&format!("{}: {e}", path.display())),
    }
}

fn failed(message: &str) -> ExitCode {
    let _ = writeln!(io::stderr(), "shards: {message}");
    ExitCode::from(NOT_RUN)
}

/// The daemon's connection, starting the daemon if none listens.
fn connect(home: &Path, daemon: &Path) -> Result<UnixStream, String> {
    let path = socket(home)?;
    match UnixStream::connect(&path) {
        Ok(conn) => return Ok(conn),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    start(daemon)?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match UnixStream::connect(&path) {
            Ok(conn) => return Ok(conn),
            Err(e) if Instant::now() >= deadline => {
                return Err(format!(
                    "the daemon did not start ({e}); see {}",
                    log(home).display()
                ));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

/// Starts `daemon daemon --detached` in a session of its own; the daemon creates its home
/// and writes its messages to the log there. Of daemons started at once, one takes the
/// home's lock and the rest exit.
fn start(daemon: &Path) -> Result<(), String> {
    use std::os::fd::AsFd;
    let null = std::fs::File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
    shards_ipc::spawn(
        daemon,
        &["daemon".as_ref(), "--detached".as_ref()],
        &[(null.as_fd(), 0), (null.as_fd(), 1), (null.as_fd(), 2)],
        true,
    )
    .map(drop)
    .map_err(|e| format!("starting the daemon {}: {e}", daemon.display()))
}

/// The command's stdin: /dev/null, or with `interactive` a pipe a thread fills from this
/// process's stdin, closing it when that ends or this process exits.
fn command_stdin(interactive: bool) -> Result<OwnedFd, String> {
    if !interactive {
        return File::open("/dev/null")
            .map(OwnedFd::from)
            .map_err(|e| format!("/dev/null: {e}"));
    }
    let (reader, mut writer) = io::pipe().map_err(|e| format!("a pipe for stdin: {e}"))?;
    std::thread::Builder::new()
        .name("stdin".into())
        .spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            let mut stdin = io::stdin().lock();
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        if writer.write_all(buf.get(..n).unwrap_or_default()).is_err() {
                            return;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => return,
                }
            }
        })
        .map_err(|e| format!("stdin thread: {e}"))?;
    Ok(OwnedFd::from(reader))
}

/// Sends the signals `docker run` forwards to the command, over the current connection.
/// They are blocked in the calling thread, which every thread started later inherits, so
/// only the forwarder's `sigwait` receives them. One that would end the client, arriving
/// with no connection to send it on, ends the client as it would have. With
/// `reads_terminal`, the terminal's job control applies to the client (shards_ipc::forwarded).
fn forward_signals(current: Arc<Mutex<Option<UnixStream>>>, reads_terminal: bool) -> Result<(), String> {
    // SAFETY: sigset operations on a local set, and pthread_sigmask on this thread.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for (sig, _) in shards_ipc::forwarded(reads_terminal) {
            libc::sigaddset(&mut set, sig);
        }
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            return Err(format!("blocking signals: {}", io::Error::last_os_error()));
        }
        set
    };
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            loop {
                let mut sig = 0;
                // SAFETY: sigwait(3) on a valid set.
                if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
                    return;
                }
                let Some(&(_, linux)) = shards_ipc::FORWARDED.iter().find(|(s, _)| *s == sig) else {
                    continue;
                };
                let sent = current
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                    .is_some_and(|conn| {
                        shards_ipc::send(conn, kind::SIGNAL, &linux.to_be_bytes(), &[]).is_ok()
                    });
                let ends = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM].contains(&sig);
                if !sent && ends {
                    // SAFETY: the default action of a terminating signal, on this process.
                    unsafe {
                        libc::signal(sig, libc::SIG_DFL);
                        let mut only: libc::sigset_t = std::mem::zeroed();
                        libc::sigemptyset(&mut only);
                        libc::sigaddset(&mut only, sig);
                        libc::pthread_sigmask(libc::SIG_UNBLOCK, &only, std::ptr::null_mut());
                        libc::raise(sig);
                    }
                }
            }
        })
        .map_err(|e| format!("signal thread: {e}"))?;
    Ok(())
}
