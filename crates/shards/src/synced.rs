//! A client's build context, kept in step on the machine that builds it (D180 part 3c). The
//! machine keeps a copy of each context a client builds there; the client walks its context
//! as a build walks one (fsutil's walk, the build's ignore file's excludes), compares what
//! the copy holds, and sends what changed, as a layer the copy is brought in step by
//! (shards_archive's `pack_changes` and `apply_layer`): a context unchanged sends nothing.
//! The build file and the
//! ignore file beside it, a few kilobytes, go whole each time, ahead of the layer.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use shards_build::host::Stat;

/// What names a step of a client's context to the machine (`manifest`, `build`): no command
/// line's word, as no command line can hold NUL.
pub(crate) const SYNC_STEP: &str = "\u{0}sync";

/// What stands, in a build's command line as a client sends it, for the machine's copy of its
/// context, and of its build file's directory, which the machine names.
pub(crate) const CONTEXT: &str = "\u{0}context";
pub(crate) const DOCKERFILE: &str = "\u{0}dockerfile/";
/// What stands for the SSH agent `ID` a build names (`\0agent/ID`): there, a descriptor the
/// build asks on for each connection to it (`/dev/fd/N`, build::ssh::asked_of), which no
/// path names; the client relays each to its own agent of that id.
pub(crate) const AGENT_SOCKET: &str = "\u{0}agent/";
/// What stands for another tree of the client's that a build names (`\0tree/KEY`): its copy
/// there, kept in step before the build ([`receive_tree`]).
pub(crate) const TREE: &str = "\u{0}tree/";
/// What stands for where the build writes an output of the client's (`\0out/NAME`): the
/// directory of this build's outputs there, which its step sends back as the build ends.
pub(crate) const OUT: &str = "\u{0}out/";
/// What stands for the `N`th secret a client sends with its build (`\0secret/N`), which the
/// machine's build reads from a pipe, the descriptor [`SECRET_FD`] + N.
pub(crate) const SECRET: &str = "\u{0}secret/";
pub(crate) const SECRET_FD: i32 = 3;

/// Where a build's words are put on the machine: the copies of its context and its build
/// file's directory, the directory of its outputs, and the client's copies.
pub(crate) struct Places<'a> {
    pub context: &'a Path,
    pub dockerfile: &'a Path,
    pub out: &'a Path,
    pub copies: &'a Path,
    /// The socket of each SSH agent the build names, by its id.
    pub agents: &'a BTreeMap<String, PathBuf>,
}

/// A word of a build's command line on the machine: [`CONTEXT`] its copy of the context; and
/// wherever they stand, [`DOCKERFILE`]'s files its copy of the build file's directory's,
/// each [`OUT`] the directory of its outputs, each [`TREE`] the copy of that tree, and a
/// [`SECRET`] the pipe its secret is read from.
pub(crate) fn placed(word: &str, places: &Places<'_>) -> std::ffi::OsString {
    if word == CONTEXT {
        return places.context.as_os_str().to_os_string();
    }
    // Wherever it stands: a flag's value, a field of a CSV one.
    let mut said = word
        .replace(OUT, &format!("{}/", places.out.display()))
        .replace(DOCKERFILE, &format!("{}/", places.dockerfile.display()));
    while let Some((before, after)) = said.split_once(TREE) {
        let (key, rest) = after.split_at_checked(64).unwrap_or((after, ""));
        let Ok(copy) = copy_of_key(places.copies, key) else {
            break;
        };
        said = format!("{before}{}{rest}", copy.join("context").display());
    }
    if let Some((before, n)) = said.split_once(SECRET)
        && let Some(fd) = n.parse::<i32>().ok().and_then(|n| n.checked_add(SECRET_FD))
    {
        said = format!("{before}/dev/fd/{fd}");
    }
    if let Some((before, id)) = said.split_once(AGENT_SOCKET)
        && let Some(socket) = places.agents.get(id)
    {
        said = format!("{before}{}", socket.display());
    }
    std::ffi::OsString::from(said)
}

/// The secrets of `secrets` its daemon gave this build at a number, on Windows
/// (`src=/dev/fd/N`, [`placed`]): each read whole from what was given there
/// (`shards_ipc::given_fd`), at most `max` bytes past which the store refuses it, and named
/// for the store (`buildflags::store`) by a variable no environment holds, a NUL its
/// first character, which the build answers from what was read. A number given nothing is
/// a path as any other, which no file of Windows' names.
#[cfg(windows)]
pub(crate) fn given_secrets(
    secrets: Vec<shards_cmdline::buildflags::Secret>,
    max: u64,
) -> Result<
    (
        Vec<shards_cmdline::buildflags::Secret>,
        BTreeMap<String, shards_cmdline::buildflags::SecretBytes>,
    ),
    String,
> {
    let mut given = BTreeMap::new();
    let mut kept = Vec::with_capacity(secrets.len());
    for mut secret in secrets {
        let at = secret
            .file
            .strip_prefix("/dev/fd/")
            .and_then(|n| n.parse::<i32>().ok());
        let handle = match at {
            Some(n) => shards_ipc::given_fd(n).map_err(|e| format!("{}: {e}", secret.file))?,
            None => None,
        };
        if let Some(handle) = handle {
            let mut bytes = Vec::new();
            std::fs::File::from(handle)
                .take(max.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(|e| format!("reading secret {}: {e}", secret.id))?;
            let name = format!("\u{0}given{}", secret.file);
            given.insert(name.clone(), shards_cmdline::buildflags::SecretBytes::new(bytes));
            secret.file = String::new();
            secret.env = name;
        }
        kept.push(secret);
    }
    Ok((kept, given))
}

/// What starts what a client sends of a build: the build file's directory, its secrets,
/// then the layer; and says the build's step frames its stdout ([`SAID`]).
const MAGIC: &[u8] = b"shards context 3\n";

/// What a build's step says on its stdout, each a frame of a tag, a length and bytes: the
/// build's own stdout as it comes ([`SAID`]); then, once the build is through, each output
/// of the client's it wrote, its name ([`FILE`] or [`DIR`]), its bytes ([`CHUNK`], a
/// directory's as its archive) and its end ([`DONE`]).
pub(crate) const SAID: u8 = 0;
pub(crate) const FILE: u8 = 1;
pub(crate) const DIR: u8 = 2;
pub(crate) const CHUNK: u8 = 3;
pub(crate) const DONE: u8 = 4;
/// A connection a step made to an SSH agent the build names, as it is made: its agent's id
/// and the token the client claims it by, which it then relays (the step `agent`).
pub(crate) const AGENT: u8 = 5;

/// A step's stdout, framed, a whole frame at a time from whichever thread says one: the
/// build's own output, and its agents' connections as they come.
pub(crate) struct Framed<W>(std::sync::Mutex<W>);

impl<W: Write> Framed<W> {
    pub(crate) fn new(out: W) -> Framed<W> {
        Framed(std::sync::Mutex::new(out))
    }

    pub(crate) fn frame(&self, tag: u8, bytes: &[u8]) -> io::Result<()> {
        let mut out = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        frame(&mut *out, tag, bytes)
    }

    pub(crate) fn into_inner(self) -> W {
        self.0
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A frame's most bytes: what a read of the build's stdout takes at once.
const FRAME_MOST: usize = 64 << 10;

fn frame(out: &mut dyn Write, tag: u8, bytes: &[u8]) -> io::Result<()> {
    out.write_all(&[tag])?;
    out.write_all(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes())?;
    out.write_all(bytes)
}

/// What writes [`CHUNK`] frames, a frame a write.
struct Chunks<'a>(&'a mut dyn Write);

impl Write for Chunks<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = bytes.len().min(FRAME_MOST);
        frame(self.0, CHUNK, bytes.get(..n).unwrap_or_default())?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// The build's stdout as [`SAID`] frames, as it comes, to its end.
pub(crate) fn say<W: Write>(mut build: impl Read, out: &Framed<W>) -> io::Result<()> {
    let mut buf = vec![0u8; FRAME_MOST];
    loop {
        let n = build.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        out.frame(SAID, buf.get(..n).unwrap_or_default())?;
    }
}

/// Each output the build wrote in `dir`, by its name, in its order: a file's bytes, a
/// directory's archive.
pub(crate) fn send_outputs(dir: &Path, out: &mut dyn Write) -> Result<(), String> {
    let at = |e: io::Error| format!("{}: {e}", dir.display());
    let mut names: Vec<std::ffi::OsString> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|e| e.map(|e| e.file_name()))
            .collect::<io::Result<_>>()
            .map_err(at)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(at(e)),
    };
    names.sort();
    for name in names {
        let path = dir.join(&name);
        let shown = name.to_string_lossy().into_owned();
        let sent = std::fs::symlink_metadata(&path).and_then(|meta| {
            if meta.is_dir() {
                frame(out, DIR, shown.as_bytes())?;
                shards_archive::pack(&path, &shards_archive::PackOptions::default(), Chunks(out))
                    .map(drop)
                    .map_err(io::Error::other)?;
            } else {
                frame(out, FILE, shown.as_bytes())?;
                io::copy(&mut std::fs::File::open(&path)?, &mut Chunks(out))?;
            }
            frame(out, DONE, &[])
        });
        sent.map_err(|e| format!("sending the build's output {shown}: {e}"))?;
    }
    Ok(())
}

/// Where a build's outputs go here, as they come back: a file's whole or not at all, a
/// directory's archive unpacked.
pub(crate) trait Outputs: Send {
    fn begin(&mut self, name: &str, dir: bool) -> io::Result<()>;
    fn chunk(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn end(&mut self) -> io::Result<()>;
    /// A step's connection to the agent `id`, to claim by `token` and relay.
    fn agent(&mut self, id: &str, token: &str) -> io::Result<()>;
}

/// What a build's step says, taken apart as it comes: the build's own stdout written to
/// `said`, its outputs given to `outputs`.
pub(crate) struct Demux<S, O> {
    said: S,
    outputs: O,
    held: Vec<u8>,
}

impl<S: Write, O: Outputs> Demux<S, O> {
    pub(crate) fn new(said: S, outputs: O) -> Demux<S, O> {
        Demux {
            said,
            outputs,
            held: Vec::new(),
        }
    }
}

impl<S: Write, O: Outputs> Write for Demux<S, O> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.held.extend_from_slice(bytes);
        let mut at = 0;
        while let Some((&tag, rest)) = self.held.get(at..).and_then(<[u8]>::split_first) {
            let Some((len, rest)) = rest.split_first_chunk::<4>() else {
                break;
            };
            let len = usize::try_from(u32::from_be_bytes(*len)).unwrap_or(usize::MAX);
            let Some(body) = rest.get(..len) else {
                break;
            };
            match tag {
                SAID => self.said.write_all(body)?,
                FILE | DIR => self.outputs.begin(&String::from_utf8_lossy(body), tag == DIR)?,
                CHUNK => self.outputs.chunk(body)?,
                DONE => self.outputs.end()?,
                AGENT => {
                    let said = String::from_utf8_lossy(body);
                    let (id, token) = said.split_once('\0').unwrap_or((&said, ""));
                    self.outputs.agent(id, token)?;
                }
                other => return Err(io::Error::other(format!("a build's step said {other}"))),
            }
            at += 5 + len;
        }
        self.held.drain(..at.min(self.held.len()));
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.said.flush()
    }
}

/// The most of a secret a build takes: what a step's frame carries, as a build here takes.
const SECRET_MAX: u64 = shards_abi::run::MAX_PAYLOAD as u64;

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

/// Writes what a client sends of a build: the build file's directory's files, its secrets
/// by id, each written as it is held, in no buffer of this function's, then the layer
/// `pack` writes.
pub(crate) fn send(
    out: &mut dyn Write,
    files: &[Carried],
    secrets: &[(String, shards_cmdline::buildflags::SecretBytes)],
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
    head.extend_from_slice(&u32::try_from(secrets.len()).unwrap_or(u32::MAX).to_be_bytes());
    out.write_all(&head)
        .map_err(|e| format!("sending the build file: {e}"))?;
    for (id, secret) in secrets {
        let mut named = Vec::new();
        put(&mut named, id.as_bytes());
        named.extend_from_slice(
            &u64::try_from(secret.bytes().len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        out.write_all(&named)
            .and_then(|()| out.write_all(secret.bytes()))
            .map_err(|e| format!("sending the build's secrets: {e}"))?;
    }
    pack(out)
}

fn read_array<const N: usize>(input: &mut dyn Read) -> Result<[u8; N], String> {
    let mut b = [0u8; N];
    input
        .read_exact(&mut b)
        .map_err(|e| format!("reading the build file: {e}"))?;
    Ok(b)
}

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

/// Reads what [`send`] wrote, on the machine: the build file's directory written whole into
/// `dockerfile`, made afresh, then the layer applied over `context`. Names are a file's
/// alone, no path. Returns the bytes the layer's files held.
pub(crate) fn receive(
    input: &mut dyn Read,
    dockerfile: &Path,
    context: &Path,
    named: usize,
) -> Result<(i64, Vec<(String, shards_cmdline::buildflags::SecretBytes)>), String> {
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
    // The secrets, in memory alone: no file of the machine's holds one.
    let count = u32::from_be_bytes(read_array(input)?);
    if usize::try_from(count).map_or(true, |count| count > named) {
        return Err(format!("{count} secrets, where the build names {named}"));
    }
    let mut secrets = Vec::new();
    for _ in 0..count {
        let len = u32::from_be_bytes(read_array(input)?);
        let id = String::from_utf8(read_vec(input, u64::from(len))?)
            .map_err(|_| "a secret's id that is not UTF-8".to_string())?;
        let size = u64::from_be_bytes(read_array(input)?);
        total = total.saturating_add(size);
        if size > SECRET_MAX || total > DOCKERFILE_MAX {
            return Err(format!(
                "{id}: a secret past {SECRET_MAX} bytes, or secrets past {DOCKERFILE_MAX}"
            ));
        }
        secrets.push((id, read_secret(input, size)?));
    }
    std::fs::create_dir_all(context).map_err(|e| format!("{}: {e}", context.display()))?;
    // A link's own time kept, as a tree's is ([`receive_tree`]): a build's context copied
    // with none had its symlink's time its making's, and an artifact made of it differed
    // from the one made of the client's (CI 41237707540, windows-2025).
    let opts = shards_archive::UnpackOptions {
        no_lchown: true,
        best_effort_xattrs: true,
        link_times: true,
        ..shards_archive::UnpackOptions::default()
    };
    let sent = shards_archive::apply_layer(input, context, &opts)
        .map_err(|e| format!("applying the context: {e}"))?;
    Ok((sent, secrets))
}

/// A secret of `n` bytes from `input`, read into a buffer of that length, held as a secret
/// before it is filled: no growth leaves a copy, and one cut short is overwritten too.
fn read_secret(input: &mut dyn Read, n: u64) -> Result<shards_cmdline::buildflags::SecretBytes, String> {
    let n = usize::try_from(n).map_err(|_| "a secret too long".to_string())?;
    let mut held = shards_cmdline::buildflags::SecretBytes::new(vec![0u8; n]);
    input
        .read_exact(held.bytes_mut())
        .map_err(|e| format!("reading the build's secrets: {e}"))?;
    Ok(held)
}

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
/// its context's, then a NUL, then its build file's directory's, which may be none; then,
/// a NUL before each, each tree's key and its path, a NUL between.
pub(crate) const SHOWN_AS: &str = "SHARDS_SYNCED_AS";

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

/// The copy a client's build keeps on the machine of `home`: `synced/CLIENT/KEY`, KEY the
/// hex the client names it by, of 64 hex digits.
pub(crate) fn copy_of(home: &Path, client: &str, key: &str) -> Result<PathBuf, String> {
    copy_of_key(&home.join("synced").join(client), key)
}

/// The copy `key` names among a client's `copies`.
fn copy_of_key(copies: &Path, key: &str) -> Result<PathBuf, String> {
    if key.len() != 64
        || !key
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(format!("{key:?}: a context's key is 64 hex digits"));
    }
    Ok(copies.join(key))
}

/// What starts what a client sends of another tree its build names: the layer of what
/// changed.
const TREE_MAGIC: &[u8] = b"shards tree 1\n";

/// Writes what a client sends of another tree: the layer `pack` writes.
pub(crate) fn send_tree(
    out: &mut dyn Write,
    pack: impl FnOnce(&mut dyn Write) -> Result<(), String>,
) -> Result<(), String> {
    out.write_all(TREE_MAGIC)
        .map_err(|e| format!("sending a tree: {e}"))?;
    pack(out)
}

/// What a client sends of another tree, brought into its copy, `context`: the bytes of files
/// that came.
pub(crate) fn receive_tree(input: &mut dyn Read, context: &Path) -> Result<i64, String> {
    let mut magic = vec![0u8; TREE_MAGIC.len()];
    input
        .read_exact(&mut magic)
        .map_err(|e| format!("reading a tree: {e}"))?;
    if magic != TREE_MAGIC {
        return Err("not a tree of a shards client's".into());
    }
    std::fs::create_dir_all(context).map_err(|e| format!("{}: {e}", context.display()))?;
    let opts = shards_archive::UnpackOptions {
        no_lchown: true,
        best_effort_xattrs: true,
        link_times: true,
        ..shards_archive::UnpackOptions::default()
    };
    shards_archive::apply_layer(input, context, &opts).map_err(|e| format!("applying a tree: {e}"))
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

    /// A build's secrets cross with its context, each named in its words by the pipe it is
    /// read from there; more than its words name are refused before any is read.
    #[cfg(unix)]
    #[test]
    fn a_builds_secrets_cross_as_pipes_it_names() {
        let places = Places {
            context: Path::new("/c"),
            dockerfile: Path::new("/d"),
            out: Path::new("/o"),
            copies: Path::new("/s"),
            agents: &BTreeMap::from([("default".to_string(), PathBuf::from("/dev/fd/5"))]),
        };
        let place = |w: &str| placed(w, &places);
        assert_eq!(
            place(&format!("--ssh=default={AGENT_SOCKET}default")),
            std::ffi::OsString::from("--ssh=default=/dev/fd/5")
        );
        let key = "ab".repeat(32);
        assert_eq!(
            place(&format!("--build-context=other={TREE}{key}")),
            std::ffi::OsString::from(format!("--build-context=other=/s/{key}/context"))
        );
        assert_eq!(
            place(&format!("--build-context=l=oci-layout://{TREE}{key}:v1")),
            std::ffi::OsString::from(format!("--build-context=l=oci-layout:///s/{key}/context:v1"))
        );
        // A key that is none is no copy's: the word goes as it is, for the build to refuse.
        assert_eq!(
            place(&format!("x={TREE}../etc")),
            std::ffi::OsString::from(format!("x={TREE}../etc"))
        );
        // A field of a CSV value as a flag's whole value is.
        assert_eq!(
            place(&format!("type=oci,dest={OUT}0,tar=false")),
            std::ffi::OsString::from("type=oci,dest=/o/0,tar=false")
        );
        assert_eq!(
            place(&format!("--file={DOCKERFILE}Dockerfile")),
            std::ffi::OsString::from("--file=/d/Dockerfile")
        );
        assert_eq!(
            place("--secret=id=tok,src=\u{0}secret/1"),
            std::ffi::OsString::from("--secret=id=tok,src=/dev/fd/4")
        );
        assert_eq!(
            place("--output=type=local,dest=\u{0}out/0"),
            std::ffi::OsString::from("--output=type=local,dest=/o/0")
        );
        assert_eq!(
            place("--iidfile=\u{0}out/iidfile"),
            std::ffi::OsString::from("--iidfile=/o/iidfile")
        );
        let held = |b: &[u8]| shards_cmdline::buildflags::SecretBytes::new(b.to_vec());
        let secrets = vec![
            ("tok".to_string(), held(b"one")),
            ("var".to_string(), held(b"two")),
        ];
        let mut sent = Vec::new();
        send(&mut sent, &[], &secrets, |out| {
            shards_archive::pack_changes(Path::new("/nowhere"), &[], &[], &[], out)
                .map(drop)
                .map_err(|e| e.to_string())
        })
        .unwrap();
        let dir = shards_testdir::TempDir::new("synced-secrets").unwrap();
        let (dockerfile, context) = (dir.join("dockerfile"), dir.join("context"));
        let (_, got) = receive(&mut &sent[..], &dockerfile, &context, 2).unwrap();
        let got: Vec<(&str, &[u8])> = got.iter().map(|(id, s)| (id.as_str(), s.bytes())).collect();
        assert_eq!(got, [("tok", &b"one"[..]), ("var", &b"two"[..])]);
        let refused = receive(&mut &sent[..], &dockerfile, &context, 1).unwrap_err();
        assert!(
            refused.contains("2 secrets, where the build names 1"),
            "{refused}"
        );
    }

    /// What a build's step says comes apart as it said it, however its frames are cut: the
    /// build's own stdout, then each output's name, bytes and end.
    #[cfg(unix)]
    #[test]
    fn a_builds_step_comes_apart_as_it_was_said() {
        #[derive(Default)]
        struct Got(Vec<String>);
        impl Outputs for Got {
            fn begin(&mut self, name: &str, dir: bool) -> io::Result<()> {
                self.0.push(format!("begin {name} {dir}"));
                Ok(())
            }
            fn chunk(&mut self, bytes: &[u8]) -> io::Result<()> {
                self.0.push(format!("chunk {}", String::from_utf8_lossy(bytes)));
                Ok(())
            }
            fn end(&mut self) -> io::Result<()> {
                self.0.push("end".into());
                Ok(())
            }
            fn agent(&mut self, id: &str, token: &str) -> io::Result<()> {
                self.0.push(format!("agent {id} {token}"));
                Ok(())
            }
        }
        let framed = Framed::new(Vec::new());
        say(&b"the build's own"[..], &framed).unwrap();
        framed.frame(AGENT, b"default\0tok").unwrap();
        let mut said = framed.into_inner();
        let dir = shards_testdir::TempDir::new("synced-out").unwrap();
        std::fs::write(dir.join("iidfile"), "sha256:x").unwrap();
        send_outputs(dir.as_ref(), &mut said).unwrap();
        for cut in [1, 5, said.len()] {
            let mut stdout = Vec::new();
            let mut demux = Demux::new(&mut stdout, Got::default());
            for piece in said.chunks(cut) {
                demux.write_all(piece).unwrap();
            }
            let got = std::mem::take(&mut demux.outputs.0);
            drop(demux);
            assert_eq!(stdout, b"the build's own", "{cut}");
            assert_eq!(
                got,
                [
                    "agent default tok",
                    "begin iidfile false",
                    "chunk sha256:x",
                    "end"
                ],
                "{cut}"
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
