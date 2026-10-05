//! Where `shards save` and `export` write, as docker/cli's runSave and runExport choose
//! (cli/command/image/save.go, container/export.go):
//! stdout, unless it is a terminal; or with `-o`, a file written whole or not at all, as
//! moby/sys/atomicwriter writes one: a temporary file beside it, synced, made 0600 and
//! renamed over it once the archive is whole. What is not whole goes, an interrupted save's
//! too, where atomicwriter renames what an interrupted copy wrote into place (measured:
//! Docker 29.3.1, SIGINT 1.5 s into saving rust:1.98.0, leaves 60,300,800 bytes of it).

use std::fs::{File, OpenOptions};
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
const ENDS: [libc::c_int; 4] = [libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM];

/// An OS error in Go's words: its strerror, lower-cased, as Go's tables copy it.
pub(crate) fn go(e: &std::io::Error) -> String {
    let text = e.to_string();
    let text = e
        .raw_os_error()
        .and_then(|n| text.strip_suffix(&format!(" (os error {n})")))
        .unwrap_or(&text)
        .to_string();
    let mut chars = text.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_lowercase().chain(chars).collect())
}

/// The output `-o` names, or stdout; or what the CLI says instead.
pub fn output(path: &str, failed: &str) -> Result<Output, String> {
    if path.is_empty() {
        // SAFETY: isatty(3) on this process's stdout.
        if unsafe { libc::isatty(1) } == 1 {
            return Err("cowardly refusing to save to a terminal. Use the -o flag or redirect".into());
        }
        return Ok(Output::Stdout(std::io::stdout()));
    }
    validate(path).map_err(|e| format!("{failed}: {e}"))?;
    let dest = std::path::absolute(path).map_err(|e| format!("{failed}: {}", go(&e)))?;
    let dir = dest.parent().unwrap_or(Path::new("/")).to_path_buf();
    let base = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
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

/// os.CreateTemp: the pattern and a random number, made exclusively, mode 0600; a name
/// taken is tried again with another.
fn create_temp(dir: &Path, base: &str) -> Result<(File, Temp), String> {
    for _ in 0..10_000 {
        let temp = dir.join(format!(".tmp-{base}{}", random()?));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
        {
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
fn unblock(set: &libc::sigset_t) {
    // SAFETY: pthread_sigmask(3) on this thread, with a valid set.
    unsafe { libc::pthread_sigmask(libc::SIG_UNBLOCK, set, std::ptr::null_mut()) };
}

/// A thread of its own takes the first of `set` sent, removes `temp`, and ends the process
/// by the signal's default action, as it would have ended.
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

/// A random u32, from the kernel.
fn random() -> Result<u32, String> {
    let mut b = [0u8; 4];
    #[cfg(target_os = "linux")]
    // SAFETY: getrandom(2) fills the 4 bytes given.
    let filled = unsafe { libc::getrandom(b.as_mut_ptr().cast(), b.len(), 0) } == 4;
    #[cfg(not(target_os = "linux"))]
    // SAFETY: getentropy(2) fills the 4 bytes given.
    let filled = unsafe { libc::getentropy(b.as_mut_ptr().cast(), b.len()) } == 0;
    if !filled {
        return Err(go(&std::io::Error::last_os_error()));
    }
    Ok(u32::from_le_bytes(b))
}

/// atomicwriter's validateDestination.
fn validate(name: &str) -> Result<(), String> {
    let path = Path::new(name);
    let dir = path.parent().map(Path::as_os_str).unwrap_or_default();
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
                    "invalid output path: stat {}: {}",
                    dir.to_string_lossy(),
                    go(&e)
                ));
            }
        }
    }
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("failed to stat output path: lstat {name}: {}", go(&e))),
    };
    let t = meta.file_type();
    let mode = meta.permissions().mode();
    Err(if t.is_file() && mode & 0o7000 == 0 {
        return Ok(());
    } else if t.is_dir() {
        "cannot write to a directory".into()
    } else if t.is_symlink() {
        "cannot write to a symbolic link directly".into()
    } else if t.is_fifo() {
        "cannot write to a named pipe (FIFO)".into()
    } else if t.is_socket() {
        "cannot write to a socket".into()
    } else if t.is_char_device() {
        "cannot write to a character device file".into()
    } else if t.is_block_device() {
        "cannot write to a block device file".into()
    } else if mode & 0o4000 != 0 {
        "cannot write to a setuid file".into()
    } else if mode & 0o2000 != 0 {
        "cannot write to a setgid file".into()
    } else {
        "cannot write to a sticky bit file".into()
    })
}

impl Output {
    /// The descriptor the daemon writes the archive to.
    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd as _;
        match self {
            Output::Stdout(out) => out.as_fd(),
            Output::File { file, .. } => file.as_fd(),
        }
    }

    /// atomicwriter's Close: the file is synced and made 0600, then renamed over its
    /// destination once `status` says the archive is whole and some of it was written;
    /// else it goes. What failed, in Go's words.
    pub fn finish(self, status: u8) -> Result<(), String> {
        let Output::File { file, temp, dest } = self else {
            return Ok(());
        };
        let written = file.metadata().map(|m| m.len() > 0).unwrap_or(false);
        let done = (|| -> std::io::Result<()> {
            shards_ipc::sync_durable(&file)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
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
        // SAFETY: isatty(3) on this process's stdin.
        if unsafe { libc::isatty(0) } == 1 {
            return Err("requested load from stdin, but stdin is empty".into());
        }
        return Ok(None);
    }
    File::open(path)
        .map(Some)
        .map_err(|e| format!("open {path}: {}", go(&e)))
}
