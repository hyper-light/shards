//! Copies files between snapshots as tonistiigi/fsutil's copy package copies them for
//! BuildKit (fsutil 83cac42c1c52, copy/copy.go, mkdir.go, copy_linux.go and
//! copy_nowindows.go; continuity v0.5.0 fs/path.go for `RootPath`): `cp -a` with include
//! and exclude patterns, chown and chmod, hard links kept, and its error messages. BuildKit
//! never asks to always replace what is in the way, so that option is left out.
//!
//! Each function follows its Go counterpart call for call over [`Fs`], with one snapshot
//! to read from and one to write to, as BuildKit mounts a COPY's source and destination.
//! What Go's `defer` does at a function's end, success or not, is done there too.

use std::collections::HashMap;

use shards_dockerfile::glob::{self, MatchInfo, PatternMatcher};
use shards_dockerfile::go;
use shards_image::erofs::{Kind, Node, NodeId};

use crate::mode;
use crate::vfs::{self, Errno, Fs, PathError, S_ISGID, S_ISUID, S_ISVTX};
use crate::{Error, wrap};

/// fsutil's defaultDirectoryMode.
const DEFAULT_DIR_MODE: u32 = 0o755;
/// continuity's walkLink limit.
const MAX_ROOT_LINKS: u32 = 255;

/// Go's os.FileMode bits, as fsutil reads and writes modes through them.
pub mod fm {
    pub const DIR: u32 = 1 << 31;
    pub const SYMLINK: u32 = 1 << 27;
    pub const DEVICE: u32 = 1 << 26;
    pub const NAMED_PIPE: u32 = 1 << 25;
    pub const SOCKET: u32 = 1 << 24;
    pub const SETUID: u32 = 1 << 23;
    pub const SETGID: u32 = 1 << 22;
    pub const CHAR_DEVICE: u32 = 1 << 21;
    pub const STICKY: u32 = 1 << 20;
    pub const PERM: u32 = 0o777;
    pub const TYPE: u32 = DIR | SYMLINK | NAMED_PIPE | SOCKET | DEVICE | CHAR_DEVICE;
}

/// A node's mode as Go's `os.FileInfo.Mode` reports it (os/stat_linux.go).
pub fn file_mode(node: &Node) -> u32 {
    let st = u32::from(node.meta.mode);
    let mut m = st & 0o777;
    m |= match node.kind {
        Kind::Dir(_) => fm::DIR,
        Kind::Symlink(_) => fm::SYMLINK,
        Kind::BlockDevice { .. } => fm::DEVICE,
        Kind::CharDevice { .. } => fm::DEVICE | fm::CHAR_DEVICE,
        Kind::Fifo => fm::NAMED_PIPE,
        Kind::Socket => fm::SOCKET,
        Kind::File { .. } => 0,
    };
    if st & S_ISGID != 0 {
        m |= fm::SETGID;
    }
    if st & S_ISUID != 0 {
        m |= fm::SETUID;
    }
    if st & S_ISVTX != 0 {
        m |= fm::STICKY;
    }
    m
}

/// Go's syscallMode: the permission, set-ID and sticky bits of a FileMode.
pub fn syscall_mode(m: u32) -> u32 {
    let mut o = m & fm::PERM;
    if m & fm::SETUID != 0 {
        o |= S_ISUID;
    }
    if m & fm::SETGID != 0 {
        o |= S_ISGID;
    }
    if m & fm::STICKY != 0 {
        o |= S_ISVTX;
    }
    o
}

/// An owner, by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
}

/// BuildKit's Chowner for a file action (file/backend_unix.go mapUserToChowner, with no
/// ID mapping): with no owner asked for, what is copied keeps its owner and what is made
/// is not chowned; with one, everything gets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chown {
    Keep,
    To(User),
}

impl Chown {
    fn apply(self, old: Option<User>) -> Option<User> {
        match self {
            Chown::Keep => old,
            Chown::To(u) => Some(u),
        }
    }
}

/// fsutil's `Chown(p, old, fn)`.
pub fn chown(fs: &mut Fs, p: &[u8], old: Option<User>, ch: Chown) -> Result<(), PathError> {
    match ch.apply(old) {
        Some(u) => fs.lchown(p, u.uid, u.gid),
        None => Ok(()),
    }
}

/// fsutil's `Utimes`: with no time, nothing.
pub fn utimes(fs: &mut Fs, p: &[u8], tm: Option<(i64, u32)>) -> Result<(), Error> {
    match tm {
        Some(t) => fs
            .utimes(p, t)
            .map_err(|e| Error(format!("failed to utime {}: {}", show(p), e.errno.text()))),
        None => Ok(()),
    }
}

fn show(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn is_not_exist(e: &PathError) -> bool {
    e.errno == Errno::NoEnt
}

/// What a copy is asked to do: fsutil's CopyInfo, as BuildKit's docopy fills it.
#[derive(Debug, Clone)]
pub struct CopyInfo {
    pub chown: Chown,
    pub utime: Option<(i64, u32)>,
    pub mode: Option<u32>,
    /// A symbolic mode, which overrides `mode` when set.
    pub mode_str: Vec<u8>,
    pub copy_dir_contents: bool,
    pub follow_links: bool,
    pub include: Vec<Vec<u8>>,
    pub exclude: Vec<Vec<u8>>,
}

/// continuity's `fs.RootPath` with the snapshot's root as `root`: `path` with its
/// symlinks resolved inside the snapshot.
pub fn root_path(fs: &Fs, path: &[u8]) -> Result<Vec<u8>, Error> {
    if path.is_empty() {
        return Ok(b"/".to_vec());
    }
    let mut path = path.to_vec();
    let mut walked = 0u32;
    loop {
        let before = walked;
        let next = walk_links(fs, &path, &mut walked)?;
        path = next;
        if before == walked {
            let rooted = vfs::join(b"/", &path);
            if path == rooted {
                return Ok(rooted);
            }
            path = rooted;
        }
    }
}

fn walk_link(fs: &Fs, path: &[u8], walked: &mut u32) -> Result<(Vec<u8>, bool), Error> {
    if *walked > MAX_ROOT_LINKS {
        return Err(Error("too many links".into()));
    }
    let path = vfs::join(b"/", path);
    if path == b"/" {
        return Ok((path, false));
    }
    let id = match fs.lstat(&path) {
        Ok(id) => id,
        Err(e) if is_not_exist(&e) => return Ok((path, false)),
        Err(e) => return Err(Error(e.to_string())),
    };
    match fs.node(id).map(|n| &n.kind) {
        Some(Kind::Symlink(target)) => {
            *walked += 1;
            Ok((target.to_vec(), true))
        }
        _ => Ok((path, false)),
    }
}

fn walk_links(fs: &Fs, path: &[u8], walked: &mut u32) -> Result<Vec<u8>, Error> {
    let (dir, file) = split(path);
    if dir.is_empty() {
        return Ok(walk_link(fs, file, walked)?.0);
    }
    if file.is_empty() {
        if dir.ends_with(b"/") {
            if dir == b"/" {
                return Ok(dir.to_vec());
            }
            return walk_links(fs, dir.get(..dir.len() - 1).unwrap_or_default(), walked);
        }
        return Ok(walk_link(fs, dir, walked)?.0);
    }
    let newdir = walk_links(fs, dir, walked)?;
    let (newpath, link) = walk_link(fs, &vfs::join(&newdir, file), walked)?;
    if !link || go::is_abs(&newpath) {
        return Ok(newpath);
    }
    Ok(vfs::join(&newdir, &newpath))
}

/// Go's `filepath.Split`: everything through the last slash, and the rest.
pub fn split(p: &[u8]) -> (&[u8], &[u8]) {
    let at = p.iter().rposition(|&c| c == b'/').map_or(0, |i| i + 1);
    p.split_at_checked(at).unwrap_or((&[], p))
}

/// Go's `filepath.Base`.
pub fn base(p: &[u8]) -> Vec<u8> {
    if p.is_empty() {
        return b".".to_vec();
    }
    let mut end = p.len();
    while end > 0 && p.get(end - 1) == Some(&b'/') {
        end -= 1;
    }
    if end == 0 {
        return b"/".to_vec();
    }
    let p = p.get(..end).unwrap_or_default();
    let start = p.iter().rposition(|&c| c == b'/').map_or(0, |i| i + 1);
    p.get(start..).unwrap_or_default().to_vec()
}

/// Go's `filepath.Dir`.
pub fn dir(p: &[u8]) -> Vec<u8> {
    let at = p.iter().rposition(|&c| c == b'/').map_or(0, |i| i + 1);
    go::clean(p.get(..at).unwrap_or_default())
}

/// `filepath.Rel(base, target)` for a target at or below base, as a walk finds them.
fn rel(base: &[u8], target: &[u8]) -> Vec<u8> {
    if base == target {
        return b".".to_vec();
    }
    let prefix: Vec<u8> = if base.ends_with(b"/") {
        base.to_vec()
    } else {
        [base, b"/"].concat()
    };
    target
        .strip_prefix(prefix.as_slice())
        .map_or_else(|| target.to_vec(), <[u8]>::to_vec)
}

/// fsutil's rootPath: `p` inside the snapshot, its last symlink resolved only when
/// `follow` is set.
fn root_path_of(fs: &Fs, p: &[u8], follow: bool) -> Result<Vec<u8>, Error> {
    let p = vfs::join(b"/", p);
    if p == b"/" {
        return Ok(p);
    }
    if follow {
        return root_path(fs, &p);
    }
    let (d, f) = split(&p);
    let parent = root_path(fs, d)?;
    Ok(vfs::join(&parent, f))
}

/// fsutil's ResolveWildcards: the snapshot paths `src`'s wildcards match, relative to
/// the root, or `src` cleaned when it has none.
pub fn resolve_wildcards(fs: &Fs, src: &[u8], follow: bool) -> Result<Vec<Vec<u8>>, Error> {
    let (d1, d2) = split_wildcards(src);
    if d2.is_empty() {
        return Ok(vec![d1]);
    }
    let p = root_path_of(fs, &d1, follow)?;
    let matches = match_walk(fs, &p, &d2)?;
    Ok(matches.iter().map(|m| rel(b"/", m)).collect())
}

fn contains_wildcards(name: &[u8]) -> bool {
    let mut i = 0;
    while let Some(&c) = name.get(i) {
        if c == b'\\' {
            i += 1;
        } else if matches!(c, b'*' | b'?' | b'[') {
            return true;
        }
        i += 1;
    }
    false
}

fn split_wildcards(p: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let cleaned = go::clean(p);
    let (mut p1, mut p2): (Vec<&[u8]>, Vec<&[u8]>) = (Vec::new(), Vec::new());
    let mut found = false;
    for part in cleaned.split(|&c| c == b'/') {
        if !found && contains_wildcards(part) {
            found = true;
        }
        let part: &[u8] = if part.is_empty() { b"/" } else { part };
        if found {
            p2.push(part);
        } else {
            p1.push(part);
        }
    }
    (go::join(&p1), go::join(&p2))
}

/// fsutil's resolveWildcards: a `filepath.Walk` of `base` keeping what `comp` matches,
/// and not descending into a directory it keeps. `filepath.Match` matches no `/` with a
/// wildcard, so what it matches has as many components as `comp`: the walk goes no
/// deeper, where fsutil's walks the whole tree to find nothing more.
fn match_walk(fs: &Fs, base: &[u8], comp: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    let mut out = Vec::new();
    let depth = comp.iter().filter(|&&c| c == b'/').count() + 1;
    let root = fs.lstat(base).map_err(|e| Error(e.to_string()))?;
    walk_dir(fs, base, root, &mut |path, id| {
        let rel = rel(base, path);
        if rel == b"." {
            return Ok(Visit::Continue);
        }
        let deepest = rel.iter().filter(|&&c| c == b'/').count() + 1 >= depth;
        if !glob::filepath_match(comp, &rel).unwrap_or(false) {
            return Ok(if deepest && fs.is_dir(id) {
                Visit::SkipDir
            } else {
                Visit::Continue
            });
        }
        out.push(path.to_vec());
        Ok(if fs.is_dir(id) {
            Visit::SkipDir
        } else {
            Visit::Continue
        })
    })?;
    Ok(out)
}

/// What a walk does after visiting a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visit {
    Continue,
    SkipDir,
}

/// What a walk calls with each path and its node.
pub type Visitor<'a> = dyn FnMut(&[u8], NodeId) -> Result<Visit, Error> + 'a;

/// Go's `filepath.Walk` from `path`: each path, then a directory's entries in name order;
/// symlinks are not followed.
pub fn walk_dir(fs: &Fs, path: &[u8], id: NodeId, visit: &mut Visitor<'_>) -> Result<(), Error> {
    if let Visit::SkipDir = visit(path, id)? {
        return Ok(());
    }
    if !fs.is_dir(id) {
        return Ok(());
    }
    let names = fs.read_dir(path).map_err(|e| Error(e.to_string()))?;
    for name in names {
        let child = vfs::join(path, &name);
        let cid = fs.lstat(&child).map_err(|e| Error(e.to_string()))?;
        walk_dir(fs, &child, cid, visit)?;
    }
    Ok(())
}

/// fsutil's MkdirAll, a fork of `os.MkdirAll` that chmods, chowns and stamps each
/// directory it makes, returning them.
pub fn mkdir_all(
    fs: &mut Fs,
    path: &[u8],
    perm: u32,
    ch: Chown,
    tm: Option<(i64, u32)>,
) -> Result<Vec<Vec<u8>>, Error> {
    if let Ok(id) = fs.stat(path) {
        if fs.is_dir(id) {
            return Ok(Vec::new());
        }
        return Err(Error(
            PathError {
                op: "mkdir",
                path: path.to_vec(),
                errno: Errno::NotDir,
            }
            .to_string(),
        ));
    }
    let mut i = path.len();
    while i > 0 && path.get(i - 1) == Some(&b'/') {
        i -= 1;
    }
    let mut j = i;
    while j > 0 && path.get(j - 1) != Some(&b'/') {
        j -= 1;
    }
    let mut created = Vec::new();
    if j > 1 {
        created = mkdir_all(fs, path.get(..j - 1).unwrap_or_default(), perm, ch, tm)?;
    }
    if let Ok(id) = fs.lstat(path)
        && fs.is_dir(id)
    {
        return Ok(created);
    }
    if let Err(e) = fs.mkdir(path, syscall_mode(perm)) {
        if let Ok(id) = fs.lstat(path)
            && fs.is_dir(id)
        {
            return Ok(created);
        }
        return Err(Error(e.to_string()));
    }
    fs.chmod(path, syscall_mode(perm))
        .map_err(|e| Error(e.to_string()))?;
    created.push(path.to_vec());
    chown(fs, path, None, ch).map_err(|e| Error(e.to_string()))?;
    utimes(fs, path, tm)?;
    Ok(created)
}

/// fsutil's fixCreatedParentDirs, which every Copy defers: the directories it made get
/// `tm` again, after what was made in them since stamped them. Its error is dropped, as
/// a deferred call's is.
fn fix_created(fs: &mut Fs, dirs: &[Vec<u8>], tm: Option<(i64, u32)>) {
    if let Some(t) = tm {
        for d in dirs.iter().rev() {
            if fs.utimes(d, t).is_err() {
                return;
            }
        }
    }
}

/// fsutil's Copy, of `src` in `from` to `dst` in `to`.
pub fn copy(from: &Fs, src: &[u8], to: &mut Fs, dst: &[u8], ci: &CopyInfo) -> Result<(), Error> {
    let mut deferred: Vec<Vec<Vec<u8>>> = Vec::new();
    let r = copy_inner(from, src, to, dst, ci, &mut deferred);
    for dirs in deferred.iter().rev() {
        fix_created(to, dirs, ci.utime);
    }
    r
}

fn copy_inner(
    from: &Fs,
    src: &[u8],
    to: &mut Fs,
    dst: &[u8],
    ci: &CopyInfo,
    deferred: &mut Vec<Vec<Vec<u8>>>,
) -> Result<(), Error> {
    let (d, f) = split(dst);
    let ensure: &[u8] = if !f.is_empty() && f != b"." { d } else { dst };
    if !ensure.is_empty() {
        let ensure = root_path(to, ensure)?;
        let perm = ci.mode.unwrap_or(DEFAULT_DIR_MODE);
        let created = mkdir_all(to, &ensure, perm, ci.chown, ci.utime)?;
        deferred.push(created);
    }
    let mode_set = if ci.mode_str.is_empty() {
        None
    } else {
        Some(mode::parse(&ci.mode_str).map_err(Error)?)
    };
    let dst = root_path(to, &go::clean(dst))?;
    let mut c = Copier::new(ci, mode_set, from)?;
    let src_followed = root_path_of(from, src, ci.follow_links)?;
    let (dst, created) = c.prepare_target_dir(from, to, &src_followed, src, &dst, ci.copy_dir_contents)?;
    deferred.push(created);
    c.copy(
        from,
        to,
        &src_followed,
        b"",
        &dst,
        false,
        &MatchInfo::default(),
        &MatchInfo::default(),
    )
}

struct ParentDir {
    src: Vec<u8>,
    dst: Vec<u8>,
    copied: bool,
}

struct Copier {
    chown: Chown,
    utime: Option<(i64, u32)>,
    mode: Option<u32>,
    mode_set: Option<mode::Set>,
    /// Source files with several links, by node, and where the first was copied to.
    inodes: HashMap<NodeId, Vec<u8>>,
    /// The source's link counts.
    links: Vec<u32>,
    include: Option<PatternMatcher>,
    exclude: Option<PatternMatcher>,
    parents: Vec<ParentDir>,
}

impl Copier {
    fn new(ci: &CopyInfo, mode_set: Option<mode::Set>, from: &Fs) -> Result<Copier, Error> {
        let include = if ci.include.is_empty() {
            None
        } else {
            Some(PatternMatcher::new(&ci.include).map_err(|e| {
                Error(format!(
                    "invalid includepatterns: {}: {}",
                    go_list(&ci.include),
                    show(&e)
                ))
            })?)
        };
        let exclude = if ci.exclude.is_empty() {
            None
        } else {
            Some(PatternMatcher::new(&ci.exclude).map_err(|e| {
                Error(format!(
                    "invalid excludepatterns: {}: {}",
                    go_list(&ci.exclude),
                    show(&e)
                ))
            })?)
        };
        Ok(Copier {
            chown: ci.chown,
            utime: ci.utime,
            mode: ci.mode,
            mode_set,
            inodes: HashMap::new(),
            links: from.links(),
            include,
            exclude,
            parents: Vec::new(),
        })
    }

    fn prepare_target_dir(
        &mut self,
        from: &Fs,
        to: &mut Fs,
        src_followed: &[u8],
        src: &[u8],
        dest: &[u8],
        copy_dir_contents: bool,
    ) -> Result<(Vec<u8>, Vec<Vec<u8>>), Error> {
        let fi_src = from.lstat(src_followed).map_err(|e| Error(e.to_string()))?;
        let src_dir = from.is_dir(fi_src);
        let fi_dest = match to.stat(dest) {
            Ok(id) => Some(id),
            Err(e) if is_not_exist(&e) => None,
            Err(e) => {
                return Err(Error(format!("failed to lstat destination path: {e}")));
            }
        };
        let mut dest = dest.to_vec();
        let dest_dir = fi_dest.is_some_and(|id| to.is_dir(id));
        if (!copy_dir_contents && src_dir && fi_dest.is_some()) || (!src_dir && dest_dir) {
            dest = vfs::join(&dest, &base(src));
        }
        let mut target = dir(&dest);
        if copy_dir_contents && src_dir && fi_dest.is_none() {
            target = dest.clone();
        }
        let mode = self.mode.unwrap_or(DEFAULT_DIR_MODE);
        let created = mkdir_all(to, &target, mode, self.chown, self.utime)?;
        Ok((dest, created))
    }

    fn matches(
        matcher: &mut Option<PatternMatcher>,
        what: &str,
        path: &[u8],
        parent: &MatchInfo,
        none: bool,
    ) -> Result<(bool, MatchInfo), Error> {
        match matcher {
            None => Ok((none, MatchInfo::default())),
            Some(m) => m
                .matches_using_parent_results(path, parent)
                .map_err(|e| Error(format!("failed to match {what}: {}", show(&e)))),
        }
    }

    fn exclusions(&self) -> bool {
        self.exclude.as_ref().is_some_and(PatternMatcher::exclusions)
    }

    #[allow(clippy::too_many_arguments)]
    fn copy(
        &mut self,
        from: &Fs,
        to: &mut Fs,
        src: &[u8],
        comps: &[u8],
        target: &[u8],
        overwrite_meta: bool,
        parent_inc: &MatchInfo,
        parent_exc: &MatchInfo,
    ) -> Result<(), Error> {
        let mut include = true;
        let mut excluded = false;
        let (mut inc_info, mut exc_info) = (MatchInfo::default(), MatchInfo::default());
        if !comps.is_empty() {
            let (m, info) = Self::matches(&mut self.include, "includepatterns", comps, parent_inc, true)?;
            include = m;
            inc_info = info;
            let (m, info) = Self::matches(&mut self.exclude, "excludepatterns", comps, parent_exc, false)?;
            exc_info = info;
            if m {
                include = false;
                excluded = true;
            }
            let can_skip = !include && self.include.is_none() && !self.exclusions();
            if can_skip {
                return Ok(());
            }
        }
        let fi = from
            .lstat(src)
            .map_err(|e| Error(format!("failed to stat {}: {e}", show(src))))?;
        let Some(node) = from.node(fi) else {
            return Err(Error(format!("failed to stat {}", show(src))));
        };
        let is_dir = from.is_dir(fi);
        if !include && !is_dir {
            return Ok(());
        }
        let target_fi = match to.lstat(target) {
            Ok(id) => Some(id),
            Err(e) if is_not_exist(&e) => None,
            Err(e) => return Err(Error(format!("failed to stat {}: {e}", show(target)))),
        };
        if include {
            self.create_parent_dirs(from, to, src, overwrite_meta)?;
        }
        if !is_dir {
            ensure_empty_file_target(to, target)?;
        }
        let mut copy_info = include;
        let mut restore_time = false;
        match &node.kind {
            Kind::Dir(_) => {
                let created = self.copy_directory(
                    from,
                    to,
                    src,
                    comps,
                    target,
                    fi,
                    overwrite_meta,
                    include,
                    excluded,
                    &inc_info,
                    &exc_info,
                )?;
                if !overwrite_meta {
                    copy_info = created;
                    restore_time = !created;
                }
            }
            Kind::File { size, data } => {
                let link = self.link_source(target, fi);
                if let Some(link) = link {
                    to.link(&link, target)
                        .map_err(|e| Error(format!("failed to create hard link: {e}")))?;
                } else {
                    let id = to.create(target, 0o666).map_err(|e| {
                        Error(format!(
                            "failed to copy files: failed to open target {}: {e}",
                            show(target)
                        ))
                    })?;
                    to.set_data(id, *size, *data);
                }
            }
            Kind::Symlink(link) => {
                to.symlink(link, target)
                    .map_err(|e| Error(format!("failed to create symlink: {}: {e}", show(target))))?;
            }
            Kind::CharDevice { .. } | Kind::BlockDevice { .. } | Kind::Fifo | Kind::Socket => {
                // A socket is copied as a stub: its type bits cleared, mknod makes a file.
                let kind = match &node.kind {
                    Kind::Socket => Kind::File {
                        size: 0,
                        data: vfs::EMPTY,
                    },
                    k => k.clone(),
                };
                to.mknod(target, kind, u32::from(node.meta.mode))
                    .map_err(|e| Error(format!("failed to create device: {}", e.errno.text())))?;
            }
        }
        if copy_info {
            self.copy_file_info(from, to, fi, target)
                .map_err(|e| Error(format!("failed to copy file info: {}", e.0)))?;
            copy_xattrs(from, fi, to, target);
        } else if restore_time && target_fi.is_some() {
            self.copy_file_time(from, to, fi, target)
                .map_err(|e| Error(format!("failed to restore file timestamp: {}", e.0)))?;
        }
        Ok(())
    }

    /// fsutil's getLinkSource: where a file with other links was first copied to, or
    /// nothing, recording this copy as the first.
    fn link_source(&mut self, target: &[u8], fi: NodeId) -> Option<Vec<u8>> {
        if self.links.get(fi).copied().unwrap_or(0) <= 1 {
            return None;
        }
        match self.inodes.get(&fi) {
            Some(p) => Some(p.clone()),
            None => {
                self.inodes.insert(fi, target.to_vec());
                None
            }
        }
    }

    /// fsutil's createParentDirs: directories held back for include patterns, made once
    /// something under them is copied.
    fn create_parent_dirs(
        &mut self,
        from: &Fs,
        to: &mut Fs,
        src: &[u8],
        overwrite_meta: bool,
    ) -> Result<(), Error> {
        for i in 0..self.parents.len() {
            let Some(p) = self.parents.get(i) else { continue };
            if p.copied {
                continue;
            }
            let (psrc, pdst) = (p.src.clone(), p.dst.clone());
            let fi = from
                .stat(&psrc)
                .map_err(|e| Error(format!("failed to stat {}: {e}", show(src))))?;
            if !from.is_dir(fi) {
                return Err(Error(format!("{} is not a directory", show(&psrc))));
            }
            let created = copy_directory_only(from, to, &pdst, fi, overwrite_meta)?;
            if created {
                self.copy_file_info(from, to, fi, &pdst)
                    .map_err(|e| Error(format!("failed to copy file info: {}", e.0)))?;
                copy_xattrs(from, fi, to, &pdst);
            }
            if let Some(p) = self.parents.get_mut(i) {
                p.copied = true;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_directory(
        &mut self,
        from: &Fs,
        to: &mut Fs,
        src: &[u8],
        comps: &[u8],
        dst: &[u8],
        fi: NodeId,
        overwrite_meta: bool,
        include: bool,
        excluded: bool,
        inc_info: &MatchInfo,
        exc_info: &MatchInfo,
    ) -> Result<bool, Error> {
        let mut created = false;
        let mut parent = ParentDir {
            src: src.to_vec(),
            dst: dst.to_vec(),
            copied: false,
        };
        if include {
            created = copy_directory_only(from, to, dst, fi, overwrite_meta)?;
            parent.copied = true;
        }
        self.parents.push(parent);
        // An excluded directory was not made here, so Go's `false` is `created`.
        let r = self.copy_entries(from, to, src, comps, dst, excluded, inc_info, exc_info);
        self.parents.pop();
        r.map(|()| created)
    }

    /// The rest of copyDirectory: its entries, unless it is excluded and no exception could
    /// bring one back.
    #[allow(clippy::too_many_arguments)]
    fn copy_entries(
        &mut self,
        from: &Fs,
        to: &mut Fs,
        src: &[u8],
        comps: &[u8],
        dst: &[u8],
        excluded: bool,
        inc_info: &MatchInfo,
        exc_info: &MatchInfo,
    ) -> Result<(), Error> {
        if excluded && !self.exclusions() {
            return Ok(());
        }
        let names = from
            .read_dir(src)
            .map_err(|e| Error(format!("failed to read {}: {e}", show(src))))?;
        for name in names {
            let child_comps = if comps.is_empty() {
                name.clone()
            } else {
                [comps, b"/", &name].concat()
            };
            self.copy(
                from,
                to,
                &vfs::join(src, &name),
                &child_comps,
                &vfs::join(dst, &name),
                true,
                inc_info,
                exc_info,
            )?;
        }
        Ok(())
    }

    /// fsutil's copyFileInfo (copy_linux.go): owner, mode, then times.
    fn copy_file_info(&self, from: &Fs, to: &mut Fs, fi: NodeId, name: &[u8]) -> Result<(), Error> {
        let Some(node) = from.node(fi) else {
            return Err(Error("missing source".into()));
        };
        let old = User {
            uid: node.meta.uid,
            gid: node.meta.gid,
        };
        chown(to, name, Some(old), self.chown)
            .map_err(|e| Error(format!("failed to chown {}: {e}", show(name))))?;
        let src_mode = file_mode(node);
        let mut m = src_mode;
        if let Some(set) = &self.mode_set {
            m = set.apply(m);
        } else if let Some(mode) = self.mode {
            m = mode & fm::PERM;
            if mode & S_ISGID != 0 {
                m |= fm::SETGID;
            }
            if mode & S_ISUID != 0 {
                m |= fm::SETUID;
            }
            if mode & S_ISVTX != 0 {
                m |= fm::STICKY;
            }
        }
        if src_mode & fm::SYMLINK == 0 {
            to.chmod(name, syscall_mode(m))
                .map_err(|e| Error(format!("failed to chmod {}: {e}", show(name))))?;
        }
        self.copy_file_time(from, to, fi, name)
    }

    fn copy_file_time(&self, from: &Fs, to: &mut Fs, fi: NodeId, name: &[u8]) -> Result<(), Error> {
        if self.utime.is_some() {
            return utimes(to, name, self.utime);
        }
        let Some(node) = from.node(fi) else {
            return Err(Error("missing source".into()));
        };
        to.utimes(name, (node.meta.mtime, node.meta.mtime_nsec))
            .map_err(|e| Error(format!("failed to utime {}: {}", show(name), e.errno.text())))
    }
}

/// fsutil's copyDirectoryOnly: makes `dst` as `fi`'s mode allows, or with
/// `overwrite_meta` gives an existing one that mode; true when it made it.
fn copy_directory_only(
    from: &Fs,
    to: &mut Fs,
    dst: &[u8],
    fi: NodeId,
    overwrite_meta: bool,
) -> Result<bool, Error> {
    let mode = from.node(fi).map(file_mode).unwrap_or(fm::DIR | 0o755);
    match to.lstat(dst) {
        Err(e) if is_not_exist(&e) => {
            to.mkdir(dst, syscall_mode(mode))
                .map_err(|e| Error(format!("failed to mkdir {}: {e}", show(dst))))?;
            Ok(true)
        }
        Err(e) => Err(Error(e.to_string())),
        Ok(id) if !to.is_dir(id) => Err(Error(format!("cannot copy to non-directory: {}", show(dst)))),
        Ok(_) => {
            if overwrite_meta {
                to.chmod(dst, syscall_mode(mode))
                    .map_err(|e| Error(format!("failed to chmod on {}: {e}", show(dst))))?;
            }
            Ok(false)
        }
    }
}

fn ensure_empty_file_target(to: &mut Fs, dst: &[u8]) -> Result<(), Error> {
    match to.lstat(dst) {
        Err(e) if is_not_exist(&e) => Ok(()),
        Err(e) => Err(wrap("failed to lstat file target", e)),
        Ok(id) if to.is_dir(id) => Err(Error(format!(
            "cannot replace to directory {} with file",
            show(dst)
        ))),
        Ok(_) => to.remove(dst).map_err(|e| Error(e.to_string())),
    }
}

/// fsutil's copyXAttrs under BuildKit's handler, which logs a failure and returns nil:
/// the attributes up to the first that will not set.
fn copy_xattrs(from: &Fs, fi: NodeId, to: &mut Fs, dst: &[u8]) {
    let Some(node) = from.node(fi) else { return };
    for (k, v) in node.meta.xattrs.iter() {
        if to.setxattr(dst, k, v, false).is_err() {
            return;
        }
    }
}

/// A list as Go's `%s` prints a `[]string`.
fn go_list(items: &[Vec<u8>]) -> String {
    let parts: Vec<String> = items.iter().map(|i| show(i)).collect();
    format!("[{}]", parts.join(" "))
}
