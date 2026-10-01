//! Fixture trees made on the host, as the Go generators make them.
#![allow(dead_code)]

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use serde_json::Value;

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

pub fn make(root: &Path, tree: &Value) {
    use std::os::unix::fs::PermissionsExt;
    let mut dirs = Vec::new();
    for e in tree.as_array().unwrap() {
        let p = root.join(e["path"].as_str().unwrap().trim_start_matches('/'));
        let mtime = e.get("mtime").map_or((1_600_000_000, 500), |m| {
            (m[0].as_i64().unwrap(), m[1].as_i64().unwrap())
        });
        let ty = e["type"].as_str().unwrap();
        match ty {
            "dir" => {
                std::fs::create_dir(&p).unwrap();
                dirs.push((p.clone(), mtime));
            }
            "file" => std::fs::write(&p, e["data"].as_str().unwrap()).unwrap(),
            "symlink" => std::os::unix::fs::symlink(e["target"].as_str().unwrap(), &p).unwrap(),
            "hardlink" => {
                std::fs::hard_link(
                    root.join(e["target"].as_str().unwrap().trim_start_matches('/')),
                    &p,
                )
                .unwrap();
                continue;
            }
            "fifo" => {
                let c = CString::new(p.as_os_str().as_bytes()).unwrap();
                // SAFETY: `c` is a NUL-terminated path.
                assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            }
            "socket" => {
                std::os::unix::net::UnixListener::bind(&p).unwrap();
            }
            other => panic!("{other}"),
        }
        if ty != "symlink" {
            let default = if ty == "dir" { 0o755 } else { 0o644 };
            let mode = e.get("mode").and_then(Value::as_u64).unwrap_or(default) as u32;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        if ty != "dir" {
            times(&p, mtime.0, mtime.1);
        }
    }
    for (p, t) in dirs.iter().rev() {
        times(p, t.0, t.1);
    }
}
