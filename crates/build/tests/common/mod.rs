//! Fixture trees made on the host, as the Go generators make them.
#![allow(dead_code)]

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use serde_json::Value;

/// The tree's entries a Windows host cannot hold, which scripts/build/context's generator
/// makes none of there either: a FIFO, which Windows keeps in the named-pipe file system
/// (`\\.\pipe\`), never in a directory (stat_windows.go's fsutil reads no other kind).
#[cfg(windows)]
pub const NOT_ON_WINDOWS: &[&str] = &["fifo"];

#[cfg(unix)]
pub fn times(p: &Path, s: i64, ns: i64) {
    let c = CString::new(p.as_os_str().as_bytes()).unwrap();
    let ts = [
        libc::timespec {
            tv_sec: s as _,
            tv_nsec: ns as _,
        },
        libc::timespec {
            tv_sec: s as _,
            tv_nsec: ns as _,
        },
    ];
    // SAFETY: `c` is NUL-terminated and `ts` holds the two times utimensat(2) reads.
    let r = unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    assert_eq!(r, 0, "{}", p.display());
}

/// The path's own access and write times, a reparse point's not followed, as the
/// generator's SetFileTime sets them (NTFS keeps 100 ns, both truncating to it).
#[cfg(windows)]
pub fn times(p: &Path, s: i64, ns: i64) {
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_WRITE_ATTRIBUTES,
    };
    let t = std::time::UNIX_EPOCH + std::time::Duration::new(s as u64, ns as u32);
    std::fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(p)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t))
        .unwrap();
}

/// A symlink at `p` to `target`, as Go's os.Symlink makes one: on Windows its separators
/// Windows' and a directory's link where its target, from the link's directory, is a
/// directory now. False where Windows lets this user make none (ERROR_PRIVILEGE_NOT_HELD:
/// no Developer Mode, nor SeCreateSymbolicLinkPrivilege).
fn symlink(target: &str, p: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, p).unwrap();
        true
    }
    #[cfg(windows)]
    {
        let target = target.replace('/', "\\");
        let dest = p.parent().unwrap().join(&target);
        let made = if std::fs::metadata(dest).is_ok_and(|m| m.is_dir()) {
            std::os::windows::fs::symlink_dir(&target, p)
        } else {
            std::os::windows::fs::symlink_file(&target, p)
        };
        match made {
            Err(e) if e.raw_os_error() == Some(1314) => false,
            made => {
                made.unwrap();
                true
            }
        }
    }
}

/// An AF_UNIX socket's file at `p`, left as its listener closes.
#[cfg(unix)]
fn unix_socket(p: &Path) {
    std::os::unix::net::UnixListener::bind(p).unwrap();
}

/// An AF_UNIX socket's file at `p` (Windows 10 1803 and later, a reparse point of tag
/// IO_REPARSE_TAG_AF_UNIX), left as its socket closes: Go's net.Listen("unix") with its
/// unlinking on close turned off, as the generator makes it.
#[cfg(windows)]
fn unix_socket(p: &Path) {
    use windows_sys::Win32::Networking::WinSock::{
        AF_UNIX, INVALID_SOCKET, SOCK_STREAM, SOCKADDR, SOCKADDR_UN, WSADATA, WSAStartup, bind, closesocket,
        socket,
    };
    let path = p.to_str().unwrap().as_bytes();
    let mut addr = SOCKADDR_UN {
        sun_family: AF_UNIX,
        ..Default::default()
    };
    assert!(
        path.len() < addr.sun_path.len(),
        "{}: too long a socket path",
        p.display()
    );
    for (d, s) in addr.sun_path.iter_mut().zip(path) {
        *d = *s as i8;
    }
    // SAFETY: `data` is a WSADATA for WSAStartup to fill; `s` is the socket made here,
    // bound to `addr`, a whole SOCKADDR_UN its length says, and closed once.
    unsafe {
        let mut data: WSADATA = std::mem::zeroed();
        assert_eq!(WSAStartup(0x0202, &mut data), 0);
        let s = socket(i32::from(AF_UNIX), SOCK_STREAM, 0);
        assert_ne!(s, INVALID_SOCKET);
        let bound = bind(
            s,
            (&raw const addr).cast::<SOCKADDR>(),
            std::mem::size_of::<SOCKADDR_UN>() as i32,
        );
        closesocket(s);
        assert_eq!(bound, 0, "bind {}", p.display());
    }
}

/// A mode's bits set on the path: on Windows the one bit NTFS keeps, read-only where the
/// owner may not write, as Go's os.Chmod sets it (syscall.Chmod).
fn chmod(p: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    #[cfg(windows)]
    {
        let mut perm = std::fs::symlink_metadata(p).unwrap().permissions();
        perm.set_readonly(mode & 0o200 == 0);
        std::fs::set_permissions(p, perm).unwrap();
    }
}

/// Makes `tree` under `root`: false where this host lets its user make no symlinks
/// (Windows without Developer Mode), and the caller then skips.
pub fn make(root: &Path, tree: &Value) -> bool {
    let mut dirs = Vec::new();
    for e in tree.as_array().unwrap() {
        let p = root.join(e["path"].as_str().unwrap().trim_start_matches('/'));
        let mtime = e.get("mtime").map_or((1_600_000_000, 500), |m| {
            (m[0].as_i64().unwrap(), m[1].as_i64().unwrap())
        });
        let ty = e["type"].as_str().unwrap();
        #[cfg(windows)]
        if NOT_ON_WINDOWS.contains(&ty) {
            continue;
        }
        match ty {
            "dir" => {
                std::fs::create_dir(&p).unwrap();
                dirs.push((p.clone(), mtime));
            }
            "file" => std::fs::write(&p, e["data"].as_str().unwrap()).unwrap(),
            "symlink" => {
                if !symlink(e["target"].as_str().unwrap(), &p) {
                    return false;
                }
            }
            "hardlink" => {
                std::fs::hard_link(
                    root.join(e["target"].as_str().unwrap().trim_start_matches('/')),
                    &p,
                )
                .unwrap();
                continue;
            }
            #[cfg(unix)]
            "fifo" => {
                let c = CString::new(p.as_os_str().as_bytes()).unwrap();
                // SAFETY: `c` is a NUL-terminated path.
                assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            }
            "socket" => unix_socket(&p),
            other => panic!("{other}"),
        }
        if ty != "symlink" {
            let default = if ty == "dir" { 0o755 } else { 0o644 };
            let mode = e.get("mode").and_then(Value::as_u64).unwrap_or(default) as u32;
            chmod(&p, mode);
        }
        if ty != "dir" {
            times(&p, mtime.0, mtime.1);
        }
    }
    for (p, t) in dirs.iter().rev() {
        times(p, t.0, t.1);
    }
    true
}
