//! The build context as BuildKit receives it (docs/design/architecture.md D33): the
//! client's walk of the directory (fsutil 83cac42c1c52 fs.go, filter.go and
//! followlinks.go, under buildx's map that owns everything by root) with the include,
//! exclude and follow paths the context's source asks for, and the receiving disk
//! writer's calls (diskwriter.go, diskwriter_unix.go), replayed on a fresh snapshot.

use std::path::Path;

use shards_agentfile::glob::{self, MatchInfo, PatternMatcher};
use shards_agentfile::go;
use shards_image::erofs::{Kind, Meta, NodeId, Tree};

use crate::Error;
use crate::copy::{fm, syscall_mode};
use crate::data::Sources;
use crate::host::{self, Stat};
use crate::vfs::{self, Fs};

/// What a local source asks the client for (`local.includepatterns`,
/// `local.excludepatterns`, `local.followpaths`).
#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub include: Vec<Vec<u8>>,
    pub exclude: Vec<Vec<u8>>,
    pub follow: Vec<Vec<u8>>,
}

/// The characters that make a pattern more than a prefix (filter.go patternChars).
const PATTERN_CHARS: &[u8] = b"*[]?^\\";

fn show(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Reads the context `dir` as the client sends it and the daemon writes it: a snapshot
/// whose root BuildKit made at `now`, files owned by root. Each file's bytes are taken
/// into `stage`, a directory private to the build, as one version no later edit
/// changes ([`host::snapshot`]): as BuildKit's copy keeps its build from seeing an edit,
/// without copying where the file system clones. `stage` must outlive `sources`' reads.
pub fn load(
    dir: &Path,
    filters: &Filters,
    sources: &mut Sources,
    now: (i64, u32),
    stage: &Path,
) -> Result<Fs, Error> {
    let root = host::eval_symlinks(dir).map_err(|e| Error(format!("resolve {}: {e}", dir.display())))?;
    let mut walker = Walker::new(&root, filters)?;
    let mut sent = Vec::new();
    walker.entries(b"", &mut Vec::new(), &mut sent)?;
    reset_hardlinks(&mut sent);
    let mut taker = host::Stage::new(stage).map_err(|e| Error(format!("{}: {e}", stage.display())))?;
    let whole = whole_dirs(&sent, &walker.partial);
    let pack = std::fs::File::open(taker.pack_path()).map_err(|e| Error(e.to_string()))?;
    let pack = sources.archive(pack).map_err(|e| Error(e.to_string()))?;
    let fs = receive(&root, sent, sources, now, &mut taker, pack, &whole)?;
    taker.finish().map_err(|e| Error(e.to_string()))?;
    Ok(fs)
}

/// The directories sent that the walk kept whole, each cloned at once once its files are
/// read, where the stage clones one ([`host::Stage::clone_dir`]): the root if it is
/// whole, else each whole directory whose parent is not.
fn whole_dirs(
    sent: &[(Vec<u8>, Stat)],
    partial: &std::collections::HashSet<Vec<u8>>,
) -> std::collections::HashSet<Vec<u8>> {
    if !partial.contains(b"".as_slice()) {
        return std::collections::HashSet::from([Vec::new()]);
    }
    sent.iter()
        .filter(|(rel, st)| st.mode & fm::DIR != 0 && !partial.contains(rel) && partial.contains(parent(rel)))
        .map(|(rel, _)| rel.clone())
        .collect()
}

/// The directory `rel` is in: the root's, empty, for one at the top.
fn parent(rel: &[u8]) -> &[u8] {
    rel.iter()
        .rposition(|&c| c == b'/')
        .and_then(|i| rel.get(..i))
        .unwrap_or_default()
}

/// The one of the directories `whole` that `rel` is in, the root's included.
fn whole_of<'a>(whole: &std::collections::HashSet<Vec<u8>>, rel: &'a [u8]) -> Option<&'a [u8]> {
    let mut dir = rel;
    while !dir.is_empty() {
        dir = parent(dir);
        if whole.contains(dir) {
            return Some(dir);
        }
    }
    None
}

/// A file [`host::Stage::read`] read in place, one version once its directory's clone
/// has waited out a write under way and its times say no write came: its node, its
/// snapshot, what the read saw and the whole directory it is in.
struct Read {
    rel: Vec<u8>,
    id: NodeId,
    got: host::Snapshot,
    seen: host::Seen,
    dir: Vec<u8>,
}

/// hardlinks.go WithHardlinkReset, which the sender walks through: a file whose link
/// names a path not sent becomes the file that path's other links name.
fn reset_hardlinks(sent: &mut [(Vec<u8>, Stat)]) {
    let mut seen: std::collections::HashMap<Vec<u8>, Vec<u8>> = std::collections::HashMap::new();
    for (path, st) in sent.iter_mut() {
        if st.mode & (fm::DIR | fm::SYMLINK) != 0 {
            continue;
        }
        if !st.link.is_empty() {
            match seen.get(&st.link) {
                None => {
                    seen.insert(std::mem::take(&mut st.link), path.clone());
                }
                Some(first) if first != path => st.link = first.clone(),
                Some(_) => {}
            }
        }
        seen.insert(path.clone(), path.clone());
    }
}

/// What the client sends of the context directory `root` (already resolved): each path
/// kept, in walk order, with what fsutil's mkstat records of it. A file with other links
/// already sent carries the first one's path as its link.
pub fn walk(root: &Path, filters: &Filters) -> Result<Vec<(Vec<u8>, Stat)>, Error> {
    let mut sent = Vec::new();
    Walker::new(root, filters)?.walk(&mut sent)?;
    Ok(sent)
}

/// A walked directory, as filter.go's visitedDir.
struct Visited {
    rel: Vec<u8>,
    stat: Stat,
    include: MatchInfo,
    exclude: MatchInfo,
    called: bool,
}

struct Walker<'a> {
    root: &'a Path,
    include: Option<PatternMatcher>,
    exclude: Option<PatternMatcher>,
    only_prefix_includes: bool,
    only_prefix_exceptions: bool,
    /// Files with several links, by inode, and the first path sent of each (seenFiles).
    seen: std::collections::HashMap<u64, Vec<u8>>,
    /// The directories it did not walk all of, by path: one holding a directory it left
    /// out without walking in or could not read, a device, a FIFO or a socket, or such a
    /// directory itself. The rest are whole, cloned at once where the stage can.
    partial: std::collections::HashSet<Vec<u8>>,
}

/// filter.go patternWithoutTrailingGlob.
fn without_trailing_glob(p: &[u8]) -> &[u8] {
    let p = p.strip_suffix(b"/**").unwrap_or(p);
    p.strip_suffix(b"/*").unwrap_or(p)
}

impl<'a> Walker<'a> {
    /// NewFilterFS.
    fn new(root: &'a Path, f: &Filters) -> Result<Walker<'a>, Error> {
        let mut includes = f.include.clone();
        if !f.follow.is_empty()
            && let Some(targets) = follow_links(root, &f.follow)?
        {
            includes.extend(targets);
            includes = dedupe(includes);
        }
        let mut w = Walker {
            root,
            include: None,
            exclude: None,
            only_prefix_includes: true,
            only_prefix_exceptions: true,
            seen: std::collections::HashMap::new(),
            partial: std::collections::HashSet::new(),
        };
        if !includes.is_empty() {
            let m = PatternMatcher::new(&includes).map_err(|e| {
                Error(format!(
                    "invalid includepatterns: {}: {}",
                    go_list(&includes),
                    show(&e)
                ))
            })?;
            w.only_prefix_includes = !m.patterns().iter().any(|p| {
                !p.exclusion()
                    && without_trailing_glob(p.text())
                        .iter()
                        .any(|c| PATTERN_CHARS.contains(c))
            });
            w.include = Some(m);
        }
        if !f.exclude.is_empty() {
            let m = PatternMatcher::new(&f.exclude).map_err(|e| {
                Error(format!(
                    "invalid excludepatterns: {}: {}",
                    go_list(&f.exclude),
                    show(&e)
                ))
            })?;
            w.only_prefix_exceptions = !m.patterns().iter().any(|p| {
                p.exclusion()
                    && without_trailing_glob(p.text())
                        .iter()
                        .any(|c| PATTERN_CHARS.contains(c))
            });
            w.exclude = Some(m);
        }
        Ok(w)
    }

    fn walk(mut self, sent: &mut Vec<(Vec<u8>, Stat)>) -> Result<(), Error> {
        let mut parents = Vec::new();
        self.entries(b"", &mut parents, sent).map(|_| ())
    }

    /// The entries of the directory `rel` (the root when empty), in name order: whether
    /// it walked all of it.
    fn entries(
        &mut self,
        rel: &[u8],
        parents: &mut Vec<Visited>,
        sent: &mut Vec<(Vec<u8>, Stat)>,
    ) -> Result<bool, Error> {
        let names = match host::read_dir(&host::path(self.root, rel)) {
            Ok(n) => n,
            // A directory gone is skipped; one unreadable is skipped only if filtered out.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(e) => {
                let skipped = !rel.is_empty() && parents.last().is_some_and(|p| p.rel == rel && !p.called);
                if skipped && e.kind() == std::io::ErrorKind::PermissionDenied {
                    self.partial.insert(rel.to_vec());
                    return Ok(false);
                }
                return Err(Error(format!(
                    "open {}: {e}",
                    host::path(self.root, rel).display()
                )));
            }
        };
        let mut whole = true;
        for name in names {
            let child = if rel.is_empty() {
                name
            } else {
                [rel, b"/", &name].concat()
            };
            whole &= self.entry(&child, parents, sent)?;
        }
        if !whole {
            self.partial.insert(rel.to_vec());
        }
        Ok(whole)
    }

    fn matches(
        m: &mut Option<PatternMatcher>,
        what: &str,
        path: &[u8],
        parent: Option<&MatchInfo>,
    ) -> Result<Option<(bool, MatchInfo)>, Error> {
        let Some(m) = m else { return Ok(None) };
        let empty = MatchInfo::default();
        m.matches_using_parent_results(path, parent.unwrap_or(&empty))
            .map(Some)
            .map_err(|e| Error(format!("failed to match {what}: {}", show(&e))))
    }

    fn prefixes(&self, m: &PatternMatcher, exclusions: bool, path: &[u8]) -> bool {
        let dir_slash = [path, b"/"].concat();
        m.patterns()
            .iter()
            .filter(|p| p.exclusion() == exclusions)
            .any(|p| {
                let pat = [without_trailing_glob(p.text()), b"/"].concat();
                pat.starts_with(&dir_slash)
            })
    }

    /// One path of filterFS.Walk: whether it walked all of it.
    fn entry(
        &mut self,
        rel: &[u8],
        parents: &mut Vec<Visited>,
        sent: &mut Vec<(Vec<u8>, Stat)>,
    ) -> Result<bool, Error> {
        let stat = match host::lstat(&host::path(self.root, rel)) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(e) => {
                return Err(Error(format!(
                    "lstat {}: {e}",
                    host::path(self.root, rel).display()
                )));
            }
        };
        // stat_unix.go setUnixOpt, before mkstat sets a symlink's own target. buildx's
        // inner file system stats every path it walks, so a file the filters leave out
        // still becomes the one its other links name.
        let mut stat = stat;
        if let Some((ino, nlink)) = stat.inode
            && nlink > 1
        {
            match self.seen.get(&ino) {
                Some(first) if stat.mode & fm::SYMLINK == 0 => stat.link = first.clone(),
                Some(_) => {}
                None => {
                    self.seen.insert(ino, rel.to_vec());
                }
            }
        }
        let is_dir = stat.mode & fm::DIR != 0;
        let mut skip = false;
        let parent_inc = parents.last().map(|p| &p.include);
        let parent_exc = parents.last().map(|p| &p.exclude);
        let (mut inc_info, mut exc_info) = (MatchInfo::default(), MatchInfo::default());
        if let Some((m, info)) = Self::matches(&mut self.include, "includepatterns", rel, parent_inc)? {
            inc_info = info;
            if !m {
                if is_dir && self.only_prefix_includes {
                    let walk_in = self
                        .include
                        .as_ref()
                        .is_some_and(|i| self.prefixes(i, false, rel));
                    if !walk_in {
                        return Ok(false);
                    }
                }
                skip = true;
            }
        }
        if let Some((m, info)) = Self::matches(&mut self.exclude, "excludepatterns", rel, parent_exc)? {
            exc_info = info;
            if m {
                if is_dir && self.only_prefix_exceptions {
                    let Some(ex) = self.exclude.as_ref() else {
                        return Ok(false);
                    };
                    if !ex.exclusions() || !self.prefixes(ex, true, rel) {
                        return Ok(false);
                    }
                }
                skip = true;
            }
        }
        if !skip {
            for p in parents.iter_mut().filter(|p| !p.called) {
                p.called = true;
                sent.push((p.rel.clone(), p.stat.clone()));
            }
            sent.push((rel.to_vec(), stat.clone()));
        }
        // A device, FIFO or socket: none is cloned with its directory.
        let special = stat.socket || stat.mode & (fm::DEVICE | fm::NAMED_PIPE) != 0;
        if is_dir {
            parents.push(Visited {
                rel: rel.to_vec(),
                stat,
                include: inc_info,
                exclude: exc_info,
                called: !skip,
            });
            let r = self.entries(rel, parents, sent);
            parents.pop();
            return r;
        }
        Ok(!special)
    }
}

/// followlinks.go FollowLinks: the paths with every symlink on the way resolved, each
/// link kept too; none when the whole context is wanted.
fn follow_links(root: &Path, paths: &[Vec<u8>]) -> Result<Option<Vec<Vec<u8>>>, Error> {
    let mut resolved = std::collections::BTreeSet::new();
    for p in paths {
        append(root, &mut resolved, p)?;
    }
    let v = dedupe(resolved.into_iter().collect());
    Ok((!v.is_empty()).then_some(v))
}

fn append(root: &Path, resolved: &mut std::collections::BTreeSet<Vec<u8>>, p: &[u8]) -> Result<(), Error> {
    let mut p = go::join(&[b".", p]);
    let mut current = b".".to_vec();
    loop {
        let (first, rest) = match p.iter().position(|&c| c == b'/') {
            Some(i) => (
                p.get(..i).unwrap_or_default().to_vec(),
                p.get(i + 1..).unwrap_or_default().to_vec(),
            ),
            None => (p.clone(), Vec::new()),
        };
        current = go::join(&[&current, &first]);
        let targets = read_symlink(root, &current, true)?;
        p = rest;
        if (p.is_empty() || targets.is_some()) && resolved.contains(&current) {
            return Ok(());
        }
        if let Some(targets) = targets {
            resolved.insert(current.clone());
            for t in targets {
                append(root, resolved, &go::join(&[&t, &p]))?;
            }
            return Ok(());
        }
        if p.is_empty() {
            resolved.insert(current);
            return Ok(());
        }
    }
}

/// followlinks.go readSymlink: what a symlink at `p` points to, absolute; nothing for
/// what is not one or is not there. A wildcard last element reads every match.
fn read_symlink(root: &Path, p: &[u8], wildcard: bool) -> Result<Option<Vec<Vec<u8>>>, Error> {
    let base = crate::copy::base(p);
    let parent = crate::copy::dir(p);
    if wildcard && contains_wildcards(&base) {
        let names = match host::read_dir(&host::path(root, if parent == b"." { b"" } else { &parent })) {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Error(format!("readdir: {e}"))),
        };
        let mut out = Vec::new();
        for name in names {
            if glob::filepath_match(&base, &name).unwrap_or(false)
                && let Some(t) = read_symlink(root, &go::join(&[&parent, &name]), false)?
            {
                out.extend(t);
            }
        }
        // Go's nil slice: no match is no symlink.
        return Ok((!out.is_empty()).then_some(out));
    }
    let clean = go::clean(p);
    if clean == b"/" || clean == b"." {
        return Ok(None);
    }
    let stat = match host::lstat(&host::path(root, &clean)) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error(e.to_string())),
    };
    if stat.mode & fm::SYMLINK == 0 {
        return Ok(None);
    }
    let link = go::clean(&stat.link);
    if go::is_abs(&link) {
        return Ok(Some(vec![link]));
    }
    Ok(Some(vec![go::join(&[b"/", &go::join(&[&parent, &link])])]))
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

/// followlinks.go dedupePaths, over sorted paths: none at all for `.`, and nothing
/// below a path already kept.
fn dedupe(paths: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut last: Vec<u8> = Vec::new();
    for s in paths {
        if s == b"." {
            return Vec::new();
        }
        if s.starts_with(&[last.as_slice(), b"/"].concat()) {
            continue;
        }
        last = s.clone();
        out.push(s);
    }
    out
}

/// The disk writer's calls for each path sent, then each directory's time restored.
fn receive(
    root: &Path,
    sent: Vec<(Vec<u8>, Stat)>,
    sources: &mut Sources,
    now: (i64, u32),
    stage: &mut host::Stage,
    pack: u32,
    whole: &std::collections::HashSet<Vec<u8>>,
) -> Result<Fs, Error> {
    let data_of = |sources: &mut Sources, taken: host::Taken, size: u64| match taken {
        host::Taken::Pack(offset) => Ok(shards_image::erofs::DataRef { source: pack, offset }),
        host::Taken::File(path) => sources.host(path, size).map_err(|e| Error(e.to_string())),
    };
    let mut read: Vec<Read> = Vec::new();
    let mut fs = Fs::new(
        Tree::new(Meta {
            mode: 0o755,
            mtime: now.0,
            mtime_nsec: now.1,
            ..Meta::default()
        }),
        now,
    );
    let os = |e: vfs::PathError| Error(e.to_string());
    let mut dirs: Vec<(Vec<u8>, (i64, u32))> = Vec::new();
    for (rel, mut st) in sent {
        let p = vfs::join(b"/", &rel);
        let mode = st.mode;
        if mode & fm::DIR != 0 {
            fs.mkdir(&p, syscall_mode(mode)).map_err(os)?;
            dirs.push((p.clone(), st.mtime));
        } else if mode & (fm::DEVICE | fm::NAMED_PIPE) != 0 {
            let kind = if mode & fm::CHAR_DEVICE != 0 {
                Kind::CharDevice {
                    major: st.devmajor,
                    minor: st.devminor,
                }
            } else if mode & fm::NAMED_PIPE != 0 {
                Kind::Fifo
            } else {
                Kind::BlockDevice {
                    major: st.devmajor,
                    minor: st.devminor,
                }
            };
            fs.mknod(&p, kind, syscall_mode(mode) & 0o7777).map_err(os)?;
        } else if mode & fm::SYMLINK != 0 {
            fs.symlink(&st.link, &p).map_err(os)?;
        } else if !st.link.is_empty() {
            fs.link(&vfs::join(b"/", &st.link), &p).map_err(os)?;
        } else if st.socket {
            fs.create(&p, syscall_mode(mode)).map_err(os)?;
        } else {
            // The bytes and the stat they go with, as one version of the file: a small
            // one of a whole directory read in place, the directory's clone to say so.
            let src = host::path(root, &rel);
            let dir = whole_of(whole, &rel).filter(|_| st.size < host::CLONE_MIN);
            let (got, taken, seen) = match dir {
                Some(_) => stage.read(&src).map(|(g, t, s)| (g, t, Some(s))),
                None => stage.take(&src).map(|(g, t)| (g, t, None)),
            }
            .map_err(|e| Error(format!("{}: {e}", src.display())))?;
            st.mode = (st.mode & !(fm::PERM | fm::SETUID | fm::SETGID | fm::STICKY)) | got.mode;
            st.size = got.size;
            st.mtime = got.mtime;
            let id = fs.create(&p, syscall_mode(st.mode)).map_err(os)?;
            let data = data_of(sources, taken, st.size)?;
            fs.set_data(id, st.size, data);
            if let (Some(dir), Some(seen)) = (dir, seen) {
                read.push(Read {
                    dir: dir.to_vec(),
                    rel,
                    id,
                    got,
                    seen,
                });
            }
        }
        rewrite(&mut fs, &p, &st).map_err(os)?;
    }
    // Each whole directory a file was read in place in, cloned at once: the clone waits
    // out a write under way in any of them, whose end its times then show. Where it is not
    // cloned, each of its files is taken again on its own, and is what it was or refused.
    let mut cloned = std::collections::HashMap::new();
    for r in read {
        let src = host::path(root, &r.rel);
        let changed = || Error(format!("{}: file changed as the build read it", src.display()));
        let dir = match cloned.get(&r.dir) {
            Some(&done) => done,
            None => {
                let at = host::path(root, &r.dir);
                let done = stage
                    .clone_dir(&at, &r.dir)
                    .map_err(|e| Error(format!("{}: {e}", at.display())))?;
                cloned.insert(r.dir.clone(), done);
                done
            }
        };
        if dir {
            if !stage
                .still(&src, &r.seen)
                .map_err(|e| Error(format!("{}: {e}", src.display())))?
            {
                return Err(changed());
            }
            continue;
        }
        let (again, taken) = stage
            .take(&src)
            .map_err(|e| Error(format!("{}: {e}", src.display())))?;
        if again != r.got {
            return Err(changed());
        }
        let data = data_of(sources, taken, again.size)?;
        fs.set_data(r.id, again.size, data);
    }
    // The disk writer stamps directories once everything below them is written.
    for (p, t) in dirs.iter().rev() {
        fs.utimes(p, *t).map_err(os)?;
    }
    fs.begin();
    Ok(fs)
}

/// diskwriter_unix.go rewriteMetadata: xattrs as far as Linux takes them, root's
/// ownership, the mode, then the time.
fn rewrite(fs: &mut Fs, p: &[u8], st: &Stat) -> Result<(), vfs::PathError> {
    for (k, v) in &st.xattrs {
        let _ = fs.setxattr(p, k, v, true);
    }
    fs.lchown(p, 0, 0)?;
    if st.mode & fm::SYMLINK == 0 {
        fs.chmod(p, syscall_mode(st.mode))?;
    }
    fs.utimes(p, st.mtime)
}

/// A list as Go's `%s` prints a `[]string`.
fn go_list(items: &[Vec<u8>]) -> String {
    let parts: Vec<String> = items.iter().map(|i| show(i)).collect();
    format!("[{}]", parts.join(" "))
}
