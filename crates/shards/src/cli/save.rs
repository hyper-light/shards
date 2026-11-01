//! Where `shards save` and `export` write, as docker/cli's runSave and runExport choose
//! (cli/command/image/save.go, container/export.go):
//! stdout, unless it is a terminal; or with `-o`, a file written whole or not at all, as
//! moby/sys/atomicwriter writes one: a temporary file beside it, synced, made 0600 and
//! renamed over it once the archive is whole. What is not whole goes, an interrupted save's
//! too, where atomicwriter renames what an interrupted copy wrote into place (measured:
//! Docker 29.3.1, SIGINT 1.5 s into saving rust:1.98.0, leaves 60,300,800 bytes of it).
//! The file is a context's client's too, on every system (D180): what a command there
//! writes, written here.

use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// Where the archive goes.
pub enum Output {
    Stdout(std::io::Stdout),
    File { file: File, temp: Temp, dest: PathBuf },
}

/// The temporary file the archive is written to, removed when this is dropped: once it is
/// renamed into place, there is nothing there to remove.
pub struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The signals that end the client by default, after which a save leaves nothing behind.
#[cfg(unix)]
const ENDS: [libc::c_int; 4] = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];

/// An OS error in Go's words: its strerror, lower-cased, as Go's tables copy it; on
/// Windows, the system's message as it is, as Go's Errno.Error gives it (FormatMessage).
pub(crate) fn go(e: &std::io::Error) -> String {
    let text = e.to_string();
    let text = e
        .raw_os_error()
        .and_then(|n| text.strip_suffix(&format!(" (os error {n})")))
        .unwrap_or(&text)
        .to_string();
    if cfg!(windows) {
        return text;
    }
    let mut chars = text.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_lowercase().chain(chars).collect())
}

/// The output `-o` names, or stdout; or what the CLI says instead.
#[cfg(unix)]
pub fn output(path: &str, failed: &str) -> Result<Output, String> {
    if path.is_empty() {
        // SAFETY: isatty(3) on this process's stdout.
        if unsafe { libc::isatty(1) } == 1 {
            return Err("cowardly refusing to save to a terminal. Use the -o flag or redirect".into());
        }
        return Ok(Output::Stdout(std::io::stdout()));
    }
    let (dir, base, dest) = destination(path, failed)?;
    // The signals that end the client are blocked before the file is made, for a thread
    // to take once it is: one sent in between stays pending, so nothing is left behind.
    let ends = block_ends().map_err(|e| format!("{failed}: {e}"))?;
    let (file, temp) = match create_temp(&dir, &base) {
        Ok(made) => made,
        Err(e) => {
            unblock(&ends);
            return Err(format!("{failed}: {e}"));
        }
    };
    if let Err(e) = remove_when_ended(ends, temp.0.clone()) {
        drop(temp);
        unblock(&ends);
        return Err(format!("{failed}: {e}"));
    }
    Ok(Output::File { file, temp, dest })
}

/// The output `-o` names, or stdout; or what the CLI says instead. On Windows the console's
/// events that end the client (Ctrl-C, Ctrl-Break, its close, logoff, shutdown) remove the
/// temporary file first, as Unix's signals do, then end the client as they would have
/// ([`remove_when_ended`]).
#[cfg(windows)]
pub fn output(path: &str, failed: &str) -> Result<Output, String> {
    use std::io::IsTerminal as _;
    if path.is_empty() {
        if std::io::stdout().is_terminal() {
            return Err("cowardly refusing to save to a terminal. Use the -o flag or redirect".into());
        }
        return Ok(Output::Stdout(std::io::stdout()));
    }
    let (dir, base, dest) = destination(path, failed)?;
    let (file, temp) = create_temp(&dir, &base).map_err(|e| format!("{failed}: {e}"))?;
    remove_when_ended(temp.0.clone()).map_err(|e| format!("{failed}: {}", go(&e)))?;
    Ok(Output::File { file, temp, dest })
}

/// The temporary file a console event that ends the client removes first
/// ([`remove_when_ended`]).
#[cfg(windows)]
static ENDED_TEMP: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Has each console event that ends the client remove `temp` first: the handler removes it,
/// then leaves the event to the next, Windows' own, which ends the process as it would have
/// ended (ExitProcess with STATUS_CONTROL_C_EXIT).
#[cfg(windows)]
fn remove_when_ended(temp: PathBuf) -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
        SetConsoleCtrlHandler,
    };
    unsafe extern "system" fn on_event(event: u32) -> windows_sys::core::BOOL {
        if matches!(
            event,
            CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
        ) && let Some(temp) = ENDED_TEMP
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = std::fs::remove_file(temp);
        }
        0
    }
    *ENDED_TEMP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(temp);
    // SAFETY: a handler this process keeps for its life.
    if unsafe { SetConsoleCtrlHandler(Some(on_event), 1) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The file `-o` names, written as [`output`] writes it, the signals that end this process
/// its caller's: a context's client sends them on to the command, whose end, whole or not,
/// is then [`Output::finish`]'s to keep or let go.
pub fn to_file(path: &str, failed: &str) -> Result<Output, String> {
    let (dir, base, dest) = destination(path, failed)?;
    let (file, temp) = create_temp(&dir, &base).map_err(|e| format!("{failed}: {e}"))?;
    Ok(Output::File { file, temp, dest })
}

/// The file `path` names, checked as atomicwriter checks it: its directory, its name and
/// where it is.
fn destination(path: &str, failed: &str) -> Result<(PathBuf, String, PathBuf), String> {
    validate(path).map_err(|e| format!("{failed}: {e}"))?;
    let dest = std::path::absolute(path).map_err(|e| format!("{failed}: {}", go(&e)))?;
    let dir = dest.parent().unwrap_or(Path::new("/")).to_path_buf();
    let base = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok((dir, base, dest))
}

/// os.CreateTemp: the pattern and a random number, made exclusively, mode 0600 where a file
/// has one; a name taken is tried again with another.
fn create_temp(dir: &Path, base: &str) -> Result<(File, Temp), String> {
    for _ in 0..10_000 {
        let temp = dir.join(format!(".tmp-{base}{}", random()?));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        match options.open(&temp) {
            Ok(file) => return Ok((file, Temp(temp))),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(format!("open {}: {}", temp.display(), go(&e)));
            }
        }
    }
    Err("no temporary file could be made".into())
}

/// Blocks the signals of `ENDS` this process was not started ignoring, in this thread,
/// which threads started later inherit. Returns them.
#[cfg(unix)]
fn block_ends() -> Result<libc::sigset_t, String> {
    // SAFETY: sigaction(2) reads of dispositions and sigset operations on local
    // structures, then pthread_sigmask(3) on this thread.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in ENDS {
            let mut was: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, std::ptr::null(), &mut was) == 0 && was.sa_sigaction != libc::SIG_IGN {
                libc::sigaddset(&mut set, sig);
            }
        }
        match libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) {
            0 => Ok(set),
            n => Err(go(&std::io::Error::from_raw_os_error(n))),
        }
    }
}

/// Unblocks `set` in this thread, where nothing will wait for it.
#[cfg(unix)]
fn unblock(set: &libc::sigset_t) {
    // SAFETY: pthread_sigmask(3) on this thread, with a valid set.
    unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, set, std::ptr::null_mut()) };
}

/// A thread of its own takes the first of `set` sent, removes `temp`, and ends the process
/// by the signal's default action, as it would have ended.
#[cfg(unix)]
fn remove_when_ended(set: libc::sigset_t, temp: PathBuf) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("save-signals".into())
        .spawn(move || {
            let mut sig = 0;
            // SAFETY: sigwait(3) on a valid set.
            if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
                return;
            }
            let _ = std::fs::remove_file(&temp);
            // SAFETY: the default action of a terminating signal, on this process.
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
                let mut only: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut only);
                libc::sigaddset(&mut only, sig);
                libc::pthread_sigmask(libc::SIG_UNBLOCK, &only, std::ptr::null_mut());
                libc::raise(sig);
            }
        })
        .map(drop)
}

/// A random u32, from the system's source.
fn random() -> Result<u32, String> {
    let mut b = [0u8; 4];
    shards_net::entropy(&mut b).map_err(|e| go(&e))?;
    Ok(u32::from_le_bytes(b))
}

/// atomicwriter's validateDestination: its directory as Go's filepath.Dir has it, and each
/// error as os.Stat and os.Lstat give it on this platform ([`stat_op`]).
fn validate(name: &str) -> Result<(), String> {
    let path = Path::new(name);
    let dir = shards_archive::copy::dir(path);
    let dir = dir.as_os_str();
    if !dir.is_empty() && dir != "." && dir != ".." {
        match std::fs::metadata(dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "invalid output path: stat {}: not a directory",
                    dir.to_string_lossy()
                ));
            }
            Err(e) => {
                return Err(format!(
                    "invalid output path: {} {}: {}",
                    stat_op(&e, "stat"),
                    dir.to_string_lossy(),
                    go(&e)
                ));
            }
        }
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if not_there(&e) => return Ok(()),
        Err(e) => {
            return Err(format!(
                "failed to stat output path: {} {name}: {}",
                stat_op(&e, "lstat"),
                go(&e)
            ));
        }
    };
    unwritable(&meta).map_or(Ok(()), |why| Err(why.to_string()))
}

/// Whether `e` is os.ErrNotExist's: on Windows ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND
/// or ERROR_BAD_NETPATH (syscall_windows.go, `Errno.Is`), where std's NotFound takes more.
fn not_there(e: &std::io::Error) -> bool {
    if cfg!(windows) {
        matches!(e.raw_os_error(), Some(2 | 3 | 53))
    } else {
        e.kind() == std::io::ErrorKind::NotFound
    }
}

/// The call Go's os.Stat and os.Lstat name in an error of theirs: `unix`'s off Windows; on
/// Windows GetFileAttributesEx's for a path not there, else CreateFile's, which they try
/// next (os/stat_windows.go, `stat`).
fn stat_op(e: &std::io::Error, unix: &'static str) -> &'static str {
    if !cfg!(windows) {
        unix
    } else if not_there(e) {
        "GetFileAttributesEx"
    } else {
        "CreateFile"
    }
}

/// What atomicwriter refuses of an existing destination, by its type and mode.
#[cfg(unix)]
fn unwritable(meta: &std::fs::Metadata) -> Option<&'static str> {
    let t = meta.file_type();
    let mode = meta.permissions().mode();
    Some(if t.is_file() && mode & 0o7000 == 0 {
        return None;
    } else if t.is_dir() {
        "cannot write to a directory"
    } else if t.is_symlink() {
        "cannot write to a symbolic link directly"
    } else if t.is_fifo() {
        "cannot write to a named pipe (FIFO)"
    } else if t.is_socket() {
        "cannot write to a socket"
    } else if t.is_char_device() {
        "cannot write to a character device file"
    } else if t.is_block_device() {
        "cannot write to a block device file"
    } else if mode & 0o4000 != 0 {
        "cannot write to a setuid file"
    } else if mode & 0o2000 != 0 {
        "cannot write to a setgid file"
    } else {
        "cannot write to a sticky bit file"
    })
}

/// What atomicwriter refuses of an existing destination: of Windows', a directory and a
/// symbolic link, which Go's os.FileMode alone tells apart there.
#[cfg(windows)]
fn unwritable(meta: &std::fs::Metadata) -> Option<&'static str> {
    let t = meta.file_type();
    if t.is_dir() {
        Some("cannot write to a directory")
    } else if t.is_symlink() {
        Some("cannot write to a symbolic link directly")
    } else {
        None
    }
}

impl Output {
    /// The file the archive is written to, until it is whole; none for stdout.
    pub fn handle(&self) -> Option<&File> {
        match self {
            Output::File { file, .. } => Some(file),
            Output::Stdout(_) => None,
        }
    }

    /// Where the file is until it is whole, which a client that ends before then removes.
    pub fn temp(&self) -> Option<&Path> {
        match self {
            Output::File { temp, .. } => Some(&temp.0),
            Output::Stdout(_) => None,
        }
    }

    /// The descriptor the daemon writes the archive to.
    pub fn fd(&self) -> shards_ipc::fd::BorrowedFd<'_> {
        use shards_ipc::fd::AsFd as _;
        match self {
            Output::Stdout(out) => out.as_fd(),
            Output::File { file, .. } => file.as_fd(),
        }
    }

    /// atomicwriter's Close: the file is synced and made 0600, then renamed over its
    /// destination once `status` says the archive is whole and some of it was written;
    /// else it goes. What failed, in Go's words.
    pub fn finish(self, status: u8) -> Result<(), String> {
        let (file, temp, dest) = match self {
            Output::File { file, temp, dest } => (file, temp, dest),
            Output::Stdout(_) => return Ok(()),
        };
        let written = file.metadata().map(|m| m.len() > 0).unwrap_or(false);
        let done = (|| -> std::io::Result<()> {
            #[cfg(unix)]
            {
                shards_ipc::sync_durable(&file)?;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            #[cfg(windows)]
            file.sync_all()?;
            drop(file);
            if status == 0 && written {
                std::fs::rename(&temp.0, &dest)?;
            }
            Ok(())
        })();
        drop(temp);
        done.map_err(|e| go(&e))
    }
}

/// Where `shards load` reads, as docker/cli's runLoad opens it: `-i`'s file, or stdin,
/// unless it is a terminal. What the CLI says instead, in Go's words.
pub fn input(path: &str) -> Result<Option<File>, String> {
    if path.is_empty() {
        if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return Err("requested load from stdin, but stdin is empty".into());
        }
        return Ok(None);
    }
    open(path).map(Some)
}

/// The file `path`, opened to read, as docker/cli opens `load -i`'s and `import`'s; what
/// failed, in Go's words.
pub fn open(path: &str) -> Result<File, String> {
    File::open(path).map_err(|e| format!("open {path}: {}", go(&e)))
}
