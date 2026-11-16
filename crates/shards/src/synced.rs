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

/// A frame's most bytes, which bounds what a [`Demux`] holds: what one read of the build's
/// stdout, a pipe, takes at most, a pipe's capacity: 16 pages on Linux (`PIPE_DEF_BUFFERS`,
/// include/linux/pipe_fs_i.h l. 5; pipe(7): 65536 bytes) and 64 KiB on macOS
/// (`BIG_PIPE_SIZE`, xnu-12377.101.15 bsd/sys/pipe.h l. 80); Windows' anonymous pipes hold
/// less. An output's bytes go in frames as large.
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

/// The most of a secret a build takes: what a step's frame carries, as a build here takes
/// (`shards_abi::run::MAX_PAYLOAD`). Every secret BuildKit takes fits: its session refuses
/// one past `MaxSecretSize`, 500 KiB (BuildKit v0.33.0 session/secrets/secretsprovider/
/// secretsprovider.go l. 15).
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
        let time = same_time(self.mtime, copy.mtime);
        if self.mode & MODE_DIR != 0 {
            return self.mode == copy.mode && time;
        }
        if self.mode & MODE_SYMLINK != 0 {
            return copy.mode & MODE_SYMLINK != 0 && self.link == copy.link && time;
        }
        self.mode == copy.mode && self.size == copy.size && self.link == copy.link && time
    }
}

/// Whether the client's time `here` is the copy's `there`: the same, or `here` to the 100 ns
/// a Windows machine's NTFS keeps one to (an NT time's unit), where `there` is one it kept:
/// a Linux or macOS client's are to the nanosecond, and a copy that kept them so went whole
/// again at each build. A write the copy has not taken came after the build that took the
/// file's write before it, later by far more than 100 ns.
fn same_time(here: (i64, u32), there: (i64, u32)) -> bool {
    here == there || (here.0 == there.0 && there.1.is_multiple_of(100) && here.1 / 100 == there.1 / 100)
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
    let inodes: BTreeSet<u128> = here
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

/// Why a build file's name a client sent is not one file's of the directory it is written
/// in (remote-02): as the machine's own paths read it, it names another, or something no
/// file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotAName {
    /// Empty, `.` or `..`: no file, or the directory or its parent.
    Dots,
    /// A separator or NUL: a path, not a name (`/` everywhere, `\` too, as Windows reads it).
    Path,
    /// On Windows, a colon: a drive's (`C:name`, a path of drive C's current directory, which
    /// `Path::join` takes in place of the directory), or a file's alternate data stream
    /// (`name:stream`).
    Colon,
    /// On Windows, a character no Win32 file's name holds (`< > " | ? *`, and control
    /// characters 1-31), some of which are the `\\?\` and `\\.\` namespaces' and wildcards.
    Reserved,
    /// On Windows, a device's name, whatever extension follows it (`CON`, `NUL.txt`, `COM1`):
    /// the device, not a file of the directory.
    Device,
    /// On Windows, a name ending in a dot or a space, which Win32 takes off: another name's.
    Trailing,
}

impl std::fmt::Display for NotAName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            NotAName::Dots => "a build file's name is a file's, not `.` or `..`",
            NotAName::Path => "a build file's name is a name alone, no path",
            NotAName::Colon => "a build file's name on Windows holds no colon: a drive's or a stream's",
            NotAName::Reserved => {
                "a build file's name on Windows holds none of < > \" | ? * or a control character"
            }
            NotAName::Device => {
                "a build file's name on Windows is no device's (CON, PRN, AUX, NUL, COM0-9, LPT0-9)"
            }
            NotAName::Trailing => "a build file's name on Windows ends in no dot or space",
        })
    }
}

/// Whether `name` is one file's name in a directory, as `windows`' paths read it or Unix's.
/// Windows' rules are Microsoft's "Naming Files, Paths, and Namespaces" (Win32 file
/// namespaces: reserved characters, device names, no trailing dot or space) and its path
/// forms (drive-relative `C:name`, UNC, `\\?\`, `\\.\`), and CreateFileW's console names
/// (`CONIN$`, `CONOUT$`).
pub(crate) fn name_alone(name: &str, windows: bool) -> Result<(), NotAName> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(NotAName::Dots);
    }
    if name.contains(['/', '\\', '\0']) {
        return Err(NotAName::Path);
    }
    if !windows {
        return Ok(());
    }
    if name.contains(':') {
        return Err(NotAName::Colon);
    }
    if name
        .chars()
        .any(|c| matches!(c, '<' | '>' | '"' | '|' | '?' | '*' | '\u{1}'..='\u{1f}'))
    {
        return Err(NotAName::Reserved);
    }
    if name.ends_with(['.', ' ']) {
        return Err(NotAName::Trailing);
    }
    // A device's name, with any extension: the part before the first dot, its trailing
    // spaces aside, as Win32 reads `NUL .txt` as NUL.
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    let numbered = |prefix: &str| {
        upper.strip_prefix(prefix).is_some_and(|n| {
            let mut digits = n.chars();
            matches!(
                (digits.next(), digits.next()),
                (Some('0'..='9' | '\u{b9}' | '\u{b2}' | '\u{b3}'), None)
            )
        })
    };
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || numbered("COM")
        || numbered("LPT")
    {
        return Err(NotAName::Device);
    }
    Ok(())
}

/// The forms of a `--build-context NAME=VALUE` that name no directory of the machine's (a
/// URL, a Git remote, an image, another stage), which a client's shards sends as they are
/// (context.rs, `named_here`); any other value is a path.
pub(crate) const ELSEWHERE: [&str; 7] = [
    "http://",
    "https://",
    "git://",
    "github.com/",
    "git@",
    "docker-image://",
    "target:",
];

/// A path a build's words name, by the flag that names it (`PATH` its context's argument).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Named {
    pub flag: &'static str,
    pub path: String,
}

/// Why a client's build's words may not run on the machine (remote-09): judged before any of
/// its files come, and again where the build runs for a client (`build::build`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotHeld {
    /// A flag's path that is no part of the client's copies: as it came, no marker of its
    /// copy's (`\0out/NAME`, `\0tree/KEY`...); placed, no path inside them.
    Outside(&'static str),
    /// A flag's form that reads the machine's own rather than a path: a secret of its
    /// environment (`type=env`, or no `src`), an SSH agent of its own (`--ssh default`).
    Machines(&'static str),
    /// Words a build reads as no build.
    Unread,
}

impl std::fmt::Display for NotHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotHeld::Outside(flag) => write!(
                f,
                "--{flag}: a client's build names paths of its own copies on this machine alone"
            ),
            NotHeld::Machines(flag) => write!(
                f,
                "--{flag}: a client's build reads no secret or agent of this machine's, only those its shards sends"
            ),
            NotHeld::Unread => f.write_str("a client's build step's words are no build's"),
        }
    }
}

/// Every path a build's `words` name, each with its flag: its context, its build file, each
/// output's and cache's directory or file, each named context's and OCI layout's directory,
/// each secret's file and SSH agent's socket or key, and each policy's file. buildx v0.37.1's
/// path-typed values (`commands.rs` BUILD: `--file`, `--iidfile`, `--metadata-file`,
/// `--output`, `--cache-from`, `--cache-to`, `--build-context`, `--secret`, `--ssh`,
/// `--policy`); its other flags name no path. A secret or an agent of no path is the machine's.
/// An agent's, a harness's or an MCP server's build names its directory alone.
pub(crate) fn named_paths(words: &[String]) -> Result<Vec<Named>, NotHeld> {
    let said = crate::cli::command_words(words);
    let refs: Vec<&str> = said.iter().map(String::as_str).collect();
    if let ["agent" | "harness" | "mcp", "build", rest @ ..] = refs.as_slice() {
        let kind = crate::agent::kind_of(refs.first().copied().unwrap_or_default()).ok_or(NotHeld::Unread)?;
        let rest: Vec<String> = rest.iter().map(|w| (*w).to_string()).collect();
        let args = crate::agent::build_args(kind, &rest).map_err(|_| NotHeld::Unread)?;
        return Ok(vec![Named {
            flag: "PATH",
            path: args.dir.to_string_lossy().into_owned(),
        }]);
    }
    let n = shards_cmdline::commands::build(&refs).ok_or(NotHeld::Unread)?;
    let rest = said.get(n..).unwrap_or_default();
    let shards_cmdline::flags::Outcome::Run(parsed) = shards_cmdline::flags::parse(
        &shards_cmdline::commands::BUILD,
        "shards buildx build",
        rest,
        &shards_cmdline::buildflags::validate,
    ) else {
        return Err(NotHeld::Unread);
    };
    use shards_cmdline::buildflags;
    let mut named = Vec::new();
    let mut path = |flag: &'static str, path: &str| {
        named.push(Named {
            flag,
            path: path.to_string(),
        })
    };
    for arg in &parsed.args {
        path("PATH", arg);
    }
    for flag in ["file", "iidfile", "metadata-file"] {
        if !parsed.string(flag).is_empty() {
            path(flag, parsed.string(flag));
        }
    }
    for e in buildflags::parse_exports(parsed.many("output")).map_err(|_| NotHeld::Unread)? {
        if !e.dest.is_empty() && e.dest != "-" {
            path("output", &e.dest);
        }
    }
    for flag in ["cache-from", "cache-to"] {
        for e in buildflags::cache_entries(parsed.many(flag), &|_| None).map_err(|_| NotHeld::Unread)? {
            for field in ["src", "dest"] {
                if let Some(p) = e.attrs.get(field) {
                    path(flag, p);
                }
            }
        }
    }
    for value in parsed.many("build-context") {
        let (_, v) = value.split_once('=').ok_or(NotHeld::Unread)?;
        if ELSEWHERE.iter().any(|p| v.starts_with(p)) {
            continue;
        }
        match crate::build::parse_oci_layout(v).map_err(|_| NotHeld::Unread)? {
            Some(layout) => path("build-context", &layout.path),
            None => path("build-context", v),
        }
    }
    for secret in buildflags::parse_secrets(parsed.many("secret")).map_err(|_| NotHeld::Unread)? {
        if !secret.env.is_empty() || secret.file.is_empty() {
            return Err(NotHeld::Machines("secret"));
        }
        path("secret", &secret.file);
    }
    for ssh in buildflags::parse_ssh(parsed.many("ssh")) {
        if ssh.paths.is_empty() {
            return Err(NotHeld::Machines("ssh"));
        }
        for p in &ssh.paths {
            path("ssh", p);
        }
    }
    for policy in buildflags::parse_policies(parsed.many("policy")).map_err(|_| NotHeld::Unread)? {
        for file in &policy.files {
            path("policy", file);
        }
    }
    Ok(named)
}

/// Whether `path` is a name within a context, as a policy's file is read within it (buildx's
/// os.Root of the context): relative, each of its parts, split at `/`, one file's name as
/// this machine's paths read one ([`name_alone`]).
fn in_context(path: &str) -> bool {
    path.split('/')
        .all(|part| name_alone(part, cfg!(windows)).is_ok())
}

/// Whether a client's build's `words`, as its shards sends them, name no path but its copies'
/// on this machine (remote-09): its context [`CONTEXT`]; its build file one of [`DOCKERFILE`]'s;
/// each file or directory it writes one of [`OUT`]'s; each tree it reads a [`TREE`]'s copy;
/// each secret one of the `secrets` it sends ([`SECRET`]); each agent one of its own
/// ([`AGENT_SOCKET`]); each policy's file a name within its context. Judged before any of
/// its files come; anything else is refused.
pub(crate) fn held_to_copies(words: &[String], secrets: usize) -> Result<(), NotHeld> {
    let name = |marker: &str, path: &str| {
        path.strip_prefix(marker)
            .is_some_and(|n| name_alone(n, cfg!(windows)).is_ok())
    };
    for Named { flag, path } in named_paths(words)? {
        let held = match flag {
            "PATH" => path == CONTEXT,
            "file" => name(DOCKERFILE, &path),
            "iidfile" | "metadata-file" | "output" | "cache-to" => name(OUT, &path),
            "cache-from" | "build-context" => path
                .strip_prefix(TREE)
                .is_some_and(|key| copy_of_key(Path::new(""), key).is_ok()),
            "secret" => path
                .strip_prefix(SECRET)
                .and_then(|n| {
                    n.parse::<usize>()
                        .ok()
                        .filter(|_| n.bytes().all(|b| b.is_ascii_digit()))
                })
                .is_some_and(|n| n < secrets),
            "ssh" => name(AGENT_SOCKET, &path),
            "policy" => in_context(&path),
            _ => false,
        };
        if !held {
            return Err(NotHeld::Outside(flag));
        }
    }
    Ok(())
}

/// Whether a build's `words`, run for a client of this machine's (`client_grants::CLIENT_VAR`),
/// name no path but within `copies`, the client's copies here, once [`placed`] (remote-09):
/// each a path beneath them, no part of it `..` or a prefix, as Windows' paths read one too; a
/// secret's or an agent's a descriptor the build was given (`/dev/fd/N`); a policy's file a
/// name within its context.
pub(crate) fn held_within(words: &[String], copies: &Path) -> Result<(), NotHeld> {
    let given = |path: &str| {
        path.strip_prefix("/dev/fd/")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    let beneath = |path: &str| {
        Path::new(path).strip_prefix(copies).is_ok_and(|rest| {
            rest.components().all(|c| match c {
                std::path::Component::Normal(n) => {
                    n.to_str().is_some_and(|n| name_alone(n, cfg!(windows)).is_ok())
                }
                _ => false,
            })
        })
    };
    for Named { flag, path } in named_paths(words)? {
        let held = match flag {
            "secret" | "ssh" => given(&path),
            "policy" => in_context(&path),
            _ => beneath(&path),
        };
        if !held {
            return Err(NotHeld::Outside(flag));
        }
    }
    Ok(())
}

/// Why this process's build may not run, where it runs for a client of this machine's
/// (`client_grants::CLIENT_VAR`, remote-09): `words`, the build's command, naming a path
/// outside the client's copies here ([`held_within`]). None for this machine's own user's.
pub(crate) fn refused_for_client(words: &[String]) -> Option<String> {
    let client = crate::client_grants::client_here();
    if client.is_empty() {
        return None;
    }
    if !shards_remote::invite::valid_name(&client) {
        return Some(format!("{client:?}: no client's name"));
    }
    let home = match shards_ipc::home() {
        Ok(home) => home,
        Err(e) => return Some(e),
    };
    held_within(words, &home.join("synced").join(&client))
        .err()
        .map(|why| why.to_string())
}

/// Reads what [`send`] wrote, on the machine: the build file's directory written whole into
/// A build-file timestamp stays typed until the command renders its refusal on stderr.
#[derive(Debug)]
pub(crate) enum ReceiveError {
    Timestamp { name: String, cause: FileTimeError },
    Context(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileTimeError {
    Nanoseconds(u32),
    Unrepresentable { seconds: i64, nanoseconds: u32 },
}

impl std::fmt::Display for ReceiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Context(why) => f.write_str(why),
            Self::Timestamp { name, cause } => write!(f, "{name:?}: invalid build-file time: {cause}"),
        }
    }
}

impl std::fmt::Display for FileTimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nanoseconds(n) => write!(f, "{n} nanoseconds is not a fraction of a second"),
            Self::Unrepresentable { seconds, nanoseconds } => {
                write!(
                    f,
                    "{seconds} seconds and {nanoseconds} nanoseconds is outside this host's time range"
                )
            }
        }
    }
}

impl From<String> for ReceiveError {
    fn from(why: String) -> Self {
        Self::Context(why)
    }
}

/// The wire's fractional nanoseconds contain no whole second (utimensat(2), tv_nsec).
/// SystemTime's checked operation supplies the receiving host's range, including Windows'.
fn file_time((seconds, nanoseconds): (i64, u32)) -> Result<std::time::SystemTime, FileTimeError> {
    if std::time::Duration::from_nanos(u64::from(nanoseconds)).as_secs() != 0 {
        return Err(FileTimeError::Nanoseconds(nanoseconds));
    }
    // Preserve the receiver's existing normalization of pre-epoch times.
    let secs = u64::try_from(seconds).unwrap_or(0);
    std::time::UNIX_EPOCH
        .checked_add(std::time::Duration::new(secs, nanoseconds))
        .ok_or(FileTimeError::Unrepresentable { seconds, nanoseconds })
}

/// `dockerfile`, made afresh, then the layer applied over `context`. Names are a file's
/// alone, no path. Returns the bytes the layer's files held.
pub(crate) fn receive(
    input: &mut dyn Read,
    dockerfile: &Path,
    context: &Path,
    named: usize,
) -> Result<(i64, Vec<(String, shards_cmdline::buildflags::SecretBytes)>), ReceiveError> {
    let mut magic = vec![0u8; MAGIC.len()];
    input
        .read_exact(&mut magic)
        .map_err(|e| format!("reading the build file: {e}"))?;
    if magic != MAGIC {
        return Err("not a context of a shards client's".to_string().into());
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
        name_alone(&name, cfg!(windows)).map_err(|why| format!("{name:?}: {why}"))?;
        let mode = u32::from_be_bytes(read_array(input)?);
        let mtime = (
            i64::from_be_bytes(read_array(input)?),
            u32::from_be_bytes(read_array(input)?),
        );
        let mtime = file_time(mtime).map_err(|cause| ReceiveError::Timestamp {
            name: name.clone(),
            cause,
        })?;
        let size = u64::from_be_bytes(read_array(input)?);
        total = total.saturating_add(size);
        if total > DOCKERFILE_MAX {
            return Err(format!("a build file's directory past {DOCKERFILE_MAX} bytes").into());
        }
        let bytes = read_vec(input, size)?;
        write_file(&dockerfile.join(&name), mode, mtime, &bytes)?;
    }
    // The secrets, in memory alone: no file of the machine's holds one.
    let count = u32::from_be_bytes(read_array(input)?);
    if usize::try_from(count).map_or(true, |count| count > named) {
        return Err(format!("{count} secrets, where the build names {named}").into());
    }
    let mut secrets = Vec::new();
    for _ in 0..count {
        let len = u32::from_be_bytes(read_array(input)?);
        let id = String::from_utf8(read_vec(input, u64::from(len))?)
            .map_err(|_| "a secret's id that is not UTF-8".to_string())?;
        let size = u64::from_be_bytes(read_array(input)?);
        total = total.saturating_add(size);
        if size > SECRET_MAX || total > DOCKERFILE_MAX {
            return Err(
                format!("{id}: a secret past {SECRET_MAX} bytes, or secrets past {DOCKERFILE_MAX}").into(),
            );
        }
        secrets.push((id, read_secret(input, size)?));
    }
    std::fs::create_dir_all(context).map_err(|e| format!("{}: {e}", context.display()))?;
    let sent = apply(input, context).map_err(|e| format!("applying the context: {e}"))?;
    Ok((sent, secrets))
}

/// The layer `input` applied over the copy `context`, everything owned by the machine's
/// user, each extended attribute kept as it can be. On Windows the copy is a Linux tree
/// (shards_archive's `apply_layer_lx`): a client's modes, owners, links, devices and names
/// that NTFS's own files keep none of are kept as WSL keeps them, and the build's walk of the
/// copy reads them back as the client's; where NTFS kept them as its own, a Linux or macOS
/// client's context went to its build with every file 0755, its symlinks made only where
/// the machine's user may make one, and two names differing in case alone one file.
fn apply(input: &mut dyn Read, context: &Path) -> Result<i64, shards_archive::Error> {
    #[cfg(windows)]
    {
        shards_archive::apply_layer_lx(input, context)
    }
    #[cfg(not(windows))]
    {
        let opts = shards_archive::UnpackOptions {
            no_lchown: true,
            best_effort_xattrs: true,
            ..shards_archive::UnpackOptions::default()
        };
        shards_archive::apply_layer(input, context, &opts)
    }
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
fn write_file(path: &Path, mode: u32, mtime: std::time::SystemTime, bytes: &[u8]) -> Result<(), String> {
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
    file.set_modified(mtime).map_err(at)
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
    apply(input, context).map_err(|e| format!("applying a tree: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_build_file_time_is_refused_before_the_file_is_created() {
        let dir = shards_testdir::TempDir::new("remote-time").unwrap();
        let (dockerfile, context) = (dir.join("dockerfile"), dir.join("context"));
        let name = b"Dockerfile";
        let mut input = MAGIC.to_vec();
        input.extend_from_slice(&1u32.to_be_bytes());
        input.extend_from_slice(&(name.len() as u32).to_be_bytes());
        input.extend_from_slice(name);
        input.extend_from_slice(&0o644u32.to_be_bytes());
        input.extend_from_slice(&i64::MAX.to_be_bytes());
        input.extend_from_slice(&u32::MAX.to_be_bytes());
        input.extend_from_slice(&0u64.to_be_bytes());
        assert!(matches!(
            receive(&mut input.as_slice(), &dockerfile, &context, 0),
            Err(ReceiveError::Timestamp {
                cause: FileTimeError::Nanoseconds(u32::MAX),
                ..
            })
        ));
        assert!(!dockerfile.join("Dockerfile").exists());
    }

    #[test]
    fn build_file_time_checks_the_fraction_and_the_receiving_hosts_range() {
        assert_eq!(file_time((0, 0)), Ok(std::time::UNIX_EPOCH));
        assert_eq!(
            file_time((0, 999_999_999)),
            Ok(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(999_999_999))
        );
        assert_eq!(
            file_time((0, 1_000_000_000)),
            Err(FileTimeError::Nanoseconds(1_000_000_000))
        );
        // Windows' FILETIME range is smaller than POSIX's signed seconds range.
        #[cfg(windows)]
        assert_eq!(
            file_time((i64::MAX, 0)),
            Err(FileTimeError::Unrepresentable {
                seconds: i64::MAX,
                nanoseconds: 0
            })
        );
    }

    /// remote-09: a client's build step names no path of this machine's but its copies', as
    /// its shards sends them, every path-typed value of buildx's build judged; and what reads
    /// the machine's own secrets or agent is refused. What its shards sends passes
    /// (context.rs, `what_a_client_sends_of_a_build_is_held_to_its_copies`).
    #[test]
    fn a_clients_build_names_no_path_but_its_copies() {
        let key = "ab".repeat(32);
        let sent = |s: &str| {
            s.replace("KEY", &key)
                .split(' ')
                .map(|w| w.replace("NUL", "\0"))
                .collect::<Vec<String>>()
        };
        let held = |s: &str, secrets: usize| held_to_copies(&sent(s), secrets);
        for ok in [
            "build --file=NULdockerfile/Dockerfile NULcontext",
            "buildx build --output=type=tar,dest=NULout/0 --iidfile=NULout/iidfile --metadata-file=NULout/metadata NULcontext",
            "build --cache-to=type=local,dest=NULout/cache-0 --cache-from=type=local,src=NULtree/KEY NULcontext",
            "build --cache-from=user/app:cache --cache-to=type=inline -o type=registry,name=r/i NULcontext",
            "build --build-context=a=NULtree/KEY --build-context=b=oci-layout://NULtree/KEY:v1 --build-context=c=docker-image://alpine NULcontext",
            "build --secret=id=tok,src=NULsecret/0 --ssh=default=NULagent/default NULcontext",
            "build --policy=filename=policy/p.rego --output=- NULcontext",
            "agent build NULcontext -t a",
        ] {
            assert_eq!(held(ok, 1), Ok(()), "{ok}");
        }
        for (refused, why) in [
            (
                "build --output=type=local,dest=/outside NULcontext",
                NotHeld::Outside("output"),
            ),
            ("build -o /outside NULcontext", NotHeld::Outside("output")),
            (
                "build --output=type=tar,dest=NULout/../x NULcontext",
                NotHeld::Outside("output"),
            ),
            (
                "build --output=type=oci,dest=NULout/0/../../x NULcontext",
                NotHeld::Outside("output"),
            ),
            ("build --iidfile=/tmp/id NULcontext", NotHeld::Outside("iidfile")),
            (
                "build --metadata-file=NULout/a/b NULcontext",
                NotHeld::Outside("metadata-file"),
            ),
            ("build --file=/etc/passwd NULcontext", NotHeld::Outside("file")),
            (
                "build --file=NULdockerfile/../../identity NULcontext",
                NotHeld::Outside("file"),
            ),
            (
                "build --cache-to=type=local,dest=/var/cache NULcontext",
                NotHeld::Outside("cache-to"),
            ),
            (
                "build --cache-to=type=local,src=NULtree/KEY NULcontext",
                NotHeld::Outside("cache-to"),
            ),
            (
                "build --cache-from=type=local,src=/home NULcontext",
                NotHeld::Outside("cache-from"),
            ),
            (
                "build --cache-from=type=local,src=NULtree/short NULcontext",
                NotHeld::Outside("cache-from"),
            ),
            (
                "build --build-context=a=/etc NULcontext",
                NotHeld::Outside("build-context"),
            ),
            (
                "build --build-context=a=oci-layout:///srv/layout NULcontext",
                NotHeld::Outside("build-context"),
            ),
            (
                "build --secret=id=a,src=/etc/shadow NULcontext",
                NotHeld::Outside("secret"),
            ),
            (
                "build --secret=id=a,src=NULsecret/1 NULcontext",
                NotHeld::Outside("secret"),
            ),
            (
                "build --secret=id=a,src=NULsecret/+0 NULcontext",
                NotHeld::Outside("secret"),
            ),
            (
                "build --secret=id=a,env=HOME NULcontext",
                NotHeld::Machines("secret"),
            ),
            ("build --secret=id=HOME NULcontext", NotHeld::Machines("secret")),
            ("build --ssh=default NULcontext", NotHeld::Machines("ssh")),
            (
                "build --ssh=default=/run/agent.sock NULcontext",
                NotHeld::Outside("ssh"),
            ),
            (
                "build --policy=filename=/etc/p.rego NULcontext",
                NotHeld::Outside("policy"),
            ),
            (
                "build --policy=filename=../p.rego NULcontext",
                NotHeld::Outside("policy"),
            ),
            ("build /srv/context", NotHeld::Outside("PATH")),
            ("build NULtree/KEY", NotHeld::Outside("PATH")),
            ("agent build /srv/agent -t a", NotHeld::Outside("PATH")),
            ("run alpine", NotHeld::Unread),
        ] {
            assert_eq!(held(refused, 1), Err(why), "{refused}");
        }
        // As Windows reads a name: a drive's prefix or a stream names another file there.
        let windows = [
            "build --iidfile=NULout/C:identity NULcontext",
            "build --file=NULdockerfile/Dockerfile:stream NULcontext",
            "build --policy=filename=C:p.rego NULcontext",
        ];
        for words in windows {
            assert_eq!(held(words, 1).is_err(), cfg!(windows), "{words}");
        }
    }

    /// remote-09: a build, an image's or an artifact's, that runs for a client of this
    /// machine's (`SHARDS_CLIENT`) refuses a path outside the client's copies before it reads
    /// or writes anything; run in a process of its own, whose environment names the client.
    #[test]
    fn a_build_run_for_a_client_refuses_this_machines_paths() {
        const CHILD: &str = "SHARDS_TEST_HELD_FOR_CLIENT";
        if std::env::var_os(CHILD).is_some() {
            // Its context its copy's, here none: only its output is outside.
            let copy = shards_ipc::home()
                .unwrap()
                .join("synced")
                .join("alice")
                .join("ab".repeat(32));
            let _ = crate::build::build(
                [
                    std::ffi::OsString::from("--output=type=local,dest=/outside"),
                    copy.join("context").into_os_string(),
                ]
                .into_iter(),
            );
            let _ = crate::agent::command(
                "agent",
                ["build", "/srv/agent", "-t", "a"]
                    .into_iter()
                    .map(std::ffi::OsString::from),
            );
            return;
        }
        let home = shards_testdir::TempDir::new("held-child").unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "synced::tests::a_build_run_for_a_client_refuses_this_machines_paths",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env(crate::client_grants::CLIENT_VAR, "alice")
            .env("SHARDS_HOME", home.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "child failed: {:?}", output.status);
        let said = String::from_utf8_lossy(&output.stderr);
        assert!(said.contains(&NotHeld::Outside("output").to_string()), "{said}");
        assert!(said.contains(&NotHeld::Outside("PATH").to_string()), "{said}");
    }

    /// remote-09, where the build runs for a client: each path its words name, once placed,
    /// is beneath the client's copies, none of its parts `..`; a secret's or an agent's a
    /// descriptor it was given.
    #[test]
    fn a_placed_build_names_nothing_outside_its_copies() {
        let dir = shards_testdir::TempDir::new("held").unwrap();
        let copies = dir.join("synced").join("alice");
        let copy = copies.join("ab".repeat(32));
        let agents = BTreeMap::from([("default".to_string(), PathBuf::from("/dev/fd/5"))]);
        let places = Places {
            context: &copy.join("context"),
            dockerfile: &copy.join("dockerfile"),
            out: &copy.join("out"),
            copies: &copies,
            agents: &agents,
        };
        let placed = |s: &str| {
            s.split(' ')
                .map(|w| placed(&w.replace("NUL", "\0"), &places).into_string().unwrap())
                .collect::<Vec<String>>()
        };
        let sent = "build --file=NULdockerfile/Dockerfile --output=type=tar,dest=NULout/0 --secret=id=a,src=NULsecret/0 --ssh=default=NULagent/default --policy=filename=p.rego NULcontext";
        assert_eq!(held_within(&placed(sent), &copies), Ok(()));
        let artifact = "agent build NULcontext -t a";
        assert_eq!(held_within(&placed(artifact), &copies), Ok(()));
        let outside = dir.join("identity");
        let outside = outside.to_str().unwrap();
        for (words, flag) in [
            (
                format!("build --output=type=local,dest={outside} NULcontext"),
                "output",
            ),
            (
                "build --iidfile=NULout/../../../identity NULcontext".to_string(),
                "iidfile",
            ),
            (format!("build --file={outside} NULcontext"), "file"),
            (format!("build --secret=id=a,src={outside} NULcontext"), "secret"),
            (format!("build {outside}"), "PATH"),
            (format!("agent build {outside} -t a"), "PATH"),
        ] {
            assert_eq!(
                held_within(&placed(&words), &copies),
                Err(NotHeld::Outside(flag)),
                "{words}"
            );
        }
    }

    /// remote-02: a build file's name a client sends is one file's of the copy as this
    /// machine's paths read it. On Windows a drive's prefix, a stream, a device's name, a
    /// reserved character or a trailing dot or space is none, and a Linux or macOS machine
    /// takes each as the name it is there.
    #[test]
    fn a_build_files_name_is_one_files_as_the_machine_reads_it() {
        for (name, why) in [
            ("", NotAName::Dots),
            (".", NotAName::Dots),
            ("..", NotAName::Dots),
            ("a/b", NotAName::Path),
            ("a\\b", NotAName::Path),
            ("\\\\?\\C:\\x", NotAName::Path),
            ("\\\\host\\share", NotAName::Path),
            ("a\0b", NotAName::Path),
        ] {
            assert_eq!(name_alone(name, false), Err(why), "{name:?}");
            assert_eq!(name_alone(name, true), Err(why), "{name:?}");
        }
        for (name, why) in [
            ("C:context", NotAName::Colon),
            ("c:identity", NotAName::Colon),
            ("Dockerfile:stream", NotAName::Colon),
            ("Dockerfile::$DATA", NotAName::Colon),
            ("a?b", NotAName::Reserved),
            ("a*", NotAName::Reserved),
            ("a\u{1f}b", NotAName::Reserved),
            ("Dockerfile.", NotAName::Trailing),
            ("Dockerfile ", NotAName::Trailing),
            ("CON", NotAName::Device),
            ("nul.txt", NotAName::Device),
            ("NUL .txt", NotAName::Device),
            ("Com1", NotAName::Device),
            ("LPT9.Dockerfile", NotAName::Device),
            ("COM\u{b9}", NotAName::Device),
            ("CONOUT$", NotAName::Device),
        ] {
            assert_eq!(name_alone(name, true), Err(why), "{name:?}");
            assert_eq!(name_alone(name, false), Ok(()), "{name:?}");
        }
        for name in [
            "Dockerfile",
            "dockerfile",
            "Dockerfile.dockerignore",
            ".dockerignore",
            "CONSOLE",
            "COM10",
            "LPT",
            "nul2.txt",
        ] {
            assert_eq!(name_alone(name, true), Ok(()), "{name:?}");
        }
    }

    /// remote-02 as a client's build sends it to a Windows machine: a name of a drive's
    /// current directory (`C:NAME`) is refused, and nothing is written there.
    #[cfg(windows)]
    #[test]
    fn a_drive_relative_build_file_is_refused_and_written_nowhere() {
        let cwd = std::env::current_dir().unwrap();
        let drive: String = cwd.to_str().unwrap().chars().take(2).collect();
        assert!(drive.ends_with(':'), "{cwd:?}");
        let leaf = format!("r02-escape-{}", std::process::id());
        let name = format!("{drive}{leaf}");
        let mut sent = Vec::new();
        let carried = [Carried {
            name: name.clone(),
            mode: 0o644,
            mtime: (0, 0),
            bytes: b"x".to_vec(),
        }];
        send(&mut sent, &carried, &[], |_| Ok(())).unwrap();
        let dir = shards_testdir::TempDir::new("r02").unwrap();
        let (dockerfile, context) = (dir.join("dockerfile"), dir.join("context"));
        let refused = receive(&mut &sent[..], &dockerfile, &context, 0).unwrap_err();
        let escaped = cwd.join(&leaf);
        let was = escaped.exists();
        let _ = std::fs::remove_file(&escaped);
        assert!(!was, "{name}: written at {escaped:?}, outside {dockerfile:?}");
        assert!(
            refused.to_string().contains(&NotAName::Colon.to_string()),
            "{refused}"
        );
    }

    /// A time a Windows machine's NTFS kept, to 100 ns, is the client's time it was cut from,
    /// and no other; a copy's time to the nanosecond is its own alone.
    #[test]
    fn a_time_ntfs_kept_is_the_clients_it_was_cut_from() {
        let at = (1_700_000_000, 123_456_789);
        assert!(same_time(at, at));
        assert!(same_time(at, (1_700_000_000, 123_456_700)));
        assert!(!same_time(at, (1_700_000_000, 123_456_800)));
        assert!(!same_time(at, (1_700_000_001, 123_456_700)));
        assert!(!same_time(at, (1_700_000_000, 123_456_780)));
    }

    /// Do: a Linux client's tree, as a layer of its own modes (0600, 0750, setuid), a symlink,
    /// a hard link, two names differing in case alone and one NTFS holds no name with,
    /// applied over a Windows machine's copy. Expect the copy's manifest, its walk as a
    /// build walks a context, the client's: each mode, the link's target, both names and
    /// `a:b`, the hard link one inode of two links; and a build's load of the copy each mode
    /// the client's, where NTFS's own files would have gone as 0755.
    #[cfg(windows)]
    #[test]
    fn a_linux_clients_tree_is_its_own_in_a_windows_copy() {
        use shards_archive::tar::{
            Format, Header, TYPE_DIR, TYPE_LINK, TYPE_REG, TYPE_SYMLINK, Time, Writer,
        };
        let entry = |typeflag: u8, name: &str, mode: i64, data: &'static [u8], link: &str| {
            let h = Header {
                typeflag,
                name: name.as_bytes().to_vec(),
                linkname: link.as_bytes().to_vec(),
                mode,
                size: i64::try_from(data.len()).unwrap(),
                mtime: Time {
                    sec: 1_700_000_000,
                    nsec: 123_456_789,
                },
                format: Format::PAX,
                ..Header::default()
            };
            (h, data)
        };
        let mut w = Writer::new(Vec::new());
        for (h, data) in [
            entry(TYPE_DIR, "bin", 0o750, b"", ""),
            entry(TYPE_REG, "bin/run.sh", 0o755, b"#!/bin/sh\n", ""),
            entry(TYPE_REG, "secret", 0o600, b"s", ""),
            entry(TYPE_REG, "set", 0o4755, b"x", ""),
            entry(TYPE_REG, "Make", 0o644, b"upper", ""),
            entry(TYPE_REG, "make", 0o644, b"lower", ""),
            entry(TYPE_REG, "a:b", 0o644, b"colon", ""),
            entry(TYPE_SYMLINK, "link", 0o777, b"", "bin/run.sh"),
            entry(TYPE_LINK, "again", 0o600, b"", "secret"),
        ] {
            w.write_header(&h).unwrap();
            std::io::Write::write_all(&mut w, data).unwrap();
        }
        let layer = w.finish().unwrap();
        let top = shards_testdir::TempDir::new("lx-copy").unwrap();
        let top: &Path = top.as_ref();
        let copy = top.join("context");
        std::fs::create_dir(&copy).unwrap();
        apply(&mut &layer[..], &copy).unwrap();

        let manifest = manifest_of(&copy).unwrap();
        let mode = |path: &str| {
            manifest
                .get(path.as_bytes())
                .unwrap_or_else(|| panic!("no {path}: {manifest:?}"))
                .mode
        };
        assert_eq!(mode("bin"), MODE_DIR | 0o750);
        assert_eq!(mode("bin/run.sh"), 0o755);
        assert_eq!(mode("secret"), 0o600);
        assert_eq!(mode("again"), 0o600);
        assert_eq!(mode("Make"), 0o644);
        assert_eq!(mode("make"), 0o644);
        assert_eq!(mode("a:b"), 0o644);
        assert_eq!(mode("link") & MODE_SYMLINK, MODE_SYMLINK);
        assert_eq!(manifest[b"link".as_slice()].link, b"bin/run.sh");
        assert_eq!(manifest[b"secret".as_slice()].mtime, (1_700_000_000, 123_456_700));
        // The second name of one file in the walk's order is a link to the first.
        let walked = shards_build::context::walk(&copy, &shards_build::context::Filters::default()).unwrap();
        let link_of = |name: &[u8]| walked.iter().find(|(p, _)| p == name).unwrap().1.link.clone();
        assert_eq!(
            (link_of(b"again"), link_of(b"secret")),
            (Vec::new(), b"again".to_vec())
        );

        let stage = top.join("stage");
        std::fs::create_dir(&stage).unwrap();
        let mut sources = shards_build::data::Sources::default();
        let fs = shards_build::context::load(
            &copy,
            &shards_build::context::Filters::default(),
            &mut sources,
            (0, 0),
            &stage,
        )
        .unwrap();
        for (path, perm) in [
            ("/secret", 0o600),
            ("/set", 0o4755),
            ("/bin/run.sh", 0o755),
            ("/bin", 0o750),
            ("/make", 0o644),
        ] {
            let node = fs.node(fs.lstat(path.as_bytes()).unwrap()).unwrap();
            assert_eq!(node.meta.mode, perm, "{path}");
        }
        assert_eq!(fs.readlink(b"/link").unwrap(), b"bin/run.sh");
    }

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
            refused.to_string().contains("2 secrets, where the build names 1"),
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
        let linked = |ino: u128, links: u64, mtime: i64| Stat {
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
