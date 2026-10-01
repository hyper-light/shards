//! BuildKit's file actions on snapshots (dockerfile/1.27.1's solver/llbsolver/file
//! backend.go and backend_unix.go, and ops/user_linux.go for owners by name): `mkdir`,
//! `mkfile` and `copy`, the actions the Dockerfile frontend asks for.
//!
//! Paths in errors are inside the snapshot, as BuildKit trims its mount's directory from
//! them.

use shards_dockerfile::go;
use shards_dockerfile::llb::{OpChown, OpUser};
use shards_image::erofs::{DataRef, Kind, Source};

use crate::Error;
use std::path::Path;

use crate::archive;
use crate::copy::{self, Chown, CopyInfo, User};
use crate::data::Sources;
use crate::vfs::{self, Errno, Fs, PathError};

/// The largest /etc/passwd or /etc/group read (user_linux.go maxUserFileBytes).
const MAX_USER_FILE: u64 = 10 << 20;
/// bufio.Scanner's longest line by default, which /etc/passwd is read with.
const MAX_PASSWD_LINE: usize = 64 * 1024;
/// The longest /etc/group line, which ParseGroupFilter allows.
const MAX_GROUP_LINE: usize = 1024 * 1024;

/// BuildKit's timestampToTime: -1 is none, anything else nanoseconds since 1970.
pub fn timestamp(ts: i64) -> Option<(i64, u32)> {
    if ts == -1 {
        return None;
    }
    let sec = ts.div_euclid(1_000_000_000);
    let nsec = ts.rem_euclid(1_000_000_000) as u32;
    Some((sec, nsec))
}

fn show(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn os(e: PathError) -> Error {
    Error(e.to_string())
}

/// BuildKit's mkdir action (WORKDIR's, among others).
pub fn mkdir(fs: &mut Fs, path: &[u8], mode: i32, parents: bool, ch: Chown, ts: i64) -> Result<(), Error> {
    let p = copy::root_path(fs, path)?;
    let perm = (mode as u32) & 0o777;
    let tm = timestamp(ts);
    if parents {
        copy::mkdir_all(fs, &p, perm, ch, tm)?;
        return Ok(());
    }
    match fs.mkdir(&p, perm) {
        Ok(_) => {}
        Err(e) if e.errno == Errno::Exist => return Ok(()),
        Err(e) => return Err(os(e)),
    }
    copy::chown(fs, &p, None, ch).map_err(os)?;
    copy::utimes(fs, &p, tm)
}

/// BuildKit's mkfile action (a heredoc's file, among others): written beside its path
/// and renamed over it, as replaceWithFile does, so what was there is replaced and never
/// written through.
pub fn mkfile(
    fs: &mut Fs,
    path: &[u8],
    mode: i32,
    data: (u64, DataRef),
    ch: Chown,
    ts: i64,
) -> Result<(), Error> {
    let target = vfs::join(b"/", path);
    let p = copy::root_path(fs, &target)?;
    if p == b"/" {
        return Err(os(PathError {
            op: "mkfile",
            path: target,
            errno: Errno::IsDir,
        }));
    }
    let named = |e: PathError| {
        os(PathError {
            op: e.op,
            path: p.clone(),
            errno: e.errno,
        })
    };
    let dir = copy::dir(&p);
    let mut n = 0u32;
    let tmp = loop {
        let candidate = vfs::join(&dir, format!(".tmp-mkfile{n}").as_bytes());
        if fs.lstat(&candidate).is_err() {
            break candidate;
        }
        n += 1;
    };
    let staged = (|| {
        let id = fs.create(&tmp, 0o600)?;
        fs.set_data(id, data.0, data.1);
        fs.chmod(&tmp, (mode as u32) & 0o777)?;
        copy::chown(fs, &tmp, None, ch)?;
        if let Some(t) = timestamp(ts) {
            fs.utimes(&tmp, t)?;
        }
        fs.rename(&tmp, &p)
    })();
    if let Err(e) = staged {
        let _ = fs.remove(&tmp);
        return Err(named(e));
    }
    Ok(())
}

/// BuildKit's cleanPath: absolute, keeping a trailing `/` or `/.`.
pub fn clean_path(s: &[u8]) -> Vec<u8> {
    let mut s2 = vfs::join(b"/", s);
    if s.ends_with(b"/.") {
        if s2 != b"/" {
            s2.push(b'/');
        }
        s2.push(b'.');
    } else if s.ends_with(b"/") && s2 != b"/" {
        s2.push(b'/');
    }
    s2
}

/// A copy action's fields (pb.FileActionCopy), as the Dockerfile frontend fills them.
#[derive(Debug, Clone, Default)]
pub struct CopyAction {
    pub src: Vec<u8>,
    pub dest: Vec<u8>,
    pub mode: i32,
    pub mode_str: Vec<u8>,
    pub follow_symlink: bool,
    pub dir_copy_contents: bool,
    pub attempt_unpack: bool,
    pub create_dest_path: bool,
    pub allow_wildcard: bool,
    pub allow_empty_wildcard: bool,
    pub timestamp: i64,
    pub include_patterns: Vec<Vec<u8>>,
    pub exclude_patterns: Vec<Vec<u8>>,
}

/// BuildKit's docopy, from `src` into `dest`. An action that may unpack (ADD's) unpacks
/// each local archive into the destination ([`archive::unpack`]), reading its bytes
/// through `sources` and decompressing it into `stage`, and copies what is not one. An
/// owner the action names owns every entry it unpacks.
pub fn copy(
    src: &Fs,
    dest: &mut Fs,
    action: &CopyAction,
    ch: Chown,
    sources: &mut Sources,
    stage: &Path,
) -> Result<(), Error> {
    let owner = match ch {
        Chown::To(u) => Some(u),
        Chown::Keep => None,
    };
    let src_path = clean_path(&action.src);
    let dest_path = clean_path(&action.dest);
    if !action.create_dest_path {
        let p = copy::root_path(dest, &vfs::join(b"/", &action.dest))?;
        if let Err(e) = dest.lstat(&copy::dir(&p)) {
            return Err(Error(format!("failed to stat {}: {e}", show(&action.dest))));
        }
    }
    let ci = CopyInfo {
        chown: ch,
        utime: timestamp(action.timestamp),
        mode: if action.mode_str.is_empty() && action.mode != -1 {
            Some(action.mode as u32)
        } else {
            None
        },
        mode_str: action.mode_str.clone(),
        copy_dir_contents: action.dir_copy_contents,
        follow_links: action.follow_symlink,
        include: action.include_patterns.clone(),
        exclude: action.exclude_patterns.clone(),
    };
    let matches = if action.allow_wildcard {
        let m = copy::resolve_wildcards(src, &src_path, action.follow_symlink)?;
        if m.is_empty() {
            if action.allow_empty_wildcard {
                return Ok(());
            }
            return Err(Error(format!("{} not found", show(&src_path))));
        }
        m
    } else {
        vec![src_path]
    };
    for s in matches {
        if action.attempt_unpack && archive::is_archive(src, &s, sources)? {
            archive::unpack(src, &s, dest, &dest_path, ch, owner, ci.utime, sources, stage)?;
            continue;
        }
        copy::copy(src, &s, dest, &dest_path, &ci)?;
    }
    Ok(())
}

/// BuildKit's readUser: the owner a ChownOpt names, by ID or by a name looked up in
/// the snapshots it names. A name not found is root.
pub fn read_user(
    chown: Option<&OpChown>,
    users: Option<&Fs>,
    groups: Option<&Fs>,
    data: &mut dyn Source,
) -> Result<Chown, Error> {
    let Some(chown) = chown else {
        return Ok(Chown::Keep);
    };
    let mut us = User { uid: 0, gid: 0 };
    match &chown.user {
        Some(OpUser::Name { name, .. }) => {
            let fs = users.ok_or_else(|| Error("invalid missing user mount".into()))?;
            if let Some(file) = user_file(fs, b"/etc/passwd", data)? {
                let found = scan(&file, MAX_PASSWD_LINE, false, |fields| {
                    (fields.first().copied() == Some(name.as_slice())).then(|| {
                        (
                            atoi(fields.get(2).copied().unwrap_or_default()),
                            atoi(fields.get(3).copied().unwrap_or_default()),
                        )
                    })
                })?;
                if let Some((uid, gid)) = found {
                    us = User { uid, gid };
                }
            }
        }
        Some(OpUser::Id(id)) => us = User { uid: *id, gid: *id },
        None => {}
    }
    match &chown.group {
        Some(OpUser::Name { name, .. }) => {
            let fs = groups.ok_or_else(|| Error("invalid missing group mount".into()))?;
            if let Some(file) = user_file(fs, b"/etc/group", data)? {
                let found = scan(&file, MAX_GROUP_LINE, true, |fields| {
                    (fields.first().copied() == Some(name.as_slice()))
                        .then(|| atoi(fields.get(2).copied().unwrap_or_default()))
                })?;
                if let Some(gid) = found {
                    us.gid = gid;
                }
            }
        }
        Some(OpUser::Id(id)) => us.gid = *id,
        None => {}
    }
    Ok(Chown::To(us))
}

/// openUserFile: the file inside the snapshot, if it is there; a regular file, read up to
/// its limit.
fn user_file(fs: &Fs, orig: &[u8], data: &mut dyn Source) -> Result<Option<Vec<u8>>, Error> {
    let p = copy::root_path(fs, orig)?;
    let id = match fs.lstat(&p) {
        Ok(id) => id,
        Err(e) if matches!(e.errno, Errno::NoEnt | Errno::NotDir) => return Ok(None),
        Err(e) => {
            return Err(os(PathError {
                op: "open",
                path: orig.to_vec(),
                errno: e.errno,
            }));
        }
    };
    match fs.node(id).map(|n| &n.kind) {
        Some(Kind::File { size, data: at }) => {
            let n = (*size).min(MAX_USER_FILE + 1);
            let mut buf = vec![0u8; usize::try_from(n).map_err(|_| Error("file too large".into()))?];
            if n > 0 {
                data.read_at(*at, 0, &mut buf)
                    .map_err(|e| Error(format!("read {}: {e}", show(orig))))?;
            }
            if *size > MAX_USER_FILE {
                // Lines are scanned until the limit is passed, so a line too long before
                // it fails first.
                scan(
                    &buf,
                    if orig == b"/etc/group" {
                        MAX_GROUP_LINE
                    } else {
                        MAX_PASSWD_LINE
                    },
                    false,
                    |_| None::<()>,
                )?;
                return Err(Error(format!("{:?} exceeds {MAX_USER_FILE} bytes", show(orig))));
            }
            Ok(Some(buf))
        }
        Some(Kind::Symlink(_)) => Err(os(PathError {
            op: "open",
            path: orig.to_vec(),
            errno: Errno::Loop,
        })),
        _ => Err(Error(format!("open {}: not a regular file", show(orig)))),
    }
}

/// moby/sys/user's Parse*Filter: lines as bufio.Scanner splits them, trimmed, blank ones
/// (and, for groups, comments) skipped, fields split at `:`; the first line `pick` takes.
fn scan<T>(
    file: &[u8],
    max_line: usize,
    comments: bool,
    mut pick: impl FnMut(&[&[u8]]) -> Option<T>,
) -> Result<Option<T>, Error> {
    let mut rest = file;
    while !rest.is_empty() {
        let (line, next) = match rest.iter().position(|&c| c == b'\n') {
            Some(i) => (
                rest.get(..i).unwrap_or_default(),
                rest.get(i + 1..).unwrap_or_default(),
            ),
            None => (rest, &[][..]),
        };
        if line.len() >= max_line {
            return Err(Error("bufio.Scanner: token too long".into()));
        }
        rest = next;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let line = go::trim_space(line);
        if line.is_empty() || (comments && line.first() == Some(&b'#')) {
            continue;
        }
        let fields: Vec<&[u8]> = line.split(|&c| c == b':').collect();
        if let Some(found) = pick(&fields) {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// `strconv.Atoi` with its error ignored, as moby/sys/user parses IDs, then cut to 32
/// bits as Linux's chown takes them.
fn atoi(s: &[u8]) -> u32 {
    let (neg, digits) = match s.first() {
        Some(b'-') => (true, s.get(1..).unwrap_or_default()),
        Some(b'+') => (false, s.get(1..).unwrap_or_default()),
        _ => (false, s),
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return 0;
    }
    let mut v: i64 = 0;
    for &c in digits {
        let d = i64::from(c - b'0');
        v = match v
            .checked_mul(10)
            .and_then(|v| if neg { v.checked_sub(d) } else { v.checked_add(d) })
        {
            Some(v) => v,
            None => return if neg { i64::MIN as u32 } else { i64::MAX as u32 },
        };
    }
    v as u32
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_split_as_go_time_unix_does() {
        assert_eq!(timestamp(-1), None);
        assert_eq!(timestamp(1_500_000_000_123), Some((1500, 123)));
        assert_eq!(timestamp(-2), Some((-1, 999_999_998)));
    }

    #[test]
    fn paths_clean_as_buildkit_cleans_them() {
        assert_eq!(clean_path(b"a/b"), b"/a/b");
        assert_eq!(clean_path(b"a/b/"), b"/a/b/");
        assert_eq!(clean_path(b"a/b/."), b"/a/b/.");
        assert_eq!(clean_path(b"/."), b"/.");
        assert_eq!(clean_path(b"/"), b"/");
    }

    #[test]
    fn ids_parse_as_atoi_and_chown_take_them() {
        assert_eq!(atoi(b"1000"), 1000);
        assert_eq!(atoi(b"x"), 0);
        assert_eq!(atoi(b""), 0);
        assert_eq!(atoi(b"-1"), u32::MAX);
        assert_eq!(atoi(b"99999999999999999999"), u32::MAX);
    }
}
