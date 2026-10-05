//! `shards cp`'s work in the guest, as dockerd does it in a container's root (moby
//! daemon/archive_unix.go: containerStatPath, containerArchivePath, containerExtractToDir;
//! daemon/containerfs_linux.go, Stat): a path's stat, a tar archive of it, and an
//! archive unpacked into a directory. Init's built-ins run in the container's root, as
//! root, as dockerd's RunInFS does.
//!
//! Each fails with its status saying how: [`NOT_FOUND`] for a path that is not there
//! (dockerd's "Could not find the file"), [`INVALID`] for one dockerd calls an invalid
//! parameter, its words on stderr; 1 for anything else.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

/// The statuses a failed built-in exits with, past 1.
pub const NOT_FOUND: i32 = 2;
pub const INVALID: i32 = 3;

/// An error and the status it exits with.
pub struct Failed(pub i32, pub String);

impl From<io::Error> for Failed {
    fn from(e: io::Error) -> Failed {
        let code = if e.kind() == io::ErrorKind::NotFound {
            NOT_FOUND
        } else {
            1
        };
        Failed(code, e.to_string())
    }
}

/// `path`'s stat as dockerd's PathStat has it (container.PathStat), as one JSON line:
/// its name, size, Go FileMode, mtime and, for a link, its target resolved in the root.
pub fn stat(path: &[u8], out: &mut impl Write) -> Result<(), Failed> {
    let p = Path::new(OsStr::from_bytes(path));
    let lstat = std::fs::symlink_metadata(p)?;
    let target = if lstat.file_type().is_symlink() {
        follow_in_scope(p)?
    } else {
        PathBuf::new()
    };
    let name = p
        .file_name()
        .map_or_else(|| b"/".to_vec(), |n| n.as_bytes().to_vec());
    let line = format!(
        "{{\"name\":{},\"size\":{},\"mode\":{},\"mtime\":{},\"linkTarget\":{}}}\n",
        json(&name),
        lstat.size(),
        go_mode(&lstat),
        json(rfc3339(lstat.mtime(), lstat.mtime_nsec()).as_bytes()),
        json(target.as_os_str().as_bytes()),
    );
    out.write_all(line.as_bytes())?;
    Ok(())
}

/// A tar archive of `path` (containerArchivePath): a directory's contents under its own
/// name, anything else as itself.
pub fn archive(path: &[u8], out: &mut impl Write) -> Result<(), Failed> {
    let given = Path::new(OsStr::from_bytes(path));
    let joined = Path::new("/").join(given);
    let abs = shards_archive::copy::preserve_trailing_dot_or_separator(&clean(&joined), given);
    let lstat = std::fs::symlink_metadata(&abs)?;
    let base = abs
        .file_name()
        .map_or_else(|| b"/".to_vec(), |n| n.as_bytes().to_vec());
    let (dir, source_base) = if lstat.is_dir() {
        (abs.clone(), b".".to_vec())
    } else {
        let (dir, entry) = shards_archive::copy::split_path_dir_entry(&abs);
        (dir, entry.as_os_str().as_bytes().to_vec())
    };
    let opts = shards_archive::copy::tar_resource_rebase_opts(&source_base, &base);
    shards_archive::pack(&dir, &opts, out)
        .map(drop)
        .map_err(|e| Failed(1, e.to_string()))
}

/// Unpacks the archive on `input` into directory `path` (containerExtractToDir): one
/// whose path, its links followed, is not a directory is refused; the archive's owners
/// kept as root unpacks them, or, with `user`, every entry the container user's
/// (`docker cp -a`); a directory replaced by a file, or the reverse, only if `overwrite`.
pub fn extract(
    path: &[u8],
    user: Option<&[u8]>,
    overwrite: bool,
    input: &mut impl Read,
) -> Result<(), Failed> {
    let given = Path::new(OsStr::from_bytes(path));
    let resolved = std::fs::canonicalize(Path::new("/").join(given))?;
    let abs = shards_archive::copy::preserve_trailing_dot_or_separator(&resolved, given);
    if !std::fs::symlink_metadata(&abs)?.is_dir() {
        return Err(Failed(INVALID, "extraction point is not a directory".into()));
    }
    let chown = match user.filter(|u| !u.is_empty()) {
        Some(u) => Some(uid_gid(u).map_err(|e| Failed(INVALID, e))?),
        None => None,
    };
    let opts = shards_archive::UnpackOptions {
        no_overwrite_dir_non_dir: !overwrite,
        chown,
        ..Default::default()
    };
    shards_archive::untar(input, &abs, &opts).map_err(|e| Failed(1, e.to_string()))
}

/// FollowSymlinkInScope(path, "/") in a root that is the container's: each link on the
/// way followed, to at most 255, what is not there taken as it is.
fn follow_in_scope(path: &Path) -> Result<PathBuf, Failed> {
    let mut pending: Vec<PathBuf> = vec![clean(&Path::new("/").join(path))];
    let mut links = 0;
    let mut done = PathBuf::from("/");
    while let Some(next) = pending.pop() {
        let mut rest: Vec<std::ffi::OsString> = next
            .components()
            .filter_map(|c| match c {
                std::path::Component::Normal(n) => Some(n.to_os_string()),
                _ => None,
            })
            .collect();
        rest.reverse();
        done = PathBuf::from("/");
        while let Some(part) = rest.pop() {
            let candidate = done.join(&part);
            match std::fs::symlink_metadata(&candidate) {
                Ok(m) if m.file_type().is_symlink() => {
                    links += 1;
                    if links > 255 {
                        return Err(Failed(
                            1,
                            format!("evaluating symlinks in {}: too many links", path.display()),
                        ));
                    }
                    let target = std::fs::read_link(&candidate)?;
                    let base = if target.is_absolute() {
                        PathBuf::from("/")
                    } else {
                        done.clone()
                    };
                    let mut again = clean(&base.join(target));
                    for p in rest.iter().rev() {
                        again.push(p);
                    }
                    pending.push(again);
                    break;
                }
                _ => done = candidate,
            }
        }
    }
    Ok(clean(&done))
}

/// `path` cleaned as Go's filepath.Clean cleans an absolute one.
fn clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for c in path.components() {
        match c {
            std::path::Component::Normal(n) => out.push(n),
            std::path::Component::ParentDir => {
                out.pop();
            }
            _ => {}
        }
    }
    out
}

/// The container user `spec`'s IDs as dockerd reads them for `cp -a`
/// (archive_tarcopyoptions_unix.go, getUIDGID): a name or ID, then perhaps `:` and a
/// group's; names from the container's own databases.
fn uid_gid(spec: &[u8]) -> Result<(i64, i64), String> {
    let spec = String::from_utf8_lossy(spec).into_owned();
    let (user, group) = spec.split_once(':').unwrap_or((&spec, ""));
    let (mut uid, mut gid) = (0i64, 0i64);
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let entries = |text: &str| -> Vec<Vec<String>> {
        text.lines()
            .map(|l| l.split(':').map(str::to_string).collect())
            .collect()
    };
    if !user.is_empty() {
        match user.parse::<u32>().ok().filter(|n| *n <= i32::MAX as u32) {
            Some(id) => {
                uid = i64::from(id);
                // An ID with no entry has group 0 (lookupUser).
                gid = entries(&passwd)
                    .iter()
                    .find(|e| e.get(2).and_then(|u| u.parse::<i64>().ok()) == Some(uid))
                    .and_then(|e| e.get(3)?.parse().ok())
                    .unwrap_or(0);
            }
            None => {
                let found = entries(&passwd)
                    .into_iter()
                    .find(|e| e.first().map(String::as_str) == Some(user));
                let found = found.ok_or_else(|| {
                    format!(
                        "failed to look up user {user:?} in container: no matching entries in passwd file"
                    )
                })?;
                uid = found.get(2).and_then(|u| u.parse().ok()).unwrap_or(0);
                gid = found.get(3).and_then(|g| g.parse().ok()).unwrap_or(0);
            }
        }
    }
    if !group.is_empty() {
        gid = match group.parse::<u32>().ok().filter(|n| *n <= i32::MAX as u32) {
            Some(id) => i64::from(id),
            None => {
                let groups = std::fs::read_to_string("/etc/group").unwrap_or_default();
                let found = entries(&groups)
                    .into_iter()
                    .find(|e| e.first().map(String::as_str) == Some(group));
                found.and_then(|e| e.get(2)?.parse().ok()).ok_or_else(|| {
                    format!(
                        "failed to look up group {group:?} in container: no matching entries in group file"
                    )
                })?
            }
        };
    }
    Ok((uid, gid))
}

/// A file's mode as Go's os.FileMode holds it (os/types.go, stat_linux.go fillFileStatFromSys).
fn go_mode(m: &std::fs::Metadata) -> u32 {
    let t = m.file_type();
    let mut mode = m.mode() & 0o777;
    if t.is_dir() {
        mode |= 1 << 31;
    } else if t.is_symlink() {
        mode |= 1 << 27;
    } else if t.is_fifo() {
        mode |= 1 << 25;
    } else if t.is_socket() {
        mode |= 1 << 24;
    } else if t.is_block_device() {
        mode |= 1 << 26;
    } else if t.is_char_device() {
        mode |= (1 << 26) | (1 << 21);
    }
    let raw = m.mode();
    if raw & libc::S_ISUID != 0 {
        mode |= 1 << 23;
    }
    if raw & libc::S_ISGID != 0 {
        mode |= 1 << 22;
    }
    if raw & libc::S_ISVTX != 0 {
        mode |= 1 << 20;
    }
    mode
}

/// Seconds and nanoseconds since the epoch as Go's time.RFC3339Nano writes UTC.
fn rfc3339(secs: i64, nsec: i64) -> String {
    let days = secs.div_euclid(86_400);
    let day = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let mut frac = String::new();
    if nsec > 0 {
        frac = format!(".{nsec:09}");
        while frac.ends_with('0') {
            frac.pop();
        }
    }
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}{frac}Z",
        day / 3600,
        day % 3600 / 60,
        day % 60
    )
}

/// `bytes` as a JSON string: invalid UTF-8 as U+FFFD, as Go's encoding/json writes it.
fn json(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
