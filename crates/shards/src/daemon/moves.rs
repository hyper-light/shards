//! A microVM moved to another machine (D176 part 6): `shards move vm NAME CONTEXT`, whose
//! client asks this daemon and the other machine's, through its context (D180), for its
//! steps one at a time (cli/moves.rs), as `shards cp` asks for its own.
//!
//! - `save NAME`: the microVM saved whole and stopped (D176), or the snapshot a save
//!   stopped it as; its [`Plan`], on stdout: what the other machine needs of it.
//! - `ready PLAN`, there: whether the move can be taken, and what it lacks ([`Needs`]): its
//!   image, by the digest it was made of, found there or pulled; the files its snapshot
//!   restores against, each byte for byte as here, else sent.
//! - `export NAME SNAPSHOT [FILE...]`: its directory, its snapshot, its volumes and the
//!   files the other machine lacks, written to the client's descriptor ([`Writer`]).
//! - `import`, there: all of it read from the client's descriptor, put in place, its
//!   snapshot made to restore against that machine's files, and the microVM made there,
//!   stopped, as saved; `restore vm` resumes it.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::commands::{Asker, Reply};
use super::snapshots::SNAPSHOTS;
use super::{Daemon, lock};
use crate::containers::{Container, Disk};

/// The first word of `shards move vm`'s steps, which no command line has (cli/moves.rs).
pub(crate) use shards_ipc::MOVE_STEP;

/// The stream's first bytes, then its version.
const MAGIC: &[u8; 8] = b"SHRDMOVE";
const VERSION: u32 = 1;
/// Kinds of the stream's entries.
const END: u8 = 0;
const DIR: u8 = 1;
const FILE: u8 = 2;
const LINK: u8 = 3;
const PLAN: u8 = 4;
/// The most a name in the stream may hold, a plan's JSON, and a file's extended attributes.
const MAX_PATH: usize = 4096;
const MAX_PLAN: usize = 16 << 20;
const MAX_XATTRS: usize = 1 << 16;
const MAX_XATTR: usize = 64 << 10;
/// How much of a file a read or write moves at once.
const CHUNK: usize = 1 << 20;
/// Where a moved microVM's files that its new machine had none of, or another of, are kept
/// in its directory: what its snapshot restores against there.
const BACKING: &str = "backing";

/// What a machine needs to take a microVM moved to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Plan {
    /// Its ID and name, which it keeps.
    pub id: String,
    pub name: String,
    /// Its image as its run named it, and the digest that name resolved to.
    pub image: String,
    pub image_id: String,
    /// Its guest's architecture, and the snapshot format it was saved in.
    pub arch: String,
    pub format: u32,
    /// The snapshot it moves as.
    pub snapshot: String,
    /// This machine's home, resolved: what each file's name in the move is relative to.
    pub home: String,
    /// The files its snapshot restores against, each relative to the home.
    pub files: Vec<Backing>,
    /// The named volumes it mounts, which move with it.
    pub volumes: Vec<String>,
    /// The host directories and files it binds, which the other machine must share with
    /// this one at the same paths: a network file system's, served there as here.
    pub binds: Vec<String>,
}

/// A file a snapshot restores against, by its name relative to the home, its length and
/// its bytes' SHA-256.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Backing {
    /// As its snapshot names it, here.
    pub path: String,
    pub rel: String,
    pub size: u64,
    pub sha256: String,
}

/// What a machine lacks to take a [`Plan`]: its image, which the client loads there, and the
/// files the export is to send.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Needs {
    pub image: bool,
    pub files: Vec<String>,
}

/// `bytes`' SHA-256, in hex.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The SHA-256 of `path`'s bytes, in hex, and its length.
fn digest_of(path: &Path) -> io::Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut len = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(buf.get(..n).unwrap_or_default());
        len = len.saturating_add(n as u64);
    }
    Ok((hex(&hasher.finalize()), len))
}

/// `rel` as a name relative to a home: `/`-separated normal components alone, none of them
/// `.`, `..`, empty or holding a NUL. Anything else is refused, as it would reach out of
/// the home.
fn relative(rel: &str) -> Result<PathBuf, String> {
    let refuse = || format!("{rel:?} names no file in a home");
    if rel.is_empty() || rel.starts_with('/') || rel.contains(['\0', '\\']) {
        return Err(refuse());
    }
    let mut out = PathBuf::new();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(refuse());
        }
        out.push(part);
    }
    match out.components().all(|c| matches!(c, Component::Normal(_))) {
        true => Ok(out),
        false => Err(refuse()),
    }
}

/// `path` relative to `home`, `/`-separated, where it is in it.
fn rel_of(home: &Path, path: &Path) -> Option<String> {
    let rest = path.strip_prefix(home).ok()?;
    let parts: Option<Vec<&str>> = rest
        .components()
        .map(|c| match c {
            Component::Normal(p) => p.to_str(),
            _ => None,
        })
        .collect();
    let parts = parts?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// A stream of a home's files, written by [`Writer`] and read by [`read_plan`] and [`read_entries`]: a magic,
/// a version, the plan, then entries, each its kind, its name relative to the home and what
/// it is, and an end. A file is its mode, its extended attributes, its length and the runs
/// of it that hold data, so that a sparse file (a snapshot's memory, a writable layer) stays
/// as sparse; nothing past its length or outside its runs is read.
pub(crate) struct Writer<W: Write> {
    out: W,
}

impl<W: Write> Writer<W> {
    pub(crate) fn new(mut out: W, plan: &Plan) -> io::Result<Writer<W>> {
        out.write_all(MAGIC)?;
        out.write_all(&VERSION.to_le_bytes())?;
        let json = serde_json::to_vec(plan).map_err(io::Error::other)?;
        out.write_all(&[PLAN])?;
        out.write_all(&(json.len() as u64).to_le_bytes())?;
        out.write_all(&json)?;
        Ok(Writer { out })
    }

    fn name(&mut self, kind: u8, rel: &str) -> io::Result<()> {
        self.out.write_all(&[kind])?;
        self.out.write_all(&(rel.len() as u32).to_le_bytes())?;
        self.out.write_all(rel.as_bytes())
    }

    fn mode(&mut self, meta: &std::fs::Metadata) -> io::Result<()> {
        self.out.write_all(&mode_of(meta).to_le_bytes())
    }

    fn xattrs(&mut self, list: &[(Vec<u8>, Vec<u8>)]) -> io::Result<()> {
        self.out.write_all(&(list.len() as u32).to_le_bytes())?;
        for (name, value) in list {
            self.out.write_all(&(name.len() as u32).to_le_bytes())?;
            self.out.write_all(name)?;
            self.out.write_all(&(value.len() as u32).to_le_bytes())?;
            self.out.write_all(value)?;
        }
        Ok(())
    }

    /// The tree at `path`, named `rel` in the stream: a directory and all it holds, a file
    /// or a symlink. `skip` leaves out the entries of a directory it names by `rel`.
    pub(crate) fn tree(&mut self, path: &Path, rel: &str, skip: &dyn Fn(&str) -> bool) -> io::Result<()> {
        let meta = std::fs::symlink_metadata(path)?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            let target = std::fs::read_link(path)?;
            let target = target
                .to_str()
                .ok_or_else(|| io::Error::other(format!("{}: a link that is not UTF-8", path.display())))?;
            self.name(LINK, rel)?;
            self.out.write_all(&(target.len() as u32).to_le_bytes())?;
            return self.out.write_all(target.as_bytes());
        }
        if kind.is_dir() {
            self.name(DIR, rel)?;
            self.mode(&meta)?;
            self.xattrs(&xattrs::list(path)?)?;
            let mut entries: Vec<(String, PathBuf)> = std::fs::read_dir(path)?
                .map(|e| {
                    let e = e?;
                    let name = e
                        .file_name()
                        .into_string()
                        .map_err(|n| io::Error::other(format!("{n:?}: a name that is not UTF-8")))?;
                    Ok((name, e.path()))
                })
                .collect::<io::Result<_>>()?;
            entries.sort();
            for (name, at) in entries {
                let child = format!("{rel}/{name}");
                if !skip(&child) {
                    self.tree(&at, &child, skip)?;
                }
            }
            return Ok(());
        }
        // A socket or a pipe is a running microVM's, which a saved one has none of.
        if !kind.is_file() {
            return Ok(());
        }
        self.file(path, rel)
    }

    /// The file at `path`, named `rel`: its mode, attributes, length and the runs of it
    /// that hold data.
    pub(crate) fn file(&mut self, path: &Path, rel: &str) -> io::Result<()> {
        let file = File::open(path)?;
        let meta = file.metadata()?;
        let len = meta.len();
        let runs = shards_vmm::platform::data_ranges(&file, len)?;
        self.name(FILE, rel)?;
        self.mode(&meta)?;
        self.xattrs(&xattrs::list(path)?)?;
        self.out.write_all(&len.to_le_bytes())?;
        self.out.write_all(&(runs.len() as u64).to_le_bytes())?;
        let mut buf = vec![0u8; CHUNK];
        for (start, n) in runs {
            let n = n.min(len.saturating_sub(start));
            self.out.write_all(&start.to_le_bytes())?;
            self.out.write_all(&n.to_le_bytes())?;
            let mut at = start;
            let end = start.saturating_add(n);
            while at < end {
                let want = usize::try_from(end - at).map_or(CHUNK, |rest| rest.min(CHUNK));
                let chunk = buf.get_mut(..want).ok_or(io::ErrorKind::InvalidInput)?;
                shards_vmm::platform::read_exact_at(&file, chunk, at)?;
                self.out.write_all(chunk)?;
                at = at.saturating_add(want as u64);
            }
        }
        Ok(())
    }

    /// The stream's end: what reads it knows it whole.
    pub(crate) fn finish(mut self) -> io::Result<()> {
        self.out.write_all(&[END])?;
        self.out.flush()
    }
}

/// A file's mode as the stream keeps it: its permission bits.
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        meta.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        if meta.permissions().readonly() { 0o444 } else { 0o644 }
    }
}

/// An entry of a stream, as [`read_entries`] gives it to its taker.
pub(crate) enum Entry<'a> {
    Dir {
        rel: String,
        mode: u32,
        xattrs: Vec<(Vec<u8>, Vec<u8>)>,
    },
    File {
        rel: String,
        mode: u32,
        xattrs: Vec<(Vec<u8>, Vec<u8>)>,
        len: u64,
        /// Its runs of data, each read into `put` at its offset.
        runs: &'a mut dyn FnMut(&mut dyn FnMut(u64, &[u8]) -> io::Result<()>) -> io::Result<()>,
    },
    Link {
        rel: String,
        target: String,
    },
}

fn u32_of(r: &mut dyn Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn u64_of(r: &mut dyn Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// `n` bytes of `r`, at most `max`: a length past it is refused before anything is
/// allocated.
fn bytes_of(r: &mut dyn Read, n: u64, max: usize, what: &str) -> io::Result<Vec<u8>> {
    let n = usize::try_from(n)
        .ok()
        .filter(|n| *n <= max)
        .ok_or_else(|| io::Error::other(format!("{what}: {n} bytes, more than a move holds")))?;
    let mut out = vec![0u8; n];
    r.read_exact(&mut out)?;
    Ok(out)
}

fn text_of(r: &mut dyn Read, max: usize, what: &str) -> io::Result<String> {
    let n = u32_of(r)?;
    String::from_utf8(bytes_of(r, u64::from(n), max, what)?)
        .map_err(|_| io::Error::other(format!("{what}: not UTF-8")))
}

fn xattrs_of(r: &mut dyn Read) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let n = u32_of(r)? as usize;
    if n > MAX_XATTRS {
        return Err(io::Error::other(format!("{n} extended attributes")));
    }
    let mut out = Vec::with_capacity(n.min(64));
    for _ in 0..n {
        let name_len = u32_of(r)?;
        let name = bytes_of(r, u64::from(name_len), 255, "an attribute's name")?;
        let value_len = u32_of(r)?;
        let value = bytes_of(r, u64::from(value_len), MAX_XATTR, "an attribute's value")?;
        out.push((name, value));
    }
    Ok(out)
}

/// Reads the head of a stream [`Writer`] wrote from `r`: its magic, version and plan.
pub(crate) fn read_plan(r: &mut dyn Read) -> Result<Plan, String> {
    let io = |e: io::Error| format!("the move's stream: {e}");
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic).map_err(io)?;
    if &magic != MAGIC {
        return Err("the move's stream is not one".into());
    }
    let version = u32_of(r).map_err(io)?;
    if version != VERSION {
        return Err(format!(
            "the move's stream is of version {version}; this shards reads {VERSION}"
        ));
    }
    let mut kind = [0u8; 1];
    r.read_exact(&mut kind).map_err(io)?;
    if kind[0] != PLAN {
        return Err("the move's stream has no plan".into());
    }
    let n = u64_of(r).map_err(io)?;
    serde_json::from_slice(&bytes_of(r, n, MAX_PLAN, "its plan").map_err(io)?)
        .map_err(|e| format!("the move's plan: {e}"))
}

/// Reads the entries of a stream after its plan ([`read_plan`]) from `r`, each given to
/// `take` in order, a file's runs read as `take` asks for them. One that ends before its
/// end, or holds what no writer writes, is refused.
pub(crate) fn read_entries(r: &mut dyn Read, take: &mut dyn FnMut(Entry<'_>) -> Result<(), String>) -> Result<(), String> {
    let io = |e: io::Error| format!("the move's stream: {e}");
    let mut kind = [0u8; 1];
    loop {
        r.read_exact(&mut kind).map_err(io)?;
        match kind[0] {
            END => return Ok(()),
            DIR => {
                let rel = text_of(r, MAX_PATH, "a name").map_err(io)?;
                let mode = u32_of(r).map_err(io)?;
                let xattrs = xattrs_of(r).map_err(io)?;
                take(Entry::Dir { rel, mode, xattrs })?;
            }
            LINK => {
                let rel = text_of(r, MAX_PATH, "a name").map_err(io)?;
                let target = text_of(r, MAX_PATH, "a link").map_err(io)?;
                take(Entry::Link { rel, target })?;
            }
            FILE => {
                let rel = text_of(r, MAX_PATH, "a name").map_err(io)?;
                let mode = u32_of(r).map_err(io)?;
                let xattrs = xattrs_of(r).map_err(io)?;
                let len = u64_of(r).map_err(io)?;
                let count = u64_of(r).map_err(io)?;
                let mut read_all = false;
                let mut runs = |put: &mut dyn FnMut(u64, &[u8]) -> io::Result<()>| -> io::Result<()> {
                    let mut buf = vec![0u8; CHUNK];
                    let mut last = 0u64;
                    for _ in 0..count {
                        let start = u64_of(r)?;
                        let n = u64_of(r)?;
                        let end = start
                            .checked_add(n)
                            .filter(|end| *end <= len && start >= last)
                            .ok_or_else(|| io::Error::other("a run of data out of its file"))?;
                        last = end;
                        let mut at = start;
                        while at < end {
                            let want = usize::try_from(end - at).map_or(CHUNK, |rest| rest.min(CHUNK));
                            let chunk = buf.get_mut(..want).ok_or(io::ErrorKind::InvalidInput)?;
                            r.read_exact(chunk)?;
                            put(at, chunk)?;
                            at = at.saturating_add(want as u64);
                        }
                    }
                    read_all = true;
                    Ok(())
                };
                take(Entry::File {
                    rel,
                    mode,
                    xattrs,
                    len,
                    runs: &mut runs,
                })?;
                if !read_all {
                    return Err("a file's data was left unread".into());
                }
            }
            other => return Err(format!("the move's stream holds an entry of kind {other}")),
        }
    }
}

/// Extended attributes, which a volume's server keeps its guest's owners in (D38): moved
/// with each file, as a share's files hold them.
mod xattrs {
    use std::io;
    use std::path::Path;

    /// What getxattr(2) says of an attribute that is not there.
    #[cfg(target_os = "macos")]
    const NO_ATTR: i32 = libc::ENOATTR;
    #[cfg(all(unix, not(target_os = "macos")))]
    const NO_ATTR: i32 = libc::ENODATA;

    /// `path`'s own attributes, a link's never followed: each name and value.
    #[cfg(unix)]
    pub(super) fn list(path: &Path) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        use std::os::unix::ffi::OsStrExt as _;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
        let mut names = vec![0u8; 4096];
        let n = loop {
            // SAFETY: llistxattr(2) writes at most `names.len()` bytes into it.
            let n = unsafe { sys::list(c.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
            if n >= 0 {
                break n as usize;
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::ERANGE) if names.len() < (1 << 20) => names.resize(names.len() * 4, 0),
                Some(libc::ENOTSUP) => return Ok(Vec::new()),
                _ => return Err(e),
            }
        };
        let mut out = Vec::new();
        for name in names.get(..n).unwrap_or_default().split(|&b| b == 0) {
            if name.is_empty() {
                continue;
            }
            let cname = std::ffi::CString::new(name).map_err(io::Error::other)?;
            let mut value = vec![0u8; 1024];
            let got = loop {
                // SAFETY: lgetxattr(2) writes at most `value.len()` bytes into it.
                let got = unsafe {
                    sys::get(c.as_ptr(), cname.as_ptr(), value.as_mut_ptr().cast(), value.len())
                };
                if got >= 0 {
                    break Some(got as usize);
                }
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::ERANGE) if value.len() < super::MAX_XATTR => value.resize(super::MAX_XATTR, 0),
                    // Gone as it was listed.
                    Some(NO_ATTR) => break None,
                    _ => return Err(e),
                }
            };
            if let Some(got) = got {
                value.truncate(got);
                out.push((name.to_vec(), value));
            }
        }
        Ok(out)
    }

    /// Sets `list` on `path`, a link's never followed. An attribute the file system or this
    /// user may not set (`security.`, `trusted.`) is left out.
    #[cfg(unix)]
    pub(super) fn set(path: &Path, list: &[(Vec<u8>, Vec<u8>)]) -> io::Result<()> {
        use std::os::unix::ffi::OsStrExt as _;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
        for (name, value) in list {
            let cname = std::ffi::CString::new(name.as_slice()).map_err(io::Error::other)?;
            // SAFETY: lsetxattr(2) reads `value.len()` bytes of it.
            let done = unsafe { sys::set(c.as_ptr(), cname.as_ptr(), value.as_ptr().cast(), value.len()) };
            if done != 0 {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(libc::EPERM | libc::ENOTSUP) => {}
                    _ => return Err(e),
                }
            }
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    mod sys {
        use libc::{c_char, c_void, size_t, ssize_t};
        pub(super) unsafe fn list(path: *const c_char, buf: *mut c_char, size: size_t) -> ssize_t {
            // SAFETY: the caller's.
            unsafe { libc::listxattr(path, buf, size, libc::XATTR_NOFOLLOW) }
        }
        pub(super) unsafe fn get(path: *const c_char, name: *const c_char, buf: *mut c_void, size: size_t) -> ssize_t {
            // SAFETY: the caller's.
            unsafe { libc::getxattr(path, name, buf, size, 0, libc::XATTR_NOFOLLOW) }
        }
        pub(super) unsafe fn set(path: *const c_char, name: *const c_char, value: *const c_void, size: size_t) -> i32 {
            // SAFETY: the caller's.
            unsafe { libc::setxattr(path, name, value, size, 0, libc::XATTR_NOFOLLOW) }
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    mod sys {
        use libc::{c_char, c_void, size_t, ssize_t};
        pub(super) unsafe fn list(path: *const c_char, buf: *mut c_char, size: size_t) -> ssize_t {
            // SAFETY: the caller's.
            unsafe { libc::llistxattr(path, buf, size) }
        }
        pub(super) unsafe fn get(path: *const c_char, name: *const c_char, buf: *mut c_void, size: size_t) -> ssize_t {
            // SAFETY: the caller's.
            unsafe { libc::lgetxattr(path, name, buf, size) }
        }
        pub(super) unsafe fn set(path: *const c_char, name: *const c_char, value: *const c_void, size: size_t) -> i32 {
            // SAFETY: the caller's.
            unsafe { libc::lsetxattr(path, name, value, size, 0) }
        }
    }

    /// Windows keeps a volume's owners in NTFS's own extended attributes (D197), which this
    /// move does not carry yet: none.
    #[cfg(windows)]
    pub(super) fn list(_path: &Path) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        Ok(Vec::new())
    }

    #[cfg(windows)]
    pub(super) fn set(_path: &Path, _list: &[(Vec<u8>, Vec<u8>)]) -> io::Result<()> {
        Ok(())
    }
}

/// Puts a stream's entries under `root`, a directory of the move's own: each name checked
/// to be one [`relative`] takes, and to be one the plan allows (`allowed`), made new, never
/// over anything there.
struct Placer<'a> {
    root: &'a Path,
    allowed: &'a dyn Fn(&str) -> bool,
}

impl Placer<'_> {
    fn path(&self, rel: &str) -> Result<PathBuf, String> {
        if !(self.allowed)(rel) {
            return Err(format!("the move's stream holds {rel}, which its plan does not name"));
        }
        Ok(self.root.join(relative(rel)?))
    }

    fn take(&self, entry: Entry<'_>) -> Result<(), String> {
        let at = |p: &Path, e: &dyn std::fmt::Display| format!("{}: {e}", p.display());
        match entry {
            Entry::Dir { rel, mode, xattrs } => {
                let path = self.path(&rel)?;
                std::fs::create_dir_all(&path).map_err(|e| at(&path, &e))?;
                xattrs::set(&path, &xattrs).map_err(|e| at(&path, &e))?;
                set_mode(&path, mode).map_err(|e| at(&path, &e))
            }
            Entry::Link { rel, target } => {
                let path = self.path(&rel)?;
                make_parent(&path)?;
                symlink(&target, &path).map_err(|e| at(&path, &e))
            }
            Entry::File {
                rel,
                mode,
                xattrs,
                len,
                runs,
            } => {
                let path = self.path(&rel)?;
                make_parent(&path)?;
                let file = crate::os::owner_only(std::fs::OpenOptions::new().read(true).write(true).create_new(true))
                    .open(&path)
                    .map_err(|e| at(&path, &e))?;
                shards_vmm::platform::make_sparse(&file).map_err(|e| at(&path, &e))?;
                file.set_len(len).map_err(|e| at(&path, &e))?;
                runs(&mut |offset, bytes| {
                    // Only what is not all zeros: the file stays as sparse as it was.
                    if bytes.iter().any(|&b| b != 0) {
                        shards_vmm::platform::write_all_at(&file, bytes, offset)
                    } else {
                        Ok(())
                    }
                })
                .map_err(|e| at(&path, &e))?;
                drop(file);
                xattrs::set(&path, &xattrs).map_err(|e| at(&path, &e))?;
                set_mode(&path, mode).map_err(|e| at(&path, &e))
            }
        }
    }
}

fn make_parent(path: &Path) -> Result<(), String> {
    match path.parent() {
        Some(parent) => std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display())),
        None => Ok(()),
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_readonly(mode & 0o222 == 0);
    std::fs::set_permissions(path, perms)
}

#[cfg(unix)]
fn symlink(target: &str, path: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, path)
}

#[cfg(windows)]
fn symlink(target: &str, path: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(target, path)
}

/// Where in a home a move's stream may put what it holds: the microVM's own directory, its
/// volumes', and the files the plan says the other machine lacks.
fn allowed(plan: &Plan, needed: &BTreeSet<String>, rel: &str) -> bool {
    let under = |root: &str| rel == root || rel.starts_with(&format!("{root}/"));
    under(&format!("containers/{}", plan.id))
        || plan.volumes.iter().any(|v| under(&format!("volumes/{v}")))
        || needed.contains(rel)
}

impl<D: Disk> Daemon<D> {
    /// `shards move vm`'s steps (D176 part 6), for its client (cli/moves.rs) or a context's
    /// (remote.rs): `args` after [`MOVE_STEP`].
    pub(super) fn move_step(&self, args: &[String], asker: &Asker, reply: &Reply<'_>) -> u8 {
        let done = match args.first().map(String::as_str) {
            Some("save") => self.move_save(args.get(1).map_or("", String::as_str)),
            Some("ready") => self.move_ready(args.get(1).map_or("", String::as_str), asker, reply),
            Some("export") => self.move_export(args.get(1..).unwrap_or_default(), asker),
            Some("import") => self.move_import(asker),
            _ => Err(format!("no step of a move in {args:?}")),
        };
        match done {
            Ok(said) => {
                if !said.is_empty() {
                    reply.out(&said);
                }
                0
            }
            Err(e) => {
                reply.err(&format!("shards: {e}"));
                1
            }
        }
    }

    /// `save NAME`: the microVM saved, stopping it, or the snapshot a save stopped it as;
    /// its plan, as JSON.
    fn move_save(&self, reference: &str) -> Result<String, String> {
        let id = self.resolve(reference)?;
        self.await_start(&id);
        let snapshot = if self.running(&id) {
            let (_, name) = self
                .save_snapshot(reference, None, false)
                .map_err(|e| e.ours(reference).trim_start_matches("shards: ").to_string())?;
            name
        } else {
            let saved = lock(&self.containers).get(&id).and_then(|c| c.saved.clone());
            saved.ok_or_else(|| {
                format!("{reference} is neither running nor stopped by a save: nothing of it to move")
            })?
        };
        let plan = self.plan(&id, &snapshot)?;
        serde_json::to_string(&plan).map_err(|e| e.to_string())
    }

    /// What another machine needs of container `id`, saved as `snapshot`, to take it.
    fn plan(&self, id: &str, snapshot: &str) -> Result<Plan, String> {
        let c = lock(&self.containers)
            .get(id)
            .cloned()
            .ok_or_else(|| format!("no container {id}"))?;
        let home = std::fs::canonicalize(&self.home).map_err(|e| format!("{}: {e}", self.home.display()))?;
        let dir = lock(&self.containers).dir(id).join(SNAPSHOTS).join(snapshot);
        let pinned = shards_vmm::snapshot::read(&dir)?;
        let mut files = Vec::new();
        for (path, _) in shards_vmm::snapshot::backing_of(&pinned) {
            // As the home is named, links and all: macOS's /var is /private/var.
            let resolved = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let rel = rel_of(&home, &resolved).ok_or_else(|| {
                format!(
                    "{}: its snapshot restores against a file outside this machine's home, which no move takes",
                    path.display()
                )
            })?;
            let (sha256, size) = digest_of(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            files.push(Backing {
                path: path.to_string_lossy().into_owned(),
                rel,
                size,
                sha256,
            });
        }
        drop(pinned);
        let mut volumes = Vec::new();
        let mut binds = Vec::new();
        for m in &c.mounts {
            match m.kind.as_str() {
                "volume" if !m.name.is_empty() => volumes.push(m.name.clone()),
                "bind" => binds.push(m.source.clone()),
                _ => {}
            }
        }
        volumes.sort();
        volumes.dedup();
        Ok(Plan {
            id: c.id.clone(),
            name: c.name.clone(),
            image: c.image.clone(),
            image_id: c.image_id.clone().unwrap_or_default(),
            arch: std::env::consts::ARCH.into(),
            format: shards_vmm::snapshot::FORMAT,
            snapshot: snapshot.to_string(),
            home: home.to_string_lossy().into_owned(),
            files,
            volumes,
            binds,
        })
    }

    /// `ready PLAN`, on the machine moved to: whether it can take the plan's microVM, and
    /// what it lacks, as JSON: its image where it has none and none could be pulled, and the
    /// files its snapshot restores against that it has none of, or another of.
    fn move_ready(&self, plan: &str, asker: &Asker, reply: &Reply<'_>) -> Result<String, String> {
        let plan: Plan = serde_json::from_str(plan).map_err(|e| format!("the move's plan: {e}"))?;
        if plan.arch != std::env::consts::ARCH {
            return Err(format!(
                "{} is a {} microVM, and this machine runs {} microVMs",
                plan.name,
                plan.arch,
                std::env::consts::ARCH
            ));
        }
        if plan.format != shards_vmm::snapshot::FORMAT {
            return Err(format!(
                "{} was saved in snapshot format {}, and this machine's shards reads {}",
                plan.name,
                plan.format,
                shards_vmm::snapshot::FORMAT
            ));
        }
        {
            let registry = lock(&self.containers);
            if registry.get(&plan.id).is_some() || registry.dir(&plan.id).exists() {
                return Err(format!("a microVM with {}'s ID is here already", plan.name));
            }
            if registry.name_taken(&plan.name).is_some() {
                return Err(format!("a microVM named {} is here already", plan.name));
            }
        }
        let store = crate::volumes::Store::new(&self.home);
        for v in &plan.volumes {
            if store.get(v).is_some() {
                return Err(format!(
                    "a volume named {v} is here already, and {}'s would move here",
                    plan.name
                ));
            }
        }
        for bind in &plan.binds {
            if std::fs::symlink_metadata(bind).is_err() {
                return Err(format!(
                    "{bind}, which {} binds, is not on this machine: a move takes a bind's host path shared at the same place",
                    plan.name
                ));
            }
        }
        // Its image, by the digest its name resolved to: here, or pulled by it.
        let image = self.moved_image(&plan, asker, reply);
        let mut needs = Needs {
            image: image.is_err(),
            files: Vec::new(),
        };
        for file in &plan.files {
            let here = self.home.join(relative(&file.rel)?);
            let same = match std::fs::metadata(&here) {
                Ok(m) if m.len() == file.size => digest_of(&here).is_ok_and(|(sha256, _)| sha256 == file.sha256),
                _ => false,
            };
            if !same {
                needs.files.push(file.rel.clone());
            }
        }
        serde_json::to_string(&needs).map_err(|e| e.to_string())
    }

    /// The plan's image here, by its ID, or pulled by the digest its name resolved to.
    fn moved_image(&self, plan: &Plan, asker: &Asker, reply: &Reply<'_>) -> Result<(), String> {
        if plan.image_id.is_empty() {
            return Err("it names no image by digest".into());
        }
        let cancel = shards_registry::http::Cancel::new();
        let say = |line: &str| reply.err(line);
        if crate::run::find_image(
            &plan.image_id,
            shards_ipc::Pull::Never,
            &[],
            &asker.registry_env,
            &self.home,
            &say,
            &cancel,
        )
        .is_ok()
        {
            return Ok(());
        }
        let mut reference =
            shards_image::reference::Reference::parse(&plan.image).map_err(|e| e.to_string())?;
        reference.tag = None;
        reference.digest = Some(
            shards_image::reference::Digest::parse(&plan.image_id).map_err(|e| e.to_string())?,
        );
        crate::run::find_image(
            &reference.to_string(),
            shards_ipc::Pull::Missing,
            &[],
            &asker.registry_env,
            &self.home,
            &say,
            &cancel,
        )
        .map(|_| ())
    }

    /// `export NAME SNAPSHOT [FILE...]`: container NAME's directory but its other snapshots,
    /// its named volumes, and the files of the home named, written as a move's stream to the
    /// client's descriptor.
    fn move_export(&self, args: &[String], asker: &Asker) -> Result<String, String> {
        let (Some(reference), Some(snapshot)) = (args.first(), args.get(1)) else {
            return Err("export: a microVM and its snapshot".into());
        };
        let out = asker
            .files
            .first()
            .ok_or("export: the client sent nowhere to write")?
            .try_clone()
            .map_err(|e| e.to_string())?;
        let id = self.resolve(reference)?;
        let plan = self.plan(&id, snapshot)?;
        let home = PathBuf::from(&plan.home);
        let wanted: BTreeSet<&str> = args.get(2..).unwrap_or_default().iter().map(String::as_str).collect();
        let fail = |e: io::Error| format!("writing the move: {e}");
        let mut w = Writer::new(io::BufWriter::with_capacity(CHUNK, File::from(out)), &plan).map_err(fail)?;
        let dir = lock(&self.containers).dir(&id);
        let own = format!("containers/{id}");
        let snapshots = format!("{own}/{SNAPSHOTS}/");
        let keep = format!("{snapshots}{snapshot}");
        w.tree(&dir, &own, &|rel| {
            rel.strip_prefix(&snapshots).is_some_and(|_| rel != keep)
        })
        .map_err(fail)?;
        let store = crate::volumes::Store::new(&self.home);
        for v in &plan.volumes {
            let root = store.data(v);
            let at = root.parent().ok_or("a volume's directory")?;
            w.tree(at, &format!("volumes/{v}"), &|_| false).map_err(fail)?;
        }
        for file in &plan.files {
            if wanted.contains(file.rel.as_str()) {
                w.file(&home.join(relative(&file.rel)?), &file.rel).map_err(fail)?;
            }
        }
        w.finish().map_err(fail)?;
        Ok(String::new())
    }

    /// `import`: a move's stream read from the client's descriptor, its files put in place,
    /// its snapshot made to restore against this machine's files, and the microVM made here,
    /// stopped as saved. Its name, once it is.
    fn move_import(&self, asker: &Asker) -> Result<String, String> {
        let input = asker
            .files
            .first()
            .ok_or("import: the client sent nothing to read")?
            .try_clone()
            .map_err(|e| e.to_string())?;
        let mut input = io::BufReader::with_capacity(CHUNK, File::from(input));
        let plan = read_plan(&mut input)?;
        let needed: BTreeSet<String> = plan.files.iter().map(|f| f.rel.clone()).collect();
        let staging = self.private_staging()?;
        let placer = Placer {
            root: staging.path(),
            allowed: &|rel| allowed(&plan, &needed, rel),
        };
        read_entries(&mut input, &mut |entry| placer.take(entry))?;
        self.adopt(&plan, staging.path())
    }

    /// A directory of a move's own in the home's private one, removed with all it holds as
    /// it drops.
    fn private_staging(&self) -> Result<Staging, String> {
        let private = self.home.join(super::PRIVATE);
        match crate::os::owner_only_dir(&mut std::fs::DirBuilder::new()).create(&private) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => {
                return Err(format!("{}: {e}", private.display()));
            }
            _ => {}
        }
        let mut bytes = [0u8; 8];
        shards_vmm::platform::fill_random(&mut bytes).map_err(|e| e.to_string())?;
        let at = private.join(format!("move-{}", hex(&bytes)));
        std::fs::create_dir(&at).map_err(|e| format!("{}: {e}", at.display()))?;
        Ok(Staging(at))
    }

    /// Makes the microVM a move's stream put in `staged` this machine's: its volumes and
    /// the files its snapshot restores against put in place, its snapshot made to restore
    /// against them, its record its own here, and it seen, stopped as saved.
    fn adopt(&self, plan: &Plan, staged: &Path) -> Result<String, String> {
        let at = |p: &Path, e: &dyn std::fmt::Display| format!("{}: {e}", p.display());
        let staged_dir = staged.join("containers").join(&plan.id);
        // The files this machine lacked, kept in its own directory, each by its place in the
        // plan.
        let backing_dir = staged_dir.join(BACKING);
        let mut shipped = std::collections::BTreeMap::new();
        for (i, file) in plan.files.iter().enumerate() {
            let sent = staged.join(relative(&file.rel)?);
            if !sent.exists() {
                continue;
            }
            std::fs::create_dir_all(&backing_dir).map_err(|e| at(&backing_dir, &e))?;
            let (sha256, size) = digest_of(&sent).map_err(|e| at(&sent, &e))?;
            if sha256 != file.sha256 || size != file.size {
                return Err(format!("{}: its bytes changed on the way", file.rel));
            }
            let kept = backing_dir.join(i.to_string());
            std::fs::rename(&sent, &kept).map_err(|e| at(&sent, &e))?;
            shipped.insert(file.rel.clone(), i);
        }
        let mut c: Container = serde_json::from_slice(
            &std::fs::read(staged_dir.join("config.json")).map_err(|e| at(&staged_dir, &e))?,
        )
        .map_err(|e| format!("its record: {e}"))?;
        if c.id != plan.id {
            return Err("its record is another microVM's".into());
        }
        let store = crate::volumes::Store::new(&self.home);
        for m in &mut c.mounts {
            if m.kind == "volume" && !m.name.is_empty() {
                m.source = store.data(&m.name).to_string_lossy().into_owned();
            }
        }
        c.saved = Some(plan.snapshot.clone());
        c.moved = true;
        c.ports.clear();
        // Its request names its image by the digest it was made of, which this machine
        // found it by: a tag here may name another.
        let request = staged_dir.join(super::REQUEST);
        if let Ok(bytes) = std::fs::read(&request)
            && let Some(mut run) = shards_ipc::Run::decode(&bytes)
            && !plan.image_id.is_empty()
        {
            run.image.clone_from(&plan.image_id);
            std::fs::write(&request, run.encode()).map_err(|e| at(&request, &e))?;
        }
        let home = std::fs::canonicalize(&self.home).map_err(|e| at(&self.home, &e))?;
        let dir = lock(&self.containers).dir(&plan.id);
        // Each file its snapshot names, by the name the plan gave it there.
        let to = |path: &Path| -> Option<PathBuf> {
            let file = plan.files.iter().find(|f| Path::new(&f.path) == path)?;
            match shipped.get(&file.rel) {
                Some(i) => Some(dir.join(BACKING).join(i.to_string())),
                None => Some(home.join(relative(&file.rel).ok()?)),
            }
        };
        let record = serde_json::to_vec(&c).map_err(|e| e.to_string())?;
        std::fs::write(staged_dir.join("config.json"), &record).map_err(|e| at(&staged_dir, &e))?;
        // In place: its volumes, then its directory, which the registry sees as it takes
        // the name.
        let volumes_root = self.home.join("volumes");
        std::fs::create_dir_all(&volumes_root).map_err(|e| at(&volumes_root, &e))?;
        let mut placed: Vec<PathBuf> = Vec::new();
        let undo = |placed: &[PathBuf]| {
            for p in placed {
                let _ = std::fs::remove_dir_all(p);
            }
        };
        {
            let _volumes = crate::volumes::lock();
            for v in &plan.volumes {
                let target = volumes_root.join(v);
                if store.get(v).is_some() || target.exists() {
                    undo(&placed);
                    return Err(format!("a volume named {v} is here already"));
                }
                let sent = staged.join("volumes").join(v);
                if let Err(e) = std::fs::rename(&sent, &target) {
                    undo(&placed);
                    return Err(at(&sent, &e));
                }
                placed.push(target);
            }
        }
        {
            let mut registry = lock(&self.containers);
            if registry.get(&plan.id).is_some() || registry.name_taken(&plan.name).is_some() || dir.exists() {
                drop(registry);
                undo(&placed);
                return Err(format!("a microVM named {} is here already", plan.name));
            }
            if let Err(e) = std::fs::rename(&staged_dir, &dir) {
                drop(registry);
                undo(&placed);
                return Err(at(&staged_dir, &e));
            }
            // Its snapshot, against this machine's files where they are now.
            if let Err(e) = shards_vmm::snapshot::relocate(&dir.join(SNAPSHOTS).join(&plan.snapshot), &to) {
                let _ = std::fs::remove_dir_all(&dir);
                drop(registry);
                undo(&placed);
                return Err(e);
            }
            if let Err(e) = registry.adopt(c) {
                let _ = std::fs::remove_dir_all(&dir);
                drop(registry);
                undo(&placed);
                return Err(e);
            }
        }
        self.container_event(&plan.id, "create", &[]);
        Ok(plan.name.clone())
    }
}

/// A move's staging directory, removed with all it holds as it drops.
struct Staging(PathBuf);

impl Staging {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        Plan {
            id: "a".repeat(64),
            name: "web".into(),
            image: "alpine".into(),
            image_id: format!("sha256:{}", "b".repeat(64)),
            arch: std::env::consts::ARCH.into(),
            format: shards_vmm::snapshot::FORMAT,
            snapshot: "s1".into(),
            home: "/h".into(),
            files: vec![Backing {
                path: "/h/images/x.erofs".into(),
                rel: "images/x.erofs".into(),
                size: 3,
                sha256: "c".repeat(64),
            }],
            volumes: vec!["data".into()],
            binds: Vec::new(),
        }
    }

    /// A tree written as a move's stream reads back whole, a sparse file as sparse, its
    /// modes and links with it; and its names are only those its plan allows.
    #[test]
    fn a_tree_moves_whole_and_as_sparse() {
        let from = shards_testdir::TempDir::new("mv-from").unwrap();
        let p = plan();
        let own = from.join("c");
        std::fs::create_dir_all(own.join("snapshots/s1")).unwrap();
        std::fs::create_dir_all(own.join("snapshots/other")).unwrap();
        std::fs::write(own.join("snapshots/other/memory"), b"left").unwrap();
        std::fs::write(own.join("config.json"), b"{}").unwrap();
        let sparse = std::fs::File::create(own.join("snapshots/s1/memory")).unwrap();
        sparse.set_len(64 << 20).unwrap();
        shards_vmm::platform::write_all_at(&sparse, b"head", 0).unwrap();
        shards_vmm::platform::write_all_at(&sparse, b"tail", (64 << 20) - 4).unwrap();
        drop(sparse);
        #[cfg(unix)]
        std::os::unix::fs::symlink("config.json", own.join("link")).unwrap();
        let mut bytes = Vec::new();
        {
            let mut w = Writer::new(&mut bytes, &p).unwrap();
            let rel = format!("containers/{}", p.id);
            let skip_from = format!("{rel}/snapshots/");
            let keep = format!("{skip_from}s1");
            w.tree(&own, &rel, &|r| r.strip_prefix(&skip_from).is_some_and(|_| r != keep))
                .unwrap();
            w.finish().unwrap();
        }
        // Far less than the file's length: its hole is not sent.
        assert!(bytes.len() < 4 << 20, "{}", bytes.len());
        let to = shards_testdir::TempDir::new("mv-to").unwrap();
        let needed: BTreeSet<String> = p.files.iter().map(|f| f.rel.clone()).collect();
        let placer = Placer {
            root: &to,
            allowed: &|rel| allowed(&p, &needed, rel),
        };
        let mut input = bytes.as_slice();
        assert_eq!(read_plan(&mut input).unwrap(), p);
        read_entries(&mut input, &mut |e| placer.take(e)).unwrap();
        let there = to.join(format!("containers/{}", p.id));
        assert_eq!(std::fs::read(there.join("config.json")).unwrap(), b"{}");
        assert!(!there.join("snapshots/other").exists());
        let memory = std::fs::read(there.join("snapshots/s1/memory")).unwrap();
        assert_eq!(memory.len(), 64 << 20);
        assert_eq!(&memory[..4], b"head");
        assert_eq!(&memory[(64 << 20) - 4..], b"tail");
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(there.join("link")).unwrap(),
            PathBuf::from("config.json")
        );
        // Cut short, it is refused.
        let mut cut = &bytes[..bytes.len() - 1];
        let elsewhere = shards_testdir::TempDir::new("mv-cut").unwrap();
        let placer = Placer {
            root: &elsewhere,
            allowed: &|_| true,
        };
        read_plan(&mut cut).unwrap();
        assert!(read_entries(&mut cut, &mut |e| placer.take(e)).is_err());
    }

    /// A stream may put nothing past what its plan names: another container's directory,
    /// a name reaching out of the home, or a file its plan did not say was needed.
    #[test]
    fn a_stream_puts_only_what_its_plan_names() {
        let p = plan();
        let needed: BTreeSet<String> = p.files.iter().map(|f| f.rel.clone()).collect();
        assert!(allowed(&p, &needed, &format!("containers/{}/config.json", p.id)));
        assert!(allowed(&p, &needed, "volumes/data/_data/f"));
        assert!(allowed(&p, &needed, "images/x.erofs"));
        assert!(!allowed(&p, &needed, "containers/other/config.json"));
        assert!(!allowed(&p, &needed, "volumes/database/_data"));
        assert!(!allowed(&p, &needed, "images/y.erofs"));
        for bad in ["", "/etc/passwd", "a/../b", "a//b", "./a", "a\0b", "..", "a\\b"] {
            assert!(relative(bad).is_err(), "{bad:?}");
        }
        assert_eq!(relative("a/b").unwrap(), Path::new("a").join("b"));
        assert_eq!(rel_of(Path::new("/h"), Path::new("/h/images/x")), Some("images/x".into()));
        assert_eq!(rel_of(Path::new("/h"), Path::new("/other/x")), None);
        assert_eq!(rel_of(Path::new("/h"), Path::new("/h")), None);
    }
}
