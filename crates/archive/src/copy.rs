//! go-archive's copy.go: what `docker cp` makes of its paths on either side. A source
//! ending in `/.` copies a directory's contents, one ending in `/` must be a directory;
//! a destination that exists as a directory receives the source, one that does not is
//! created under that name, and the archive's first name is rebased to match.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;

use crate::error::{Error, Kind};
use crate::gopath::{self, NATIVE, Os};
use crate::pack::{PackOptions, pack};
use crate::sys::{self, FileKind};
use crate::tar::{Format, Reader, TYPE_LINK, Writer};
use crate::unpack::{UnpackOptions, untar};

/// CopyInfo: a copy's source or destination.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyInfo {
    pub path: PathBuf,
    pub exists: bool,
    pub is_dir: bool,
    /// The name the source's entries take instead of its base name, if any.
    pub rebase_name: Vec<u8>,
}

/// ErrNotDirectory.
fn not_directory() -> Error {
    Error::new(Kind::NotDirectory, "not a directory")
}

fn bytes(p: &Path) -> Vec<u8> {
    sys::path_bytes(p)
}

fn path(b: &[u8]) -> PathBuf {
    sys::os_path_buf(b)
}

/// normalizePath: the platform's separators.
fn normalize(p: &[u8]) -> Vec<u8> {
    match NATIVE {
        Os::Unix => gopath::to_slash(Os::Unix, p),
        Os::Windows => gopath::from_slash(Os::Windows, p),
    }
}

/// hasTrailingPathSeparator: the platform's own separator only.
fn has_trailing_sep(os: Os, p: &[u8]) -> bool {
    p.last() == Some(&os.sep())
}

/// specifiesCurrentDir: the last element is `.`.
fn specifies_current_dir(os: Os, p: &[u8]) -> bool {
    gopath::base(os, p) == b"."
}

/// assertsDirectory: a trailing separator or `.`.
fn asserts_directory(os: Os, p: &[u8]) -> bool {
    has_trailing_sep(os, p) || specifies_current_dir(os, p)
}

/// PreserveTrailingDotOrSeparator: `cleaned` with the `/.` or `/` that `original` ended
/// in, which cleaning drops.
pub fn preserve_trailing_dot_or_separator(cleaned: &Path, original: &Path) -> PathBuf {
    path(&preserve(
        NATIVE,
        &normalize(&bytes(cleaned)),
        &normalize(&bytes(original)),
    ))
}

pub(crate) fn preserve(os: Os, cleaned: &[u8], original: &[u8]) -> Vec<u8> {
    let mut cleaned = cleaned.to_vec();
    if !specifies_current_dir(os, &cleaned) && specifies_current_dir(os, original) {
        if !has_trailing_sep(os, &cleaned) {
            cleaned.push(os.sep());
        }
        cleaned.push(b'.');
    }
    if !has_trailing_sep(os, &cleaned) && has_trailing_sep(os, original) {
        cleaned.push(os.sep());
    }
    cleaned
}

/// SplitPathDirEntry: the directory and the entry in it, the path cleaned but a
/// trailing `.` kept.
pub fn split_path_dir_entry(p: &Path) -> (PathBuf, PathBuf) {
    let (dir, base) = split_path_dir_entry_os(NATIVE, &bytes(p));
    (path(&dir), path(&base))
}

pub(crate) fn split_path_dir_entry_os(os: Os, p: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut cleaned = gopath::clean(os, &gopath::from_slash(os, p));
    if specifies_current_dir(os, p) {
        cleaned.push(os.sep());
        cleaned.push(b'.');
    }
    (gopath::dir(os, &cleaned), gopath::base(os, &cleaned))
}

/// TarResource: the archive of a copy's source.
pub fn tar_resource<W: Write>(info: &CopyInfo, out: W) -> Result<W, Error> {
    tar_resource_rebase(&info.path, &info.rebase_name, out)
}

/// TarResourceRebase: the archive of `source`, a file or directory, its first name
/// replaced by `rebase_name` unless that is empty.
pub fn tar_resource_rebase<W: Write>(source: &Path, rebase_name: &[u8], out: W) -> Result<W, Error> {
    let source = normalize(&bytes(source));
    sys::lstat(&source).map_err(|e| Error::path("lstat", &source, &e))?;
    let (dir, base) = split_path_dir_entry_os(NATIVE, &source);
    pack(&path(&dir), &tar_resource_rebase_opts(&base, rebase_name), out)
}

/// TarResourceRebaseOpts: the options that archive `base` alone, renamed.
pub fn tar_resource_rebase_opts(base: &[u8], rebase_name: &[u8]) -> PackOptions {
    PackOptions {
        include_files: vec![base.to_vec()],
        include_source_dir: true,
        rebase_names: [(base.to_vec(), rebase_name.to_vec())].into_iter().collect(),
        ..PackOptions::default()
    }
}

/// CopyInfoSourcePath: a source, which must exist, with the symlinks before its last
/// element resolved (and that one too when `follow`).
pub fn copy_info_source_path(p: &Path, follow: bool) -> Result<CopyInfo, Error> {
    let p = normalize(&bytes(p));
    let (resolved, rebase_name) = resolve_host_source_path_os(&p, follow)?;
    let st = sys::lstat(&resolved).map_err(|e| Error::path("lstat", &resolved, &e))?;
    Ok(CopyInfo {
        path: path(&resolved),
        exists: true,
        is_dir: st.kind == FileKind::Dir,
        rebase_name,
    })
}

/// CopyInfoDestinationPath: a destination, its symlinks followed (at most 10); a missing
/// one must have a directory for a parent.
pub fn copy_info_destination_path(p: &Path) -> Result<CopyInfo, Error> {
    let mut p = normalize(&bytes(p));
    let original = p.clone();
    let mut st = sys::lstat(&p);
    if let Ok(s) = &st
        && s.kind != FileKind::Symlink
    {
        return Ok(CopyInfo {
            path: path(&p),
            exists: true,
            is_dir: s.kind == FileKind::Dir,
            rebase_name: Vec::new(),
        });
    }
    let mut n = 0;
    while let Ok(s) = &st {
        if s.kind != FileKind::Symlink {
            break;
        }
        if n > 10 {
            return Err(Error::other(format!(
                "too many symlinks in {}",
                String::from_utf8_lossy(&original)
            )));
        }
        let mut target = sys::readlink(&p).map_err(|e| Error::path("readlink", &p, &e))?;
        if !gopath::is_abs(NATIVE, &target) {
            let (parent, _) = split_path_dir_entry_os(NATIVE, &p);
            target = gopath::join(NATIVE, &[&parent, &target]);
        }
        p = target;
        st = sys::lstat(&p);
        n += 1;
    }
    match st {
        Ok(s) => Ok(CopyInfo {
            path: path(&p),
            exists: true,
            is_dir: s.kind == FileKind::Dir,
            rebase_name: Vec::new(),
        }),
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(Error::path("lstat", &p, &e)),
        Err(_) => {
            let (parent, _) = split_path_dir_entry_os(NATIVE, &p);
            let ps = sys::stat(&parent).map_err(|e| Error::path("stat", &parent, &e))?;
            if ps.kind != FileKind::Dir {
                return Err(not_directory());
            }
            Ok(CopyInfo {
                path: path(&p),
                ..CopyInfo::default()
            })
        }
    }
}

/// What PrepareArchiveCopy decides: where to unpack, and the rebase the archive's names
/// need on the way, from the source's base name to the destination's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub dst_dir: PathBuf,
    pub rebase: Option<(Vec<u8>, Vec<u8>)>,
}

/// PrepareArchiveCopy.
pub fn prepare_archive_copy(src: &CopyInfo, dst: &CopyInfo) -> Result<Prepared, Error> {
    let src_path = normalize(&bytes(&src.path));
    let dst_path = normalize(&bytes(&dst.path));
    let (dst_dir, dst_base) = split_path_dir_entry_os(NATIVE, &dst_path);
    let (_, mut src_base) = split_path_dir_entry_os(NATIVE, &src_path);
    if !src.rebase_name.is_empty() {
        src_base = src.rebase_name.clone();
    }
    let rebased = || Prepared {
        dst_dir: path(&dst_dir),
        rebase: Some((src_base.clone(), dst_base.clone())),
    };
    if dst.exists && dst.is_dir {
        return Ok(Prepared {
            dst_dir: path(&dst_path),
            rebase: None,
        });
    }
    if dst.exists && src.is_dir {
        return Err(Error::new(Kind::CannotCopyDir, "cannot copy directory"));
    }
    if dst.exists || src.is_dir {
        return Ok(rebased());
    }
    if asserts_directory(NATIVE, &dst_path) {
        return Err(Error::new(Kind::DirNotExists, "no such directory"));
    }
    Ok(rebased())
}

/// newNameRebaser: `old` at the start of a name, as a whole element, becomes `new`; from
/// the root, every name is put under `new`.
fn rebaser(old: &[u8], new: &[u8]) -> impl Fn(&[u8]) -> Vec<u8> {
    let trim = |s: &[u8]| {
        let s = gopath::to_slash(NATIVE, s);
        let end = s.iter().rposition(|&c| c != b'/').map_or(0, |i| i + 1);
        s.get(..end).unwrap_or_default().to_vec()
    };
    let (old, new) = (trim(old), trim(new));
    move |name: &[u8]| {
        if old.is_empty() {
            let start = name.iter().position(|&c| c != b'/').unwrap_or(name.len());
            let name = name.get(start..).unwrap_or_default();
            if new.is_empty() {
                return name.to_vec();
            }
            return [new.as_slice(), b"/", name].concat();
        }
        match name.strip_prefix(old.as_slice()) {
            Some(suffix) if suffix.is_empty() || suffix.starts_with(b"/") => {
                [new.as_slice(), suffix].concat()
            }
            _ => name.to_vec(),
        }
    }
}

/// RebaseArchiveEntries: the archive from `input` written to `out`, `old_base` replaced
/// by `new_base` at the start of each name and hard link, every header PAX.
pub fn rebase_archive_entries<R: Read, W: Write>(
    input: R,
    out: W,
    old_base: &[u8],
    new_base: &[u8],
) -> Result<W, Error> {
    let rebase = rebaser(old_base, new_base);
    let mut tr = Reader::new(input);
    let mut tw = Writer::new(out);
    while let Some(mut hdr) = tr.next_header()? {
        hdr.format = Format::PAX;
        hdr.name = rebase(&hdr.name);
        if hdr.typeflag == TYPE_LINK {
            hdr.linkname = rebase(&hdr.linkname);
        }
        tw.write_header(&hdr)?;
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            let n = match tr.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(crate::tar::read_error(&e)),
            };
            tw.write_all(buf.get(..n).unwrap_or_default())?;
        }
    }
    tw.finish()
}

/// CopyTo: an archive of the source `src` describes, unpacked at `dst` as `docker cp`
/// unpacks it: owners not applied, and no directory replaced by a file or the reverse.
pub fn copy_to(content: impl Read + Send, src: &CopyInfo, dst: &Path) -> Result<(), Error> {
    let dst_info = copy_info_destination_path(&path(&normalize(&bytes(dst))))?;
    let prepared = prepare_archive_copy(src, &dst_info)?;
    let opts = UnpackOptions {
        no_lchown: true,
        no_overwrite_dir_non_dir: true,
        ..UnpackOptions::default()
    };
    let Some((old, new)) = prepared.rebase else {
        return untar(content, &prepared.dst_dir, &opts);
    };
    let (r, w) = io::pipe()?;
    thread::scope(|s| {
        let rebasing = thread::Builder::new()
            .name("rebase".into())
            .spawn_scoped(s, move || {
                rebase_archive_entries(content, w, &old, &new).map(drop)
            })
            .map_err(|e| Error::io(&e))?;
        let unpacked = untar(r, &prepared.dst_dir, &opts);
        let rebased = rebasing
            .join()
            .unwrap_or_else(|_| Err(Error::other("rebasing the archive failed")));
        piped(rebased, unpacked)
    })
}

/// The error of a pipe's two ends: the reader's, unless it only saw the writer stop.
fn piped(writer: Result<(), Error>, reader: Result<(), Error>) -> Result<(), Error> {
    match (writer, reader) {
        (Err(w), Err(r)) if r.to_string() == "unexpected EOF" => Err(w),
        (_, Err(r)) => Err(r),
        (Err(w), Ok(())) => Err(w),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// CopyResource: `src` copied to `dst` through an archive, as `docker cp` copies.
pub fn copy_resource(src: &Path, dst: &Path, follow: bool) -> Result<(), Error> {
    let src = normalize(&bytes(src));
    let dst = normalize(&bytes(dst));
    let src = preserve(NATIVE, &gopath::clean(NATIVE, &src), &src);
    let dst = preserve(NATIVE, &gopath::clean(NATIVE, &dst), &dst);
    let info = copy_info_source_path(&path(&src), follow)?;
    let (r, w) = io::pipe()?;
    thread::scope(|s| {
        let packing = thread::Builder::new()
            .name("tar".into())
            .spawn_scoped(s, || tar_resource(&info, w).map(drop))
            .map_err(|e| Error::io(&e))?;
        let copied = copy_to(r, &info, &path(&dst));
        let packed = packing
            .join()
            .unwrap_or_else(|_| Err(Error::other("archiving the source failed")));
        piped(packed, copied)
    })
}

/// ResolveHostSourcePath: the path with its parent's symlinks resolved, and its own
/// when `follow`, and the name its archive's entries take if resolving changed it.
pub fn resolve_host_source_path(p: &Path, follow: bool) -> Result<(PathBuf, Vec<u8>), Error> {
    let (resolved, rebase) = resolve_host_source_path_os(&bytes(p), follow)?;
    Ok((path(&resolved), rebase))
}

fn resolve_host_source_path_os(p: &[u8], follow: bool) -> Result<(Vec<u8>, Vec<u8>), Error> {
    if follow {
        let resolved = eval_symlinks(p)?;
        return Ok(rebase_name(NATIVE, p, &resolved));
    }
    let (dir, base) = gopath::split(NATIVE, p);
    let resolved_dir = eval_symlinks(dir)?;
    let resolved = [resolved_dir.as_slice(), &[NATIVE.sep()], base].concat();
    let mut rebase = Vec::new();
    if has_trailing_sep(NATIVE, p) && gopath::base(NATIVE, p) != gopath::base(NATIVE, &resolved) {
        rebase = gopath::base(NATIVE, p);
    }
    Ok((resolved, rebase))
}

/// GetRebaseName: `resolved` with `path`'s trailing `/.` or `/`, and `path`'s base name if
/// resolving changed it.
pub fn get_rebase_name(p: &Path, resolved: &Path) -> (PathBuf, Vec<u8>) {
    let (r, name) = rebase_name(NATIVE, &bytes(p), &bytes(resolved));
    (path(&r), name)
}

pub(crate) fn rebase_name(os: Os, p: &[u8], resolved: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut resolved = resolved.to_vec();
    if specifies_current_dir(os, p) && !specifies_current_dir(os, &resolved) {
        resolved.push(os.sep());
        resolved.push(b'.');
    }
    if has_trailing_sep(os, p) && !has_trailing_sep(os, &resolved) {
        resolved.push(os.sep());
    }
    let mut name = Vec::new();
    if gopath::base(os, p) != gopath::base(os, &resolved) {
        name = gopath::base(os, p);
    }
    (resolved, name)
}

/// filepath.EvalSymlinks (go1.26.1 src/path/filepath/symlink.go, walkSymlinks): every
/// symlink in `path` resolved, at most 255; a relative path stays relative.
fn eval_symlinks(path: &[u8]) -> Result<Vec<u8>, Error> {
    let os = NATIVE;
    let mut path = path.to_vec();
    let mut vol_len = gopath::volume_name_len(os, &path);
    if vol_len < path.len() && path.get(vol_len).is_some_and(|&c| os.is_sep(c)) {
        vol_len += 1;
    }
    let mut vol = path.get(..vol_len).unwrap_or_default().to_vec();
    let mut dest = vol.clone();
    let mut links = 0;
    let mut start = vol_len;
    while start < path.len() {
        while path.get(start).is_some_and(|&c| os.is_sep(c)) {
            start += 1;
        }
        let mut end = start;
        while end < path.len() && !path.get(end).is_some_and(|&c| os.is_sep(c)) {
            end += 1;
        }
        let windows_dot =
            os == Os::Windows && path.get(gopath::volume_name_len(os, &path)..) == Some(b".".as_slice());
        let elem = path.get(start..end).unwrap_or_default().to_vec();
        if end == start {
            break;
        } else if elem == b"." && !windows_dot {
            start = end;
            continue;
        } else if elem == b".." {
            let mut r = dest.len();
            while r > vol_len && !dest.get(r - 1).is_some_and(|&c| os.is_sep(c)) {
                r -= 1;
            }
            let r = r.checked_sub(1);
            match r {
                Some(r) if r >= vol_len && dest.get(r + 1..) != Some(b"..".as_slice()) => dest.truncate(r),
                _ => {
                    if dest.len() > vol_len {
                        dest.push(os.sep());
                    }
                    dest.extend_from_slice(b"..");
                }
            }
            start = end;
            continue;
        }
        if dest.len() > gopath::volume_name_len(os, &dest) && !dest.last().is_some_and(|&c| os.is_sep(c)) {
            dest.push(os.sep());
        }
        dest.extend_from_slice(&elem);
        let st = sys::lstat(&dest).map_err(|e| Error::path("lstat", &dest, &e))?;
        if st.kind != FileKind::Symlink {
            if st.kind != FileKind::Dir && end < path.len() {
                return Err(Error::new(Kind::NotDirectory, "not a directory"));
            }
            start = end;
            continue;
        }
        links += 1;
        if links > 255 {
            return Err(Error::other("EvalSymlinks: too many links"));
        }
        let link = sys::readlink(&dest).map_err(|e| Error::path("readlink", &dest, &e))?;
        if windows_dot && !gopath::is_abs(os, &link) {
            break;
        }
        path = [link.as_slice(), path.get(end..).unwrap_or_default()].concat();
        let v = gopath::volume_name_len(os, &link);
        if v > 0 {
            let v = if v < link.len() && link.get(v).is_some_and(|&c| os.is_sep(c)) {
                v + 1
            } else {
                v
            };
            vol = link.get(..v).unwrap_or_default().to_vec();
            dest = vol.clone();
            start = vol.len();
        } else if link.first().is_some_and(|&c| os.is_sep(c)) {
            dest = link.get(..1).unwrap_or_default().to_vec();
            vol = dest.clone();
            vol_len = 1;
            start = 1;
        } else {
            let mut r = dest.len();
            while r > vol_len && !dest.get(r - 1).is_some_and(|&c| os.is_sep(c)) {
                r -= 1;
            }
            match r.checked_sub(1) {
                Some(r) if r >= vol_len => dest.truncate(r),
                _ => dest = vol.clone(),
            }
            start = 0;
        }
    }
    Ok(gopath::clean(os, &dest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: Vec<u8>) -> String {
        String::from_utf8(v).unwrap()
    }

    #[test]
    fn trailing_dot_and_separator_kept() {
        for (cleaned, original, want) in [
            ("/a", "/a/", "/a/"),
            ("/a", "/a/.", "/a/."),
            ("/a", "/a", "/a"),
            ("/", "/.", "/."),
            ("/", "//", "/"),
            ("a", "a/./", "a/./"),
        ] {
            assert_eq!(
                s(preserve(Os::Unix, cleaned.as_bytes(), original.as_bytes())),
                want
            );
        }
        assert_eq!(s(preserve(Os::Windows, br"C:\a", br"C:\a\.")), r"C:\a\.");
    }

    #[test]
    fn splits_keep_a_trailing_dot() {
        for (p, dir, base) in [
            ("/a/b", "/a", "b"),
            ("/a/b/", "/a", "b"),
            ("/a/b/.", "/a/b", "."),
            ("a", ".", "a"),
            ("/", "/", "/"),
            ("/.", "/", "."),
        ] {
            let (d, b) = split_path_dir_entry_os(Os::Unix, p.as_bytes());
            assert_eq!((s(d).as_str(), s(b).as_str()), (dir, base), "{p}");
        }
    }

    #[test]
    fn rebase_names_as_go() {
        // go-archive v0.3.3 copy_test.go, TestRebaseArchiveEntriesPlatformPaths.
        let r = rebaser(b"dir/", b"newdir/");
        assert_eq!(s(r(b"dir")), "newdir");
        assert_eq!(s(r(b"dir/")), "newdir/");
        assert_eq!(s(r(b"dir/file")), "newdir/file");
        assert_eq!(s(r(b"dirx/file")), "dirx/file");
        let r = rebase_name(Os::Unix, b"/a/link/", b"/a/target");
        assert_eq!((s(r.0), s(r.1)), ("/a/target/".into(), "link".into()));
        let r = rebaser(b"", b"x");
        assert_eq!(s(r(b"/a/b")), "x/a/b");
    }
}
