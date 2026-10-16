//! A client's build context, kept in step on the machine that builds it (D180 part 3c). The
//! machine keeps a copy of each context a client builds there; the client walks its context
//! as a build walks one (fsutil's walk, the build's ignore file's excludes), compares what
//! the copy holds, and sends what changed, as a layer the copy is brought in step by
//! (shards_archive's `pack_changes` and `apply_layer`): a context unchanged sends nothing,
//! as BuildKit's session sends a receiver only the files it asks for. The build file and the
//! ignore file beside it, a few kilobytes, go whole each time, ahead of the layer.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
#[cfg(unix)]
use std::io::{self, Read};
#[cfg(unix)]
use std::path::{Path, PathBuf};

use shards_build::host::Stat;

/// What names a step of a client's context to the machine (`manifest`, `build`): no command
/// line's word, as no command line can hold NUL.
pub(crate) const SYNC_STEP: &str = "\u{0}sync";

/// What stands, in a build's command line as a client sends it, for the machine's copy of its
/// context, and of its build file's directory, which the machine names.
pub(crate) const CONTEXT: &str = "\u{0}context";
pub(crate) const DOCKERFILE: &str = "\u{0}dockerfile/";

#[cfg(unix)]
/// A word of a build's command line on the machine: [`CONTEXT`] its copy of the context,
/// [`DOCKERFILE`]'s files its copy of the build file's directory's, after any flag's `=`.
pub(crate) fn placed(word: &str, context: &Path, dockerfile: &Path) -> std::ffi::OsString {
    if word == CONTEXT {
        return context.as_os_str().to_os_string();
    }
    let (flag, value) = match word.split_once('=') {
        Some((flag, value)) if flag.starts_with("--") => (format!("{flag}="), value),
        _ => (String::new(), word),
    };
    match value.strip_prefix(DOCKERFILE) {
        Some(name) => {
            let mut placed = std::ffi::OsString::from(flag);
            placed.push(dockerfile.join(name).as_os_str());
            placed
        }
        None => std::ffi::OsString::from(word),
    }
}

/// What starts what a client sends of a build: the build file's directory, then the layer.
const MAGIC: &[u8] = b"shards context 1\n";

#[cfg(unix)]
/// The most a build file's directory sends: as much as a build reads of a Dockerfile.
const DOCKERFILE_MAX: u64 = 64 << 20;

/// Go's `os.ModeDir` and `os.ModeSymlink`, in a walked entry's mode.
const MODE_DIR: u32 = 1 << 31;
const MODE_SYMLINK: u32 = 1 << 27;

/// What tells an entry of a context apart from what a copy of it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seen {
    pub mode: u32,
    pub size: u64,
    pub mtime: (i64, u32),
    pub link: Vec<u8>,
}

impl Seen {
    pub(crate) fn of(stat: &Stat) -> Seen {
        Seen {
            mode: stat.mode,
            size: stat.size,
            mtime: stat.mtime,
            link: stat.link.clone(),
        }
    }

    /// Whether `self`, as the client has it, is what `copy` holds: a directory by its mode
    /// and time, its size being its filesystem's; a symlink by its target and time, its
    /// mode being its system's; anything else by its mode, size, time and link.
    fn same(&self, copy: &Seen) -> bool {
        if self.mode & MODE_DIR != 0 {
            return self.mode == copy.mode && self.mtime == copy.mtime;
        }
        if self.mode & MODE_SYMLINK != 0 {
            return copy.mode & MODE_SYMLINK != 0 && self.link == copy.link && self.mtime == copy.mtime;
        }
        self == copy
    }
}

/// What a copy holds, by each entry's path.
pub(crate) type Manifest = BTreeMap<Vec<u8>, Seen>;

#[cfg(unix)]
/// A copy's entries, walked as a build walks its context, nothing excluded.
pub(crate) fn manifest_of(dir: &Path) -> Result<Manifest, String> {
    if !dir.is_dir() {
        return Ok(Manifest::new());
    }
    let walked = shards_build::context::walk(dir, &shards_build::context::Filters::default())
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(walked
        .into_iter()
        .map(|(path, stat)| (path, Seen::of(&stat)))
        .collect())
}

fn put(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.extend_from_slice(bytes);
}

#[cfg(unix)]
/// The manifest as it crosses: each entry's path and link, length first, and its mode,
/// size and time.
pub(crate) fn encode(manifest: &Manifest) -> Vec<u8> {
    let mut out = Vec::new();
    for (path, seen) in manifest {
        put(&mut out, path);
        out.extend_from_slice(&seen.mode.to_be_bytes());
        out.extend_from_slice(&seen.size.to_be_bytes());
        out.extend_from_slice(&seen.mtime.0.to_be_bytes());
        out.extend_from_slice(&seen.mtime.1.to_be_bytes());
        put(&mut out, &seen.link);
    }
    out
}

/// A reader of what [`encode`] wrote.
struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        if self.0.len() < n {
            return Err("a context's manifest cut short".into());
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        self.take(N)?
            .try_into()
            .map_err(|_| "a context's manifest cut short".to_string())
    }

    fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let n = u32::from_be_bytes(self.array()?);
        Ok(self
            .take(usize::try_from(n).map_err(|e| e.to_string())?)?
            .to_vec())
    }
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Manifest, String> {
    let mut c = Cursor(bytes);
    let mut manifest = Manifest::new();
    while !c.0.is_empty() {
        let path = c.bytes()?;
        let mode = u32::from_be_bytes(c.array()?);
        let size = u64::from_be_bytes(c.array()?);
        let mtime = (i64::from_be_bytes(c.array()?), u32::from_be_bytes(c.array()?));
        let link = c.bytes()?;
        manifest.insert(
            path,
            Seen {
                mode,
                size,
                mtime,
                link,
            },
        );
    }
    Ok(manifest)
}

/// What brings a copy in step with the client's walk of it.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Each entry new or changed, with the directories it is in, so that their times are
    /// set again once it is in place, in the walk's order.
    pub changed: Vec<Vec<u8>>,
    /// The topmost of what went, a directory's removal taking all it held.
    pub removed: Vec<Vec<u8>>,
    /// Entries the copy holds already, unchanged, that a changed one is a hard link of:
    /// linked to there (`shards_archive::pack_changes`), where alone the change went as a
    /// file of its own, its bytes sent again, and the copy's tree no longer the client's.
    pub linked: Vec<Vec<u8>>,
}

/// What brings the copy `there` in step with the walk `here`.
pub(crate) fn plan(here: &[(Vec<u8>, Stat)], there: &Manifest) -> Plan {
    let kept: BTreeSet<&[u8]> = here.iter().map(|(p, _)| p.as_slice()).collect();
    let mut changed: BTreeSet<Vec<u8>> = BTreeSet::new();
    for (path, stat) in here {
        if there.get(path).is_some_and(|copy| Seen::of(stat).same(copy)) {
            continue;
        }
        changed.insert(path.clone());
    }
    let mut removed: Vec<Vec<u8>> = Vec::new();
    for path in there.keys() {
        if kept.contains(path.as_slice()) || removed.iter().any(|r| within(path, r)) {
            continue;
        }
        removed.push(path.clone());
    }
    // A directory a change is in has its time set again after it.
    for path in changed.clone().iter().chain(removed.iter()) {
        let mut at = path.as_slice();
        while let Some(i) = at.iter().rposition(|&b| b == b'/') {
            at = at.get(..i).unwrap_or_default();
            if kept.contains(at) {
                changed.insert(at.to_vec());
            }
        }
    }
    // The inodes of what changed that more than one name holds, and the unchanged names
    // that hold them too (a walk sees no directory's).
    let inodes: BTreeSet<u64> = here
        .iter()
        .filter(|(p, _)| changed.contains(p))
        .filter_map(|(_, s)| s.inode.filter(|&(_, links)| links > 1).map(|(ino, _)| ino))
        .collect();
    let linked: Vec<Vec<u8>> = here
        .iter()
        .filter(|(p, s)| {
            !changed.contains(p)
                && s.inode
                    .is_some_and(|(ino, links)| links > 1 && inodes.contains(&ino))
        })
        .map(|(p, _)| p.clone())
        .collect();
    // In the walk's order, parents before what they hold.
    let order: BTreeMap<&[u8], usize> = here
        .iter()
        .enumerate()
        .map(|(i, (p, _))| (p.as_slice(), i))
        .collect();
    let mut changed: Vec<Vec<u8>> = changed.into_iter().collect();
    changed.sort_by_key(|p| order.get(p.as_slice()).copied().unwrap_or(usize::MAX));
    Plan {
        changed,
        removed,
        linked,
    }
}

/// Whether `path` is `dir` or within it.
fn within(path: &[u8], dir: &[u8]) -> bool {
    path == dir || (path.starts_with(dir) && path.get(dir.len()) == Some(&b'/'))
}

/// One file of a build file's directory as it crosses: its name, its mode and time, and
/// what it holds.
pub(crate) struct Carried {
    pub name: String,
    pub mode: u32,
    pub mtime: (i64, u32),
    pub bytes: Vec<u8>,
}

/// Writes what a client sends of a build: the build file's directory's files, then the
/// layer `pack` writes.
pub(crate) fn send(
    out: &mut dyn Write,
    files: &[Carried],
    pack: impl FnOnce(&mut dyn Write) -> Result<(), String>,
) -> Result<(), String> {
    let mut head = MAGIC.to_vec();
    head.extend_from_slice(&u32::try_from(files.len()).unwrap_or(u32::MAX).to_be_bytes());
    for f in files {
        put(&mut head, f.name.as_bytes());
        head.extend_from_slice(&f.mode.to_be_bytes());
        head.extend_from_slice(&f.mtime.0.to_be_bytes());
        head.extend_from_slice(&f.mtime.1.to_be_bytes());
        head.extend_from_slice(&u64::try_from(f.bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
        head.extend_from_slice(&f.bytes);
    }
    out.write_all(&head)
        .map_err(|e| format!("sending the build file: {e}"))?;
    pack(out)
}

#[cfg(unix)]
fn read_array<const N: usize>(input: &mut dyn Read) -> Result<[u8; N], String> {
    let mut b = [0u8; N];
    input
        .read_exact(&mut b)
        .map_err(|e| format!("reading the build file: {e}"))?;
    Ok(b)
}

#[cfg(unix)]
fn read_vec(input: &mut dyn Read, n: u64) -> Result<Vec<u8>, String> {
    if n > DOCKERFILE_MAX {
        return Err(format!("a build file's directory past {DOCKERFILE_MAX} bytes"));
    }
    let mut v = Vec::new();
    input
        .take(n)
        .read_to_end(&mut v)
        .map_err(|e| format!("reading the build file: {e}"))?;
    if u64::try_from(v.len()).ok() != Some(n) {
        return Err("a build file cut short".into());
    }
    Ok(v)
}

#[cfg(unix)]
/// Reads what [`send`] wrote, on the machine: the build file's directory written whole into
/// `dockerfile`, made afresh, then the layer applied over `context`. Names are a file's
/// alone, no path. Returns the bytes the layer's files held.
pub(crate) fn receive(input: &mut dyn Read, dockerfile: &Path, context: &Path) -> Result<i64, String> {
    let mut magic = vec![0u8; MAGIC.len()];
    input
        .read_exact(&mut magic)
        .map_err(|e| format!("reading the build file: {e}"))?;
    if magic != MAGIC {
        return Err("not a context of a shards client's".into());
    }
    let count = u32::from_be_bytes(read_array(input)?);
    if dockerfile.exists() {
        std::fs::remove_dir_all(dockerfile).map_err(|e| format!("{}: {e}", dockerfile.display()))?;
    }
    std::fs::create_dir_all(dockerfile).map_err(|e| format!("{}: {e}", dockerfile.display()))?;
    let mut total: u64 = 0;
    for _ in 0..count {
        let len = u32::from_be_bytes(read_array(input)?);
        let name = String::from_utf8(read_vec(input, u64::from(len))?)
            .map_err(|_| "a build file's name that is not UTF-8".to_string())?;
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
            return Err(format!("{name:?}: a build file's name is a name alone"));
        }
        let mode = u32::from_be_bytes(read_array(input)?);
        let mtime = (
            i64::from_be_bytes(read_array(input)?),
            u32::from_be_bytes(read_array(input)?),
        );
        let size = u64::from_be_bytes(read_array(input)?);
        total = total.saturating_add(size);
        if total > DOCKERFILE_MAX {
            return Err(format!("a build file's directory past {DOCKERFILE_MAX} bytes"));
        }
        let bytes = read_vec(input, size)?;
        write_file(&dockerfile.join(&name), mode, mtime, &bytes)?;
    }
    std::fs::create_dir_all(context).map_err(|e| format!("{}: {e}", context.display()))?;
    let opts = shards_archive::UnpackOptions {
        no_lchown: true,
        best_effort_xattrs: true,
        ..shards_archive::UnpackOptions::default()
    };
    shards_archive::apply_layer(input, context, &opts).map_err(|e| format!("applying the context: {e}"))
}

#[cfg(unix)]
/// `bytes` as the file `path`, of `mode`'s permissions and time `mtime`.
fn write_file(path: &Path, mode: u32, mtime: (i64, u32), bytes: &[u8]) -> Result<(), String> {
    let at = |e: io::Error| format!("{}: {e}", path.display());
    let mut file = std::fs::File::create(path).map_err(at)?;
    file.write_all(bytes).map_err(at)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(mode & 0o777))
            .map_err(at)?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    let secs = u64::try_from(mtime.0).unwrap_or(0);
    file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::new(secs, mtime.1))
        .map_err(at)
}

/// The variable a client names its own paths in, for what a command of its copy prints:
/// its context's, then a NUL, then its build file's directory's, which may be none.
pub(crate) const SHOWN_AS: &str = "SHARDS_SYNCED_AS";

#[cfg(unix)]
/// What a command run of a client's copy prints, its copy's paths said as the client's own
/// (each pair the copy's path, then the client's), as it comes: all but a tail that may yet
/// begin one, held until what follows says.
pub(crate) struct Shown<R> {
    inner: R,
    pairs: Vec<(Vec<u8>, Vec<u8>)>,
    held: Vec<u8>,
    ready: Vec<u8>,
    at: usize,
    ended: bool,
}

#[cfg(unix)]
impl<R: Read> Shown<R> {
    pub(crate) fn new(inner: R, pairs: Vec<(Vec<u8>, Vec<u8>)>) -> Shown<R> {
        let mut pairs: Vec<(Vec<u8>, Vec<u8>)> =
            pairs.into_iter().filter(|(from, _)| !from.is_empty()).collect();
        // The longest first, which a shorter one it begins with would otherwise take.
        pairs.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        Shown {
            inner,
            pairs,
            held: Vec::new(),
            ready: Vec::new(),
            at: 0,
            ended: false,
        }
    }
}

#[cfg(unix)]
impl<R: Read> Read for Shown<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at >= self.ready.len() {
            if self.ended && self.held.is_empty() {
                return Ok(0);
            }
            let mut chunk = [0u8; 8192];
            let n = if self.ended {
                0
            } else {
                self.inner.read(&mut chunk)?
            };
            self.ended |= n == 0;
            self.held.extend_from_slice(chunk.get(..n).unwrap_or_default());
            let (shown, used) = replaced(&self.held, &self.pairs, self.ended);
            self.held.drain(..used.min(self.held.len()));
            self.ready = shown;
            self.at = 0;
        }
        let rest = self.ready.get(self.at..).unwrap_or_default();
        let n = rest.len().min(buf.len());
        if let (Some(to), Some(from)) = (buf.get_mut(..n), rest.get(..n)) {
            to.copy_from_slice(from);
        }
        self.at += n;
        Ok(n)
    }
}

#[cfg(unix)]
/// `text` with each pair's path said as the other, as far as it can be said: all of it
/// where nothing follows (`ended`), else up to a tail that may yet begin a path. What it
/// says, and how much of `text` that was.
fn replaced(text: &[u8], pairs: &[(Vec<u8>, Vec<u8>)], ended: bool) -> (Vec<u8>, usize) {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    'scan: while let Some(rest) = text.get(i..).filter(|r| !r.is_empty()) {
        for (from, to) in pairs {
            if rest.starts_with(from) {
                out.extend_from_slice(to);
                i += from.len();
                continue 'scan;
            }
        }
        if !ended
            && pairs
                .iter()
                .any(|(from, _)| from.len() > rest.len() && from.starts_with(rest))
        {
            break;
        }
        // As it is, up to where a path could begin next.
        let next = rest
            .iter()
            .skip(1)
            .position(|b| pairs.iter().any(|(from, _)| from.first() == Some(b)))
            .map_or(rest.len(), |at| at + 1);
        out.extend_from_slice(rest.get(..next).unwrap_or_default());
        i += next;
    }
    (out, i)
}

#[cfg(unix)]
/// The copy a client's build keeps on the machine of `home`: `synced/CLIENT/KEY`, KEY the
/// hex the client names it by, of 64 hex digits.
pub(crate) fn copy_of(home: &Path, client: &str, key: &str) -> Result<PathBuf, String> {
    if key.len() != 64
        || !key
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(format!("{key:?}: a context's key is 64 hex digits"));
    }
    Ok(home.join("synced").join(client).join(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(mode: u32, size: u64, mtime: (i64, u32)) -> Stat {
        Stat {
            mode,
            size,
            mtime,
            inode: None,
            devmajor: 0,
            devminor: 0,
            link: Vec::new(),
            xattrs: BTreeMap::new(),
            socket: false,
        }
    }

    #[test]
    fn what_changed_and_what_went_is_sent_alone() {
        let dir = MODE_DIR | 0o755;
        let here = vec![
            (b"a".to_vec(), stat(dir, 64, (1, 0))),
            (b"a/kept".to_vec(), stat(0o644, 4, (1, 0))),
            (b"a/changed".to_vec(), stat(0o644, 5, (2, 7))),
            (b"new".to_vec(), stat(0o644, 3, (3, 0))),
        ];
        let mut there = Manifest::new();
        // A directory's size is its filesystem's: no change.
        there.insert(b"a".to_vec(), Seen::of(&stat(dir, 4096, (1, 0))));
        there.insert(b"a/kept".to_vec(), Seen::of(&stat(0o644, 4, (1, 0))));
        there.insert(b"a/changed".to_vec(), Seen::of(&stat(0o644, 5, (2, 6))));
        there.insert(b"gone".to_vec(), Seen::of(&stat(dir, 64, (1, 0))));
        there.insert(b"gone/inner".to_vec(), Seen::of(&stat(0o644, 1, (1, 0))));
        there.insert(b"a/went".to_vec(), Seen::of(&stat(0o644, 1, (1, 0))));
        let Plan {
            changed,
            removed,
            linked,
        } = plan(&here, &there);
        // The changed file's directory goes again, for its time; in the walk's order.
        assert_eq!(changed, [b"a".to_vec(), b"a/changed".to_vec(), b"new".to_vec()]);
        // The topmost of what went.
        assert_eq!(removed, [b"a/went".to_vec(), b"gone".to_vec()]);
        assert!(linked.is_empty(), "{linked:?}");
        // Nothing changed, nothing sent.
        let same: Manifest = here.iter().map(|(p, s)| (p.clone(), Seen::of(s))).collect();
        assert_eq!(plan(&here, &same), Plan::default());
        #[cfg(unix)]
        assert_eq!(decode(&encode(&there)).unwrap(), there);
    }

    /// The copy's paths in what a command prints are the client's, however its output
    /// comes apart: a byte at a time, a path cut across reads, a tail that only began one.
    #[cfg(unix)]
    #[test]
    fn what_a_copy_prints_names_the_clients_paths() {
        let pairs = vec![
            (b"/h/synced/l/k/context".to_vec(), b"/Users/me/ctx".to_vec()),
            (
                b"/h/synced/l/k/dockerfile".to_vec(),
                b"/Users/me/ctx/build".to_vec(),
            ),
        ];
        let text = b"ERROR: /h/synced/l/k/context/agent.json: invalid\n/h/synced/l/k/dockerfile/Dockerfile:3\nends /h/synced/l/k/con";
        let want = b"ERROR: /Users/me/ctx/agent.json: invalid\n/Users/me/ctx/build/Dockerfile:3\nends /h/synced/l/k/con";
        struct Dribble<'a>(&'a [u8], usize);
        impl Read for Dribble<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = self.1.min(buf.len()).min(self.0.len());
                let (now, rest) = self.0.split_at(n);
                buf.get_mut(..n).unwrap().copy_from_slice(now);
                self.0 = rest;
                Ok(n)
            }
        }
        for step in [1, 3, 7, 4096] {
            let mut shown = Vec::new();
            Shown::new(Dribble(text, step), pairs.clone())
                .read_to_end(&mut shown)
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&shown),
                String::from_utf8_lossy(want),
                "{step}"
            );
        }
    }

    /// A new name of a file the copy holds, unchanged, is linked to it there: its partner
    /// is named, and no other file that has more than one name.
    #[test]
    fn a_new_link_to_a_file_the_copy_holds_names_it() {
        let linked = |ino: u64, links: u64, mtime: i64| Stat {
            inode: Some((ino, links)),
            ..stat(0o644, 4, (mtime, 0))
        };
        let here = vec![
            (b"kept".to_vec(), linked(7, 2, 1)),
            (b"other".to_vec(), linked(8, 2, 1)),
            (b"other-too".to_vec(), linked(8, 2, 1)),
            (b"new".to_vec(), linked(7, 2, 1)),
        ];
        let there: Manifest = here
            .iter()
            .filter(|(p, _)| p != b"new")
            .map(|(p, s)| (p.clone(), Seen::of(s)))
            .collect();
        let planned = plan(&here, &there);
        assert_eq!(planned.changed, [b"new".to_vec()]);
        assert_eq!(planned.linked, [b"kept".to_vec()]);
        #[cfg(unix)]
        assert_eq!(decode(&encode(&there)).unwrap(), there);
    }

    #[cfg(unix)]
    #[test]
    fn a_copy_is_named_by_a_key_of_hex_alone() {
        let home = Path::new("/h");
        assert!(copy_of(home, "laptop", &"ab".repeat(32)).is_ok());
        for bad in [
            "../x",
            "",
            &"AB".repeat(32),
            &"a".repeat(63),
            &format!("{}/", "a".repeat(63)),
        ] {
            assert!(copy_of(home, "laptop", bad).is_err(), "{bad}");
        }
    }
}
