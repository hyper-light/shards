//! Where `shards save` writes, as docker/cli's runSave chooses (cli/command/image/save.go):
//! stdout, unless it is a terminal; or with `-o`, a file written whole or not at all, as
//! moby/sys/atomicwriter writes one: a temporary file beside it, mode 0600, synced and
//! renamed over it once the archive is whole.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileTypeExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

/// Where the archive goes.
pub enum Output {
    Stdout(std::io::Stdout),
    File {
        file: File,
        temp: PathBuf,
        dest: PathBuf,
    },
}

/// An OS error in Go's words: its strerror, lower-cased, as Go's tables copy it.
fn go(e: &std::io::Error) -> String {
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
pub fn output(path: &str) -> Result<Output, String> {
    if path.is_empty() {
        // SAFETY: isatty(3) on this process's stdout.
        if unsafe { libc::isatty(1) } == 1 {
            return Err("cowardly refusing to save to a terminal. Use the -o flag or redirect".into());
        }
        return Ok(Output::Stdout(std::io::stdout()));
    }
    validate(path).map_err(|e| format!("failed to save image: {e}"))?;
    let dest = std::path::absolute(path).map_err(|e| format!("failed to save image: {}", go(&e)))?;
    let dir = dest.parent().unwrap_or(Path::new("/")).to_path_buf();
    let base = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // os.CreateTemp: the pattern and a random number, made exclusively, mode 0600; a
    // name taken is tried again with another.
    for _ in 0..10_000 {
        let temp = dir.join(format!(".tmp-{base}{}", random()?));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
        {
            Ok(file) => return Ok(Output::File { file, temp, dest }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(format!(
                    "failed to save image: open {}: {}",
                    temp.display(),
                    go(&e)
                ));
            }
        }
    }
    Err("failed to save image: no temporary file could be made".into())
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
        return Err(format!(
            "failed to save image: {}",
            go(&std::io::Error::last_os_error())
        ));
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

    /// atomicwriter's Close: once `status` says the archive is whole and some of it was
    /// written, the file, made 0600, is synced and renamed over its destination; else it
    /// goes. What failed, in Go's words.
    pub fn finish(self, status: u8) -> Result<(), String> {
        let Output::File { file, temp, dest } = self else {
            return Ok(());
        };
        let written = file.metadata().map(|m| m.len() > 0).unwrap_or(false);
        let done = (|| -> std::io::Result<()> {
            file.sync_all()?;
            drop(file);
            if status == 0 && written {
                std::fs::rename(&temp, &dest)?;
            }
            Ok(())
        })();
        let _ = std::fs::remove_file(&temp);
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
