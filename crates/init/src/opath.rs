//! Path descriptors (open(2) `O_PATH`): a file opened as a place in the tree alone. The
//! file's own open is never run: no permission on it is asked, its file system's open and
//! release are not called, and no fsnotify event says it was opened (Linux v7.2-rc4
//! fs/open.c, `do_dentry_open`; `build_open_flags`, whose access mode for `O_PATH` is none).
//! What one serves is being a place: fchdir(2), the directory of the `*at` calls,
//! `/proc/self/fd/N`, fstat(2) and SCM_RIGHTS (open(2), `O_PATH`).
//!
//! Opened by open(2) itself, never through std's `OpenOptions`: std clears the bits of
//! `O_ACCMODE` from the flags `custom_flags` gives it (library/std/src/sys/fs/unix.rs
//! `File::open_c`, Rust 1.94.1 to 1.100), and musl's `O_ACCMODE` holds `O_PATH`
//! (`03|O_SEARCH`, its `O_SEARCH` being `O_PATH`: musl 1.2.5 include/fcntl.h; libc 0.2.189
//! `0o10000003`). Built for musl, as init alone is, such an open is a read-only one (PM
//! M525, M527).

use std::io;
use std::os::fd::{FromRawFd as _, OwnedFd};
use std::path::Path;

/// `path` opened as a path descriptor: open(2) with `flags` (`O_DIRECTORY`, `O_NOFOLLOW`)
/// beside `O_PATH` and `O_CLOEXEC`, again where a signal interrupts it, as std's open is.
pub fn open(path: impl AsRef<Path>, flags: libc::c_int) -> io::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt as _;
    let path = std::ffi::CString::new(path.as_ref().as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path holds NUL"))?;
    loop {
        // SAFETY: open(2) of a NUL-terminated path; no O_CREAT, so no mode is read.
        let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_PATH | libc::O_CLOEXEC) };
        if fd >= 0 {
            // SAFETY: a descriptor just opened, owned here alone.
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::os::fd::AsRawFd as _;

    /// F_GETFL and F_GETFD of `fd`, as the kernel holds them.
    fn flags(fd: &OwnedFd) -> (libc::c_int, libc::c_int) {
        // SAFETY: fcntl(2) F_GETFL and F_GETFD of a descriptor of ours.
        unsafe {
            (
                libc::fcntl(fd.as_raw_fd(), libc::F_GETFL),
                libc::fcntl(fd.as_raw_fd(), libc::F_GETFD),
            )
        }
    }

    /// A directory and a file each opened as a path descriptor, which no exec keeps, as the
    /// kernel holds its open file (F_GETFL holds `O_PATH`, fcntl(2)) on every libc; and a
    /// directory none may read, as a path asks nothing of the file it names.
    #[test]
    fn what_it_opens_is_a_path_descriptor() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = shards_testdir::TempDir::new("opath").unwrap();
        let file = dir.join("file");
        std::fs::write(&file, b"x").unwrap();
        let unread = dir.join("unread");
        std::fs::create_dir(&unread).unwrap();
        std::fs::set_permissions(&unread, std::fs::Permissions::from_mode(0o000)).unwrap();
        let opened: Vec<_> = [
            (dir.path(), libc::O_DIRECTORY),
            (file.as_path(), libc::O_NOFOLLOW),
            (unread.as_path(), libc::O_DIRECTORY),
        ]
        .into_iter()
        .map(|(path, extra)| (path, open(path, extra)))
        .collect();
        // Readable again before any assertion, so that the directory is removed after.
        std::fs::set_permissions(&unread, std::fs::Permissions::from_mode(0o700)).unwrap();
        for (path, fd) in opened {
            let fd = fd.unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let (status, descriptor) = flags(&fd);
            assert!(
                status >= 0 && status & libc::O_PATH != 0,
                "{}: not O_PATH: {status:#o}",
                path.display()
            );
            assert!(
                descriptor >= 0 && descriptor & libc::FD_CLOEXEC != 0,
                "{}: kept across exec: {descriptor:#x}",
                path.display()
            );
        }
        assert_eq!(
            open(&file, libc::O_DIRECTORY).unwrap_err().raw_os_error(),
            Some(libc::ENOTDIR)
        );
    }

    /// No source of init's asks std's `OpenOptions` for a path descriptor (`custom_flags`
    /// with `O_PATH`): such an open passes every test on glibc and opens the file read-only
    /// on musl (module docs). Comments are not code: what follows `//` on a line is not read.
    #[test]
    fn no_source_asks_std_for_a_path_descriptor() {
        let call = concat!("custom", "_flags(");
        let mut found = Vec::new();
        let mut dirs = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let code: Vec<String> = std::fs::read_to_string(&path)
                    .unwrap()
                    .lines()
                    .map(|l| l.split("//").next().unwrap_or_default().to_string())
                    .collect();
                let code = code.join("\n");
                for (at, _) in code.match_indices(call) {
                    let args = code.get(at + call.len()..).unwrap_or_default();
                    let args = args.split(')').next().unwrap_or_default();
                    if args.contains("O_PATH") {
                        let line = code.get(..at).unwrap_or_default().matches('\n').count() + 1;
                        found.push(format!("{}:{line}", path.display()));
                    }
                }
            }
        }
        assert!(
            found.is_empty(),
            "O_PATH asked of std's OpenOptions, which musl's O_ACCMODE takes away; open it with crate::opath::open: {found:?}"
        );
    }
}
