//! What an orchestrator stages into a member before its program runs (D231): variables
//! over its image's, and regular files under its home, the scratch tmpfs init makes for it
//! (`HOME=/tmp`, domains.rs). The host checked each entry as it made it; each is read and
//! checked again here, from the guest's own copy (D109: every check at both ends).

use std::ffi::CString;
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

use shards_abi::run::members;

/// Refuses an entry naming a member this run starts no program for: one the image does
/// not declare, or one of files alone, which nothing would ever read it in.
pub fn check(setup: &[Vec<u8>], starts: impl Fn(u32) -> bool) -> Result<(), String> {
    let staged = members::staged_members(setup).map_err(|e| format!("a staging entry: {e}"))?;
    match staged.into_iter().find(|&m| !starts(m)) {
        Some(m) => Err(format!(
            "something is staged into member {m}, which this image starts no program for"
        )),
        None => Ok(()),
    }
}

/// Member `member`'s environment: its image's (`image`), each variable staged for it
/// replacing the image's of the same key in place, or added after them, in the order
/// staged; a key staged twice keeps its last.
pub fn env(image: &[CString], member: u32, setup: &[Vec<u8>]) -> Result<Vec<CString>, String> {
    let staged = members::envs_of(setup, member).map_err(|e| format!("a staged variable: {e}"))?;
    let mut out: Vec<CString> = image.to_vec();
    for e in staged {
        let variable = CString::new(e.variable).map_err(|_| "a staged variable holding a NUL".to_string())?;
        let key = e.key();
        let same = |c: &CString| c.as_bytes().split(|&b| b == b'=').next() == Some(key);
        match out.iter_mut().find(|c| same(c)) {
            Some(slot) => *slot = variable,
            None => out.push(variable),
        }
    }
    Ok(out)
}

/// Member `member`'s working directory, where its run names one: the project path it works
/// in, absolute and plain, checked again here as the host checked it; one at most.
pub fn workdir(member: u32, setup: &[Vec<u8>]) -> Result<Option<CString>, String> {
    members::workdir_of(setup, member)
        .map_err(|e| format!("a staged working directory: {e}"))?
        .map(|w| CString::new(w).map_err(|_| "a staged working directory holding a NUL".to_string()))
        .transpose()
}

/// Writes the files staged for `member` beneath `home`, a directory descriptor of its
/// scratch tmpfs, each owned by `owner` (uid, gid), its directories made 0700 and owned
/// so, nothing followed that a link could redirect, and nothing written twice. `home` is
/// `None` where the image has no `/tmp` for a scratch (D139): a member given files then is
/// refused, as it would have nowhere to read them.
pub fn files(
    home: Option<BorrowedFd<'_>>,
    owner: (u32, u32),
    member: u32,
    setup: &[Vec<u8>],
) -> Result<(), String> {
    let staged = members::files_of(setup, member).map_err(|e| format!("a staged file: {e}"))?;
    if staged.is_empty() {
        return Ok(());
    }
    let home = home.ok_or("files are staged for a member whose image has no /tmp, its home")?;
    for f in staged {
        write(home, owner, &f).map_err(|e| {
            format!(
                "staging {} into member {member}: {e}",
                String::from_utf8_lossy(f.path)
            )
        })?;
    }
    Ok(())
}

fn write(home: BorrowedFd<'_>, (uid, gid): (u32, u32), f: &members::File<'_>) -> io::Result<()> {
    let mut parts = f.path.split(|&b| b == b'/').peekable();
    let mut dir: Option<OwnedFd> = None;
    while let Some(part) = parts.next() {
        let name = CString::new(part).map_err(io::Error::other)?;
        let at = dir.as_ref().map_or(home.as_raw_fd(), AsRawFd::as_raw_fd);
        if parts.peek().is_none() {
            // SAFETY: openat(2) of a NUL-terminated name beneath a directory descriptor of
            // ours; O_EXCL and O_NOFOLLOW refuse a file there already and a link.
            let fd = unsafe {
                libc::openat(
                    at,
                    name.as_ptr(),
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    f.mode as libc::c_uint,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: the descriptor openat just made, owned from here on.
            let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
            file.write_all(f.bytes)?;
            // Owner first, then the mode: a chown clears set-ID bits (chown(2)), and the
            // mode is set whole, past the umask the creation took.
            // SAFETY: fchown(2) of the descriptor held.
            if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fchmod(2) of the descriptor held.
            if unsafe { libc::fchmod(file.as_raw_fd(), f.mode as libc::mode_t) } != 0 {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        // SAFETY: mkdirat(2) of a NUL-terminated name beneath a directory descriptor of ours.
        let made = unsafe { libc::mkdirat(at, name.as_ptr(), 0o700) } == 0;
        if !made {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EEXIST) {
                return Err(e);
            }
        }
        // SAFETY: openat(2) of a NUL-terminated name beneath a directory descriptor of
        // ours, refusing a link and anything but a directory.
        let fd = unsafe {
            libc::openat(
                at,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor openat just made, owned from here on.
        let next = unsafe { OwnedFd::from_raw_fd(fd) };
        // One it made is the member's; one there before was made so by an earlier entry.
        // SAFETY: fchown(2) of the descriptor held.
        if made && unsafe { libc::fchown(next.as_raw_fd(), uid, gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        dir = Some(next);
    }
    Err(io::Error::other("an empty path"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::fd::AsFd as _;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    fn own() -> (u32, u32) {
        // SAFETY: getuid(2) and getgid(2) cannot fail.
        unsafe { (libc::getuid(), libc::getgid()) }
    }

    fn cstrings(list: &[&str]) -> Vec<CString> {
        list.iter().map(|s| CString::new(*s).unwrap()).collect()
    }

    /// A staged variable replaces the image's of its key in place and adds the rest in
    /// order; another member's are not its own.
    #[test]
    fn staged_variables_replace_by_key() {
        let setup = vec![
            members::env(1, b"HOME=/tmp/elsewhere").unwrap(),
            members::env(1, b"OPENAI_API_KEY=sk").unwrap(),
            members::env(2, b"NOT=mine").unwrap(),
            members::env(1, b"OPENAI_API_KEY=sk2").unwrap(),
        ];
        let image = cstrings(&["PATH=/bin", "HOME=/tmp", "SHARDS_DOMAIN=harness codex"]);
        let got = env(&image, 1, &setup).unwrap();
        assert_eq!(
            got,
            cstrings(&[
                "PATH=/bin",
                "HOME=/tmp/elsewhere",
                "SHARDS_DOMAIN=harness codex",
                "OPENAI_API_KEY=sk2"
            ])
        );
        assert_eq!(env(&image, 0, &setup).unwrap(), image);
    }

    /// A crafted entry naming init's own variable is refused in the guest too.
    #[test]
    fn the_domains_label_is_never_staged() {
        let crafted = vec![[members::ENV_KEY, b"0\0SHARDS_DOMAIN=agent x"].concat()];
        assert!(env(&cstrings(&["A=b"]), 0, &crafted).is_err());
    }

    /// A member's working directory is the one its run names, read again as the host made
    /// it; a crafted relative or escaping one, or a second, is refused here too.
    #[test]
    fn a_working_directory_is_read_again_in_the_guest() {
        let setup = vec![
            members::workdir(1, b"/Users/ada/proj").unwrap(),
            members::env(1, b"A=b").unwrap(),
        ];
        assert_eq!(
            workdir(1, &setup).unwrap(),
            Some(CString::new("/Users/ada/proj").unwrap())
        );
        assert_eq!(workdir(0, &setup).unwrap(), None);
        for crafted in [&b"1\0proj"[..], b"1\0/Users/../etc", b"1\0/Users//x"] {
            let setup = vec![[members::WORKDIR_KEY, crafted].concat()];
            assert!(workdir(1, &setup).is_err(), "{crafted:?}");
        }
        let mut twice = setup.clone();
        twice.push(members::workdir(1, b"/elsewhere").unwrap());
        assert!(
            workdir(1, &twice)
                .unwrap_err()
                .contains("second working directory")
        );
    }

    /// Entries naming a member the run starts nothing for are refused as a whole.
    #[test]
    fn staging_into_no_running_member_is_refused() {
        let setup = vec![members::env(4, b"A=b").unwrap()];
        assert!(check(&setup, |m| m == 4).is_ok());
        assert!(check(&setup, |m| m == 3).unwrap_err().contains("member 4"));
        assert!(check(&[], |_| false).is_ok());
    }

    /// Files land beneath the home at their paths, with exactly their bytes and modes, their
    /// new directories 0700; a second entry for one path is refused, never written over.
    #[test]
    fn files_are_written_beneath_the_home() {
        let dir = shards_testdir::TempDir::new("staged").unwrap();
        let home = std::fs::File::open(dir.path()).unwrap();
        let setup = vec![
            members::file(0, 0o600, b".codex/auth.json", b"{\"k\":\"v\"}").unwrap(),
            members::file(0, 0o755, b"bin/run", b"#!/bin/sh\n").unwrap(),
            members::file(1, 0o600, b"other", b"x").unwrap(),
        ];
        files(Some(home.as_fd()), own(), 0, &setup).unwrap();
        let auth = dir.path().join(".codex/auth.json");
        assert_eq!(std::fs::read(&auth).unwrap(), b"{\"k\":\"v\"}");
        assert_eq!(
            std::fs::metadata(&auth).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(dir.path().join(".codex"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(dir.path().join("bin/run"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
        assert_eq!(std::fs::metadata(&auth).unwrap().uid(), own().0);
        assert!(!dir.path().join("other").exists());
        let twice = vec![members::file(0, 0o600, b".codex/auth.json", b"again").unwrap()];
        assert!(files(Some(home.as_fd()), own(), 0, &twice).is_err());
        assert_eq!(std::fs::read(&auth).unwrap(), b"{\"k\":\"v\"}");
    }

    /// A link where a staged path goes is never followed, as a directory or as the file.
    #[test]
    fn links_in_the_way_are_not_followed() {
        let dir = shards_testdir::TempDir::new("staged-links").unwrap();
        let outside = shards_testdir::TempDir::new("staged-outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("d")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("f"), dir.path().join("f")).unwrap();
        let home = std::fs::File::open(dir.path()).unwrap();
        for path in [&b"d/x"[..], b"f"] {
            let setup = vec![members::file(0, 0o600, path, b"x").unwrap()];
            assert!(files(Some(home.as_fd()), own(), 0, &setup).is_err(), "{path:?}");
        }
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    /// Files with no home to go to are refused; none staged needs none.
    #[test]
    fn files_need_a_home() {
        let setup = vec![members::file(0, 0o600, b"a", b"x").unwrap()];
        assert!(files(None, own(), 0, &setup).is_err());
        assert!(files(None, own(), 1, &setup).is_ok());
    }
}
