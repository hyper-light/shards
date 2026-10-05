//! go-archive's UnpackLayer (diff.go), through ApplyUncompressedLayer: an image layer, or a
//! container's saved writable layer, applied over a tree. Its entries are unpacked as
//! [`crate::unpack`] unpacks them, but whiteouts are applied rather than written: `.wh.NAME`
//! deletes NAME, `.wh..wh..opq` deletes whatever its directory held that this layer did
//! not put there, and AUFS's `.wh..wh.plnk` hard link targets are resolved and dropped.
//!
//! ApplyUncompressedLayer clears the process's umask while it runs, so modes are exactly
//! the layer's; here the one mode the umask would touch, that of directories a layer
//! implies, is set explicitly, and the process's umask is left alone. UnpackLayer reads
//! neither ExcludePatterns, NoOverwriteDirNonDir nor WhiteoutFormat, so neither does this.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, quote};
use crate::gopath::{self, NATIVE, posix};
use crate::sys::{self, Root};
use crate::tar::{Header, Reader, TYPE_DIR, TYPE_LINK, TYPE_REG};
use crate::unpack::{
    UnpackOptions, bound, create, implied_directories, latest, resolve_archive_path, resolve_fs_root_path,
    trim_slashes, unrepresentable,
};
use crate::whiteout::{WHITEOUT_LINK_DIR, WHITEOUT_META_PREFIX, WHITEOUT_OPAQUE_DIR, WHITEOUT_PREFIX};

/// ApplyUncompressedLayer: the layer read from `input` applied over the directory `dest`.
/// Returns the sum of its headers' sizes, as Go's does.
pub fn apply_layer(input: impl Read, dest: &Path, opts: &UnpackOptions) -> Result<i64, Error> {
    let dest = gopath::clean(NATIVE, &sys::path_bytes(dest));
    let root = Root::open(&dest)?;
    let mut tr = Reader::new(input);
    let mut size: i64 = 0;
    let mut dirs: Vec<(Header, Vec<u8>)> = Vec::new();
    // What this layer wrote, by resolved root-relative path: an opaque whiteout keeps it.
    let mut unpacked: HashSet<Vec<u8>> = HashSet::new();
    let mut plnk = Plnk::default();
    let r = (|| {
        while let Some(mut hdr) = tr.next_header()? {
            size = size.wrapping_add(hdr.size);
            let name = posix::clean(trim_slashes(&hdr.name));
            if name == b"." {
                continue;
            }
            if !gopath::is_local(NATIVE, &name) {
                return Err(Error::breakout(format!(
                    "invalid entry name {}",
                    quote(&hdr.name)
                )));
            }
            hdr.name = name;
            if unrepresentable(&hdr) {
                continue;
            }
            // AUFS metadata: its hard link targets are kept aside, the rest skipped, but
            // for an opaque whiteout of the root.
            if hdr.name.starts_with(WHITEOUT_META_PREFIX) {
                if hdr.name.starts_with(WHITEOUT_LINK_DIR) && hdr.typeflag == TYPE_REG {
                    plnk.keep(&root, &dest, &hdr, &mut tr, opts)?;
                }
                if hdr.name != WHITEOUT_OPAQUE_DIR {
                    continue;
                }
            }
            let dst = resolve_archive_path(&root, &gopath::from_slash(NATIVE, &hdr.name))?;
            implied_directories(&root, &dst, false)?;
            let base = gopath::base(NATIVE, &dst);
            if base.starts_with(WHITEOUT_PREFIX) {
                let dir = gopath::dir(NATIVE, &dst);
                if base == WHITEOUT_OPAQUE_DIR {
                    root.lstat(&dir).map_err(|e| e.error("statat", &dir))?;
                    let abs = resolve_fs_root_path(root.name(), &dir)?.path;
                    hide(&root, &abs, &unpacked)?;
                } else {
                    let original = gopath::join(
                        NATIVE,
                        &[&dir, base.get(WHITEOUT_PREFIX.len()..).unwrap_or_default()],
                    );
                    root.remove_all(&original)
                        .map_err(|e| e.error("RemoveAll", &original))?;
                }
                continue;
            }
            if let Ok(fi) = root.lstat(&dst)
                && (fi.kind != sys::FileKind::Dir || hdr.typeflag != TYPE_DIR)
            {
                root.remove_all(&dst).map_err(|e| e.error("RemoveAll", &dst))?;
            }
            // A hard link into .wh..wh.plnk, which is not unpacked, becomes a copy of the
            // file kept aside.
            if hdr.typeflag == TYPE_LINK && posix::clean(&hdr.linkname).starts_with(WHITEOUT_LINK_DIR) {
                let base = posix::base(&hdr.linkname);
                let (src, mut data) = plnk.open(&base)?;
                create(&root, &dst, &src, &mut data, opts)?;
            } else {
                create(&root, &dst, &hdr, &mut tr, opts)?;
            }
            if hdr.typeflag == TYPE_DIR {
                dirs.push((hdr, dst.clone()));
            }
            unpacked.insert(dst);
        }
        for (hdr, dst) in &dirs {
            let atime = bound(latest(hdr.atime, hdr.mtime));
            root.chtimes(dst, atime, bound(hdr.mtime))
                .map_err(|e| e.error("chtimesat", dst))?;
        }
        Ok(size)
    })();
    plnk.remove(&root);
    r
}

/// The opaque whiteout's walk: under `abs`, whatever this layer did not write is removed;
/// what it wrote is kept, and directories it wrote are walked too.
fn hide(root: &Root, abs: &[u8], unpacked: &HashSet<Vec<u8>>) -> Result<(), Error> {
    let entries = match sys::read_dir(abs) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::path("open", abs, &e)),
    };
    for (name, is_dir) in entries {
        let p = gopath::join(NATIVE, &[abs, &name]);
        let Some(rel) = gopath::rel(NATIVE, root.name(), &p) else {
            return Err(Error::other(format!(
                "Rel: can't make {} relative to {}",
                String::from_utf8_lossy(&p),
                String::from_utf8_lossy(root.name())
            )));
        };
        if !unpacked.contains(&rel) {
            root.remove_all(&rel).map_err(|e| e.error("RemoveAll", &rel))?;
        } else if is_dir {
            hide(root, &p, unpacked)?;
        }
    }
    Ok(())
}

/// AUFS's hard link targets, unpacked into a temporary directory in the destination
/// (go-archive's aufsTempdir) and their headers kept.
#[derive(Debug, Default)]
struct Plnk {
    /// The directory's name in the destination, and its full path.
    dir: Option<(Vec<u8>, Vec<u8>)>,
    headers: Vec<(Vec<u8>, Header)>,
}

impl Plnk {
    fn keep(
        &mut self,
        root: &Root,
        dest: &[u8],
        hdr: &Header,
        data: &mut impl Read,
        opts: &UnpackOptions,
    ) -> Result<(), Error> {
        let base = posix::base(&hdr.name);
        self.headers.retain(|(b, _)| *b != base);
        self.headers.push((base.clone(), hdr.clone()));
        let (_, path) = match &self.dir {
            Some(d) => d.clone(),
            None => {
                let d = make_temp(root, dest)?;
                self.dir = Some(d.clone());
                d
            }
        };
        let aufs = Root::open(&path)?;
        create(&aufs, &base, hdr, data, opts)
    }

    /// The header kept for `base`, and its file.
    fn open(&self, base: &[u8]) -> Result<(Header, std::fs::File), Error> {
        let Some((_, hdr)) = self.headers.iter().find(|(b, _)| b == base) else {
            return Err(Error::other("invalid aufs hardlink"));
        };
        let Some((_, path)) = &self.dir else {
            return Err(Error::other("invalid aufs hardlink"));
        };
        let file_path = gopath::join(NATIVE, &[path, base]);
        let file = std::fs::File::open(sys::os_path_buf(&file_path))
            .map_err(|e| Error::path("open", &file_path, &e))?;
        Ok((hdr.clone(), file))
    }

    /// os.RemoveAll of the temporary directory, its error ignored as Go's defer ignores it.
    fn remove(&self, root: &Root) {
        if let Some((name, _)) = &self.dir {
            let _ = root.remove_all(name);
        }
    }
}

/// os.MkdirTemp(dest, "dockerplnk"): a new directory, mode 0700, named for this process,
/// the time and a counter.
fn make_temp(root: &Root, dest: &[u8]) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    for n in 0..10_000u32 {
        let name = format!("dockerplnk{}{nanos}{n}", std::process::id()).into_bytes();
        match root.mkdir(&name, 0o700) {
            Ok(()) => return Ok((name.clone(), gopath::join(NATIVE, &[dest, &name]))),
            Err(crate::root::WalkError::Os(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.error("mkdirat", &name)),
        }
    }
    Err(Error::other("mkdirtemp: no unused name for dockerplnk"))
}
