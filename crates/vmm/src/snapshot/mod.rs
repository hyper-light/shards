//! VM snapshots on disk. A snapshot directory holds generations, each a complete snapshot
//! in a directory of its own, and one pointer naming the generation in use:
//!
//! - `current`: the name of the generation in use
//! - `g-<id>/state`: a versioned header, the generation's name, the machine configuration
//!   restore rebuilds and the identity of each file backing it, the architecture's CPU and
//!   interrupt-controller state, and the devices' state
//! - `g-<id>/memory`: guest RAM, one region after another, with all-zero pages as holes
//!
//! and, once a VM resumed from the generation has recorded one, a third:
//!
//! - `g-<id>/working-set`: the guest pages that VM touched after the snapshot, which
//!   restores ahead of their request prefetch (PM M30)
//!
//! A write builds a generation in a staging directory of its own, syncs its files and the
//! directory, renames it into place, then points `current` at it, holding the snapshot
//! directory's lock throughout so that writers take turns. A reader therefore finds the
//! generation before a write or the one after it, whole, never a mix of the two (audit
//! A03). It opens both files through the generation's directory and holds them, so a
//! later write cannot swap one out from under it.

pub mod codec;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use codec::{DecodeError, Reader, Writer};

use crate::hv::Touch;
use crate::memory::GuestMemory;
use crate::platform;

/// The pointer to the generation in use.
pub const CURRENT: &str = "current";
const LOCK: &str = ".lock";
const STATE: &str = "state";
const MEMORY: &str = "memory";
const WORKING_SET: &str = "working-set";
const WORKING_SET_MAGIC: [u8; 8] = *b"SHRDWSET";
const WORKING_SET_VERSION: u32 = 1;
/// A working set's bytes before its entries: magic, version, architecture, page size
/// and count, with room to spare.
pub const WORKING_SET_HEADER: u64 = 64;
const MAGIC: [u8; 8] = *b"SHRDSNAP";
/// 2: MachineConfig records whether the machine has a vsock device.
/// 3: and its virtio-pmem files.
/// 4: virtio devices' own state follows their queues' (vsock: the streams a restore resets).
/// 5: the control page's state records the identity the guest's init announced.
/// 6: generations, each named in its state; backing files by absolute path in the OS's
///    own bytes, with their identities.
/// 7: arm64's GIC as the backend's own serialization of the device, not its registers.
/// 8: x86's TSC offsets, so every vCPU's TSC comes back in step with the others'.
/// 9: MachineConfig records the machine's network device, by its MAC, if it has one.
const VERSION: u32 = 9;
/// The snapshot format this build writes and reads: what a snapshot kept for reuse is
/// keyed by.
pub const FORMAT: u32 = VERSION;
/// A state file is kilobytes; anything past this is not one of ours.
const MAX_STATE: u64 = 64 << 20;
const MAX_DISKS: usize = 64;
const MAX_PATH: usize = 4096;
const MAX_NAME: usize = 64;
/// How often a reader looks again at `current` for a generation a newer write removed
/// between its look and its open.
const READ_ATTEMPTS: usize = 8;

/// What restore needs to rebuild the machine the snapshot came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineConfig {
    pub vcpus: u32,
    pub memory_mib: u64,
    /// virtio-blk disks by absolute path, in guest order. A restore reopens the same files,
    /// which must hold what the guest's page cache expects (as with Firecracker).
    pub disks: Vec<(PathBuf, bool)>,
    /// Read-only virtio-pmem files by absolute path, in guest order, after the disks. A
    /// restore maps the same files, which must be unchanged.
    pub pmem: Vec<PathBuf>,
    /// Whether a vsock device follows the pmem devices. Its host socket path is not
    /// recorded: a restored VM needs a path of its own.
    pub vsock: bool,
    /// The network device's MAC, if a network device follows the vsock device: its
    /// network process is the restore's own.
    pub net: Option<[u8; 6]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub config: MachineConfig,
    /// The architecture's machine state, as its machine module encodes it.
    pub arch: Vec<u8>,
    /// The device bus's state.
    pub devices: Vec<u8>,
}

/// A generation, pinned: its state decoded and its memory file held open. Its files are
/// a VM's inputs (`platform::open_input`): a VM in App Sandbox is handed each open, and
/// never its directory (PM M70).
#[derive(Debug)]
pub struct Pinned {
    pub snapshot: Snapshot,
    pub memory: File,
    /// The generation's directory.
    pub path: PathBuf,
    /// The generation's name, which a working set recorded from it names (runtime.rs,
    /// `accept_working_set`).
    pub name: String,
}

/// Enough of a backing file's identity to refuse a restore against another file without
/// reading it: which file it is and, for one the guest only reads, that it is unchanged
/// (audit A18).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Identity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_s: i64,
    mtime_ns: u32,
}

impl Identity {
    fn of(path: &Path) -> io::Result<Identity> {
        let m = platform::input_metadata(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            Ok(Identity {
                dev: m.dev(),
                ino: m.ino(),
                size: m.size(),
                mtime_s: m.mtime(),
                mtime_ns: u32::try_from(m.mtime_nsec()).unwrap_or(0),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt as _;
            // 100 ns intervals since 1601.
            let written = m.last_write_time();
            Ok(Identity {
                size: m.len(),
                mtime_s: i64::try_from(written / 10_000_000).unwrap_or(i64::MAX),
                mtime_ns: u32::try_from(written % 10_000_000 * 100).unwrap_or(0),
                ..Identity::default()
            })
        }
    }

    fn encode(&self, w: &mut Writer) {
        w.u64(self.dev);
        w.u64(self.ino);
        w.u64(self.size);
        w.u64(self.mtime_s as u64);
        w.u32(self.mtime_ns);
    }

    fn decode(r: &mut Reader<'_>) -> codec::Result<Identity> {
        Ok(Identity {
            dev: r.u64()?,
            ino: r.u64()?,
            size: r.u64()?,
            mtime_s: r.u64()? as i64,
            mtime_ns: r.u32()?,
        })
    }
}

/// The files `pinned` restores against, disks then pmem, each with whether the guest only
/// reads it: what a restore will open, for a sandbox to allow before it does (shards
/// confine.rs). Of a snapshot already read, which the restore goes on to resume, so
/// that it reads and decodes its state once (review 1.12).
pub fn backing_of(pinned: &Pinned) -> Vec<(PathBuf, bool)> {
    backing(&pinned.snapshot.config)
        .map(|(path, read_only)| (path.to_path_buf(), read_only))
        .collect()
}

/// The MAC of the snapshot in `dir`'s network device, if it has one: a VM restored from
/// it needs a network process of its own.
pub fn net(dir: &Path) -> Result<Option<[u8; 6]>, String> {
    Ok(read(dir)?.snapshot.config.net)
}

/// The files of the generation the snapshot in `dir` is at, for a VM in App Sandbox to be
/// granted before it reads the snapshot (shards `grant`; PM M70): its state and memory,
/// and its working set, which it may not have. Its pointer is read as the VM's input.
pub fn generation_files(dir: &Path) -> Result<([PathBuf; 2], PathBuf), String> {
    let name = current(dir).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => format!("{}: no snapshot here", dir.display()),
        _ => format!("{}: {CURRENT}: {e}", dir.display()),
    })?;
    let generation = dir.join(name);
    Ok((
        [generation.join(STATE), generation.join(MEMORY)],
        generation.join(WORKING_SET),
    ))
}

/// A snapshot's pointer, for [`generation_files`].
pub fn pointer(dir: &Path) -> PathBuf {
    dir.join(CURRENT)
}

/// The files backing `config`, disks then pmem, each with whether the guest only reads it.
fn backing(config: &MachineConfig) -> impl Iterator<Item = (&Path, bool)> {
    config
        .disks
        .iter()
        .map(|(path, read_only)| (path.as_path(), *read_only))
        .chain(config.pmem.iter().map(|path| (path.as_path(), true)))
}

/// `path` in the OS's own bytes. It must be absolute: a restore in another directory
/// must open the same file (audit A18).
fn path_bytes(path: &Path) -> Result<&[u8], String> {
    if !path.is_absolute() {
        return Err(format!(
            "{}: a snapshot names its files by absolute path",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Ok(path.as_os_str().as_bytes())
    }
    #[cfg(not(unix))]
    path.to_str()
        .map(str::as_bytes)
        .ok_or_else(|| format!("{}: a path that is not UTF-8", path.display()))
}

fn path_from(bytes: &[u8]) -> codec::Result<PathBuf> {
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStrExt as _;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    };
    #[cfg(not(unix))]
    let path = PathBuf::from(
        std::str::from_utf8(bytes).map_err(|_| DecodeError("a path that is not UTF-8".into()))?,
    );
    if !path.is_absolute() {
        return Err(DecodeError(format!("{}: not an absolute path", path.display())));
    }
    Ok(path)
}

fn encode(s: &Snapshot, generation: &str, identities: &[Identity]) -> Result<Vec<u8>, String> {
    let disks = s
        .config
        .disks
        .iter()
        .map(|(path, read_only)| Ok((path_bytes(path)?, *read_only)))
        .collect::<Result<Vec<_>, String>>()?;
    let pmem = s
        .config
        .pmem
        .iter()
        .map(|path| path_bytes(path))
        .collect::<Result<Vec<_>, String>>()?;
    let mut w = Writer::default();
    MAGIC.iter().for_each(|&b| w.u8(b));
    w.u32(VERSION);
    w.bytes(std::env::consts::ARCH.as_bytes());
    w.bytes(generation.as_bytes());
    w.u32(s.config.vcpus);
    w.u64(s.config.memory_mib);
    w.seq(&disks, |w, (path, read_only)| {
        w.bytes(path);
        w.bool(*read_only);
    });
    w.seq(&pmem, |w, path| w.bytes(path));
    w.seq(identities, |w, identity| identity.encode(w));
    w.bool(s.config.vsock);
    w.bool(s.config.net.is_some());
    w.bytes(&s.config.net.unwrap_or_default());
    w.bytes(&s.arch);
    w.bytes(&s.devices);
    Ok(w.into_bytes())
}

/// A state file, decoded: the snapshot, the generation it belongs to, and its backing
/// files' identities.
#[derive(Debug)]
struct Decoded {
    snapshot: Snapshot,
    generation: String,
    identities: Vec<Identity>,
}

/// Decodes a state file's bytes, as a restore does, and says only whether they decode:
/// for fuzzing (fuzz/fuzz_targets/snapshot-state.rs).
#[doc(hidden)]
pub fn decodes(bytes: &[u8]) -> bool {
    decode(bytes).is_ok()
}

fn decode(bytes: &[u8]) -> codec::Result<Decoded> {
    let mut r = Reader::new(bytes);
    let mut magic = [0u8; 8];
    for b in &mut magic {
        *b = r.u8()?;
    }
    if magic != MAGIC {
        return Err(DecodeError("not a shards snapshot".into()));
    }
    let version = r.u32()?;
    if version != VERSION {
        return Err(DecodeError(format!(
            "snapshot format {version}; this shards reads {VERSION}"
        )));
    }
    let arch = r.bytes(32)?;
    if arch != std::env::consts::ARCH.as_bytes() {
        return Err(DecodeError(format!(
            "snapshot of a {} guest; this host runs {} guests",
            String::from_utf8_lossy(arch),
            std::env::consts::ARCH
        )));
    }
    let generation = std::str::from_utf8(r.bytes(MAX_NAME)?)
        .map_err(|_| DecodeError("a generation name that is not UTF-8".into()))?
        .to_string();
    let vcpus = r.u32()?;
    let memory_mib = r.u64()?;
    let disks = r.seq(MAX_DISKS, 5, |r| Ok((path_from(r.bytes(MAX_PATH)?)?, r.bool()?)))?;
    let pmem = r.seq(MAX_DISKS, 4, |r| path_from(r.bytes(MAX_PATH)?))?;
    let identities = r.seq(2 * MAX_DISKS, 36, Identity::decode)?;
    if identities.len() != disks.len() + pmem.len() {
        return Err(DecodeError(format!(
            "{} identities for {} backing files",
            identities.len(),
            disks.len() + pmem.len()
        )));
    }
    let vsock = r.bool()?;
    let has_net = r.bool()?;
    let mac = r.bytes(6)?;
    let net = match <[u8; 6]>::try_from(mac) {
        Ok(m) if has_net => Some(m),
        Ok(_) => None,
        Err(_) => return Err(DecodeError("a MAC that is not six bytes".into())),
    };
    let arch_state = r.bytes(usize::MAX)?.to_vec();
    let devices = r.bytes(usize::MAX)?.to_vec();
    r.finish()?;
    Ok(Decoded {
        snapshot: Snapshot {
            config: MachineConfig {
                vcpus,
                memory_mib,
                disks,
                pmem,
                vsock,
                net,
            },
            arch: arch_state,
            devices,
        },
        generation,
        identities,
    })
}

/// A name no other generation on this host has had: the time, this process, and a count
/// within it.
fn fresh_name() -> String {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!(
        "g-{nanos:024x}-{:x}-{:x}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::Relaxed)
    )
}

fn is_generation(name: &str) -> bool {
    name.len() <= MAX_NAME
        && name
            .strip_prefix("g-")
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
}

/// The generation `dir`'s pointer names. A pointer is a name and a newline; no more of
/// one is read.
fn current(dir: &Path) -> io::Result<String> {
    let mut bytes = Vec::new();
    platform::open_input(&dir.join(CURRENT), false)?
        .take(MAX_NAME as u64 + 2)
        .read_to_end(&mut bytes)?;
    let name = std::str::from_utf8(&bytes)
        .ok()
        .map(|text| text.trim_end_matches('\n'))
        .filter(|name| is_generation(name))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "a pointer to no generation"))?;
    Ok(name.to_string())
}

/// Creates `path`, fills it, and syncs it: a name no other file had.
fn create(path: &Path, fill: impl FnOnce(&File) -> io::Result<()>) -> io::Result<()> {
    platform::sync_durable(&create_unsynced(path, fill)?)
}

/// [`create`], but for its durable flush: its caller makes it durable later.
fn create_unsynced(path: &Path, fill: impl FnOnce(&File) -> io::Result<()>) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(path)?;
    fill(&file)?;
    Ok(file)
}

/// Points `dir` at the generation `name`: a new pointer, synced, renamed over the old one,
/// and the directory synced.
fn point(dir: &Path, name: &str) -> io::Result<()> {
    let temp = dir.join(format!(".{CURRENT}.{name}.tmp"));
    let pointed = create(&temp, |mut f| f.write_all(format!("{name}\n").as_bytes()))
        .and_then(|()| fs::rename(&temp, dir.join(CURRENT)))
        .and_then(|()| platform::sync_dir(dir));
    if pointed.is_err() {
        let _ = fs::remove_file(&temp);
    }
    pointed
}

/// Waits for, then holds, an exclusive lock on `file`, released when it is closed.
fn lock_exclusive(file: &File) -> io::Result<()> {
    loop {
        match file.lock() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            locked => return locked,
        }
    }
}

/// Where a write can fail, for tests that fail it after each step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Memory,
    State,
    Staged,
    Renamed,
    Pointed,
}

/// Writes a snapshot of a paused VM into `dir` as a new generation, creating `dir` if
/// needed, and makes it the one in use. Returns the generation's directory, held open:
/// where a working set recorded from it goes, wherever it goes. The tests' way; a VM
/// stages its snapshot and commits it apart (review 1.17).
#[cfg(all(test, unix))]
fn write(dir: &Path, snap: &Snapshot, memory: &GuestMemory) -> Result<File, String> {
    write_with(dir, snap, memory, &mut |_| Ok(()))
}

#[cfg(all(test, unix))]
fn write_with(
    dir: &Path,
    snap: &Snapshot,
    memory: &GuestMemory,
    after: &mut dyn FnMut(Step) -> io::Result<()>,
) -> Result<File, String> {
    let staged = stage_with(dir, snap, memory, after)?;
    let generation = staged.generation()?;
    staged.commit_with(after)?;
    Ok(generation)
}

/// A snapshot written, but not yet durable nor in use: all a paused VM must wait for. A
/// VM that goes on runs again before [`Staged::commit`] makes it durable and points `dir`
/// at it, since its memory and state are in their files by then (PM M63). Until then it
/// is a staging directory, which no reader takes for a generation; dropped uncommitted,
/// it is a dead writer's, which the next write removes.
#[derive(Debug)]
pub struct Staged {
    dir: PathBuf,
    name: String,
    staging: PathBuf,
    /// Its memory and state, made durable by the commit.
    files: [File; 2],
    /// Writers to one snapshot take turns, this one until its commit.
    _lock: File,
}

/// Stages a snapshot of a paused VM in `dir`, creating `dir` if needed ([`Staged`]).
pub fn stage(dir: &Path, snap: &Snapshot, memory: &GuestMemory) -> Result<Staged, String> {
    stage_with(dir, snap, memory, &mut |_| Ok(()))
}

fn stage_with(
    dir: &Path,
    snap: &Snapshot,
    memory: &GuestMemory,
    after: &mut dyn FnMut(Step) -> io::Result<()>,
) -> Result<Staged, String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", dir.display());
    // The state first: a configuration a restore could not use fails before anything is
    // written.
    let name = fresh_name();
    let identities = backing(&snap.config)
        .map(|(path, _)| {
            path_bytes(path)?;
            Identity::of(path).map_err(|e| format!("{}: {e}", path.display()))
        })
        .collect::<Result<Vec<_>, String>>()
        .map_err(|e| at(&e))?;
    let state = encode(snap, &name, &identities).map_err(|e| at(&e))?;
    fs::create_dir_all(dir).map_err(|e| at(&e))?;
    // Writers to one snapshot take turns: a generation another writer has renamed into
    // place but not yet pointed at is never removed as superseded.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(LOCK))
        .map_err(|e| at(&e))?;
    lock_exclusive(&lock).map_err(|e| at(&e))?;
    let staging = dir.join(format!(".{name}.tmp"));
    fs::create_dir(&staging).map_err(|e| at(&e))?;
    let files = (|| -> io::Result<[File; 2]> {
        let memory_file = create_unsynced(&staging.join(MEMORY), |f| memory.save(f))?;
        after(Step::Memory)?;
        let state_file = create_unsynced(&staging.join(STATE), |mut f| f.write_all(&state))?;
        after(Step::State)?;
        Ok([memory_file, state_file])
    })();
    match files {
        Ok(files) => Ok(Staged {
            dir: dir.to_path_buf(),
            name,
            staging,
            files,
            _lock: lock,
        }),
        Err(e) => {
            tidy(dir);
            Err(at(&e))
        }
    }
}

impl Staged {
    /// The generation's name, once it is in place.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The generation's directory, held open: it stays the generation's once the commit
    /// renames it into place.
    pub fn generation(&self) -> Result<File, String> {
        platform::open_dir(&self.staging).map_err(|e| format!("{}: {e}", self.staging.display()))
    }

    /// Makes the snapshot durable and the one in use.
    pub fn commit(self) -> Result<(), String> {
        self.commit_with(&mut |_| Ok(()))
    }

    fn commit_with(self, after: &mut dyn FnMut(Step) -> io::Result<()>) -> Result<(), String> {
        let Staged {
            dir,
            name,
            staging,
            files,
            _lock,
        } = self;
        let committed = (|| -> io::Result<()> {
            for file in &files {
                platform::sync_durable(file)?;
            }
            platform::sync_dir(&staging)?;
            after(Step::Staged)?;
            fs::rename(&staging, dir.join(&name))?;
            after(Step::Renamed)?;
            platform::sync_dir(&dir)?;
            point(&dir, &name)?;
            after(Step::Pointed)
        })();
        tidy(&dir);
        committed.map_err(|e| format!("{}: {e}", dir.display()))
    }
}

/// Removes everything but what `current` names, which is whole: superseded generations,
/// and what failed or dead writers left.
fn tidy(dir: &Path) {
    let keep = current(dir).ok();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let entry_name = entry.file_name();
            let entry_name = entry_name.to_string_lossy();
            let generation = is_generation(&entry_name) && keep.as_deref() != Some(&*entry_name);
            let leftover = entry_name.starts_with(".g-") || entry_name.starts_with(&format!(".{CURRENT}."));
            if generation || leftover {
                let _ = fs::remove_dir_all(entry.path()).or_else(|_| fs::remove_file(entry.path()));
            }
        }
    }
}

/// Whether `dir` holds a snapshot: a pointer to a generation with its state.
pub fn exists(dir: &Path) -> bool {
    current(dir).is_ok_and(|name| dir.join(name).join(STATE).is_file())
}

/// A generation that could not be opened: gone (a newer write removed it) or unusable.
enum Open {
    Gone(String),
    Bad(String),
}

/// Reads the generation `dir` points at, pinned.
pub fn read(dir: &Path) -> Result<Pinned, String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", dir.display());
    let mut gone = String::new();
    for _ in 0..READ_ATTEMPTS {
        let name = current(dir).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => at(&"no snapshot here"),
            _ => at(&format!("{CURRENT}: {e}")),
        })?;
        match open(dir, &name) {
            Ok(pinned) => return Ok(pinned),
            Err(Open::Gone(e)) => gone = e,
            Err(Open::Bad(e)) => return Err(e),
        }
    }
    Err(at(&format!("its generations kept being replaced: {gone}")))
}

fn open(dir: &Path, name: &str) -> Result<Pinned, Open> {
    let path = dir.join(name);
    let at = |file: &str, e: &dyn std::fmt::Display| format!("{}: {e}", path.join(file).display());
    let lost = |file: &str, e: io::Error| {
        if e.kind() == io::ErrorKind::NotFound {
            Open::Gone(at(file, &e))
        } else {
            Open::Bad(at(file, &e))
        }
    };
    let bytes = platform::read_input(&path.join(STATE), MAX_STATE)
        .map_err(|e| Open::Bad(at(STATE, &e)))?
        .ok_or_else(|| Open::Gone(at(STATE, &"gone")))?;
    let decoded = decode(&bytes).map_err(|e| Open::Bad(at(STATE, &e)))?;
    if decoded.generation != name {
        return Err(Open::Bad(at(
            STATE,
            &format!("the state of generation {}", decoded.generation),
        )));
    }
    let snapshot = decoded.snapshot;
    for ((file, read_only), saved) in backing(&snapshot.config).zip(&decoded.identities) {
        let now = Identity::of(file).map_err(|e| Open::Bad(format!("{}: {e}", file.display())))?;
        let same = now.dev == saved.dev && now.ino == saved.ino;
        if !same || (read_only && now != *saved) {
            return Err(Open::Bad(format!(
                "{}: not the file this snapshot was taken with",
                file.display()
            )));
        }
    }
    let memory = platform::open_input(&path.join(MEMORY), false).map_err(|e| lost(MEMORY, e))?;
    let expected = snapshot.config.memory_mib.saturating_mul(1 << 20);
    let actual = memory.metadata().map_err(|e| Open::Bad(at(MEMORY, &e)))?.len();
    if actual != expected {
        return Err(Open::Bad(at(
            MEMORY,
            &format!("{actual} bytes; the snapshot's guest has {expected}"),
        )));
    }
    Ok(Pinned {
        snapshot,
        memory,
        path,
        name: name.to_string(),
    })
}

pub fn encode_working_set(pages: &[Touch], page: u64) -> Vec<u8> {
    let mut w = Writer::default();
    WORKING_SET_MAGIC.iter().for_each(|&b| w.u8(b));
    w.u32(WORKING_SET_VERSION);
    w.bytes(std::env::consts::ARCH.as_bytes());
    w.u64(page);
    w.seq(pages, |w, t| w.u64(t.gpa | u64::from(t.written)));
    w.into_bytes()
}

/// A working set recorded at stage-2 pages of `page` bytes, of at most `max_pages` pages;
/// `None` for one recorded at another page size. Its pages must be aligned and distinct
/// (audit A16), in the order they were recorded: first touch on HVF, address on KVM.
fn decode_working_set(bytes: &[u8], page: u64, max_pages: usize) -> codec::Result<Option<Vec<Touch>>> {
    let mut r = Reader::new(bytes);
    let mut magic = [0u8; 8];
    for b in &mut magic {
        *b = r.u8()?;
    }
    if magic != WORKING_SET_MAGIC {
        return Err(DecodeError("not a shards working set".into()));
    }
    let version = r.u32()?;
    if version != WORKING_SET_VERSION {
        return Err(DecodeError(format!(
            "working set format {version}; this shards reads {WORKING_SET_VERSION}"
        )));
    }
    if r.bytes(32)? != std::env::consts::ARCH.as_bytes() {
        return Err(DecodeError("a working set of another architecture".into()));
    }
    let recorded = r.u64()?;
    if recorded != page {
        return Ok(None);
    }
    let pages = r.seq(max_pages, 8, |r| {
        let entry = r.u64()?;
        Ok(Touch {
            gpa: entry & !1,
            written: entry & 1 != 0,
        })
    })?;
    r.finish()?;
    let aligned = pages.iter().all(|t| page.is_power_of_two() && t.gpa % page == 0);
    // Reserved as the entries were: a set the host cannot hold is refused, not an abort.
    let mut gpas: Vec<u64> = Vec::new();
    gpas.try_reserve_exact(pages.len())
        .map_err(|e| DecodeError(format!("a working set of {} pages: {e}", pages.len())))?;
    gpas.extend(pages.iter().map(|t| t.gpa));
    gpas.sort_unstable();
    let distinct = gpas.windows(2).all(|w| matches!(w, [a, b] if a != b));
    if !aligned || !distinct {
        return Err(DecodeError(
            "a working set whose pages are not aligned and distinct".into(),
        ));
    }
    Ok(Some(pages))
}

/// Saves `pages`, recorded at stage-2 pages of `page` bytes, as the working set of the
/// generation whose directory `generation` holds open, wherever it has gone.
fn write_working_set(generation: &File, pages: &[Touch], page: u64) -> Result<(), String> {
    platform::write_in(generation, WORKING_SET, &encode_working_set(pages, page))
        .map_err(|e| format!("the working set: {e}"))
}

/// The working set `bytes` holds, as [`encode_working_set`] wrote it and as a restore
/// would take it: its pages aligned, distinct and no more than `max_pages`; `None` for one
/// recorded at another page size.
fn decode_working_set_bytes(bytes: &[u8], page: u64, max_pages: u64) -> Result<Option<Vec<Touch>>, String> {
    let at = |e: &dyn std::fmt::Display| format!("the working set: {e}");
    let max_pages = usize::try_from(max_pages).map_err(|e| at(&e))?;
    decode_working_set(bytes, page, max_pages).map_err(|e| at(&e))
}

/// Writes the working set a VM recorded from generation `name` of the snapshot in `dir`
/// (the daemon's: no VM writes a snapshot it restores, D30). Taken only if `name` is still
/// the generation `dir` is at, and only as a restore would take it: recorded at `page`
/// bytes, its pages aligned, distinct and no more than `max_pages` of the snapshot's guest.
/// Returns how many pages it wrote: 0 for a set recorded from a generation since replaced,
/// or at another page size.
pub fn accept_working_set(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    page: u64,
    max_pages: impl FnOnce(&Snapshot) -> u64,
) -> Result<usize, String> {
    let pinned = read(dir)?;
    if pinned.name != name {
        return Ok(0);
    }
    let Some(pages) = decode_working_set_bytes(bytes, page, max_pages(&pinned.snapshot))? else {
        return Ok(0);
    };
    let generation =
        platform::open_dir(&pinned.path).map_err(|e| format!("{}: {e}", pinned.path.display()))?;
    write_working_set(&generation, &pages, page)?;
    Ok(pages.len())
}

/// The working set saved in generation directory `generation`, if it has one recorded at
/// stage-2 pages of `page` bytes. It holds at most `max_pages` pages, as many as the
/// guest has; a file too long for that is not read.
pub fn read_working_set(generation: &Path, page: u64, max_pages: u64) -> Result<Option<Vec<Touch>>, String> {
    let at = |e: &dyn std::fmt::Display| format!("the working set: {e}");
    let max_bytes = max_pages
        .checked_mul(8)
        .and_then(|b| b.checked_add(WORKING_SET_HEADER))
        .ok_or_else(|| at(&"a guest too large to bound"))?;
    let Some(bytes) = platform::read_input(&generation.join(WORKING_SET), max_bytes).map_err(|e| at(&e))?
    else {
        return Ok(None);
    };
    let max_pages = usize::try_from(max_pages).map_err(|e| at(&e))?;
    decode_working_set(&bytes, page, max_pages).map_err(|e| at(&e))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A snapshot of a machine backed by `a.img`, `b.img` and `base.erofs` in `dir`, its
    /// architecture state starting with `marker`.
    fn sample(dir: &Path, marker: u8) -> Snapshot {
        Snapshot {
            config: MachineConfig {
                vcpus: 4,
                memory_mib: 1,
                disks: vec![(dir.join("a.img"), true), (dir.join("b.img"), false)],
                pmem: vec![dir.join("base.erofs")],
                vsock: true,
                net: Some([2, 0, 0, 0, 0, 1]),
            },
            arch: vec![marker, 2, 3],
            devices: vec![9; 100],
        }
    }

    /// An absolute directory on every OS, for snapshots that are only encoded.
    fn nowhere() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"C:\shards" } else { "/shards" })
    }

    fn touches(gpas: &[u64]) -> Vec<Touch> {
        gpas.iter()
            .map(|&gpa| Touch {
                gpa,
                written: gpa % 0x8000 == 0,
            })
            .collect()
    }

    #[test]
    fn rejects_foreign_or_damaged_state() {
        let snap = sample(&nowhere(), 1);
        let ids = vec![Identity::default(); 3];
        let good = encode(&snap, "g-1", &ids).unwrap();
        let decoded = decode(&good).unwrap();
        assert_eq!((decoded.snapshot, decoded.generation.as_str()), (snap, "g-1"));
        for cut in 0..good.len() {
            assert!(decode(&good[..cut]).is_err());
        }
        let mut bad_magic = good.clone();
        bad_magic[0] ^= 1;
        assert!(decode(&bad_magic).is_err());
        let mut bad_version = good.clone();
        bad_version[8] = 99;
        assert!(decode(&bad_version).is_err());
        let mut trailing = good;
        trailing.push(0);
        assert!(decode(&trailing).is_err());
        // As many identities as backing files.
        let short = encode(&sample(&nowhere(), 1), "g-1", &ids[1..]).unwrap();
        assert!(decode(&short).unwrap_err().0.contains("2 identities for 3"));
    }

    /// Backing files are named by absolute path (audit A18).
    #[test]
    fn a_relative_backing_path_is_refused() {
        let mut snap = sample(&nowhere(), 1);
        snap.config.pmem = vec![PathBuf::from("base.erofs")];
        let ids = vec![Identity::default(); 3];
        assert!(encode(&snap, "g-1", &ids).unwrap_err().contains("absolute"));
    }

    /// A path that is not UTF-8 survives a round trip (audit A18).
    #[cfg(unix)]
    #[test]
    fn paths_round_trip_in_the_os_bytes() {
        use std::os::unix::ffi::OsStrExt as _;
        let path = nowhere().join(std::ffi::OsStr::from_bytes(b"pm\xffem"));
        let mut snap = sample(&nowhere(), 1);
        snap.config.pmem = vec![path.clone()];
        let state = encode(&snap, "g-1", &[Identity::default(); 3]).unwrap();
        assert_eq!(decode(&state).unwrap().snapshot.config.pmem, vec![path]);
    }

    #[test]
    fn working_sets_round_trip_and_reject_damage() {
        let pages = touches(&[0x8000_4000, 0xc000_0000]);
        let good = encode_working_set(&pages, 16384);
        assert_eq!(decode_working_set(&good, 16384, 8).unwrap().unwrap(), pages);
        assert_eq!(
            decode_working_set(&good, 4096, 8).unwrap(),
            None,
            "recorded at another page size"
        );
        for cut in 0..good.len() {
            assert!(decode_working_set(&good[..cut], 16384, 8).is_err());
        }
        let mut bad_magic = good.clone();
        bad_magic[0] ^= 1;
        assert!(decode_working_set(&bad_magic, 16384, 8).is_err());
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(decode_working_set(&trailing, 16384, 8).is_err());
        // Pages keep the order they were recorded in.
        let touched = touches(&[0xc000_0000, 0x8000_4000, 0x9000_0000]);
        let bytes = encode_working_set(&touched, 16384);
        assert_eq!(decode_working_set(&bytes, 16384, 8).unwrap().unwrap(), touched);
        // More pages than the guest has, unaligned pages, and repeated ones.
        assert!(decode_working_set(&good, 16384, 1).is_err());
        for bad in [
            &[0x8000_4002][..],
            &[0x8000_4000, 0x8000_4000],
            &[0x8000_4000, 0xc000_0000, 0x8000_4000],
        ] {
            let bytes = encode_working_set(&touches(bad), 16384);
            assert!(decode_working_set(&bytes, 16384, 8).is_err(), "{bad:x?}");
        }
    }

    /// A few bytes claiming 2^26 pages fail before anything is allocated for them (audit
    /// A16).
    #[test]
    fn a_short_working_set_claiming_many_pages_allocates_nothing() {
        let mut claim = encode_working_set(&[], 4096);
        let count_at = claim.len() - 4;
        claim[count_at..].copy_from_slice(&(1u32 << 26).to_le_bytes());
        claim.extend_from_slice(&[0; 16]);
        let e = decode_working_set(&claim, 4096, 1 << 26).unwrap_err();
        assert!(e.0.contains("bytes left"), "{e}");
    }

    /// The store on disk. It pins generations by their open directories, which only the
    /// Unix platform layer opens files in; Windows has no backend to take snapshots with
    /// yet (build.rs).
    #[cfg(unix)]
    mod on_disk {
        use super::*;

        /// A directory of its own under the system's temporary directory, holding the
        /// files [`sample`] names, removed on drop.
        struct Scratch(PathBuf);

        impl Scratch {
            fn new(name: &str) -> Scratch {
                static N: AtomicU64 = AtomicU64::new(0);
                let dir = std::env::temp_dir().join(format!(
                    "shards-snap-{name}-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                let _ = fs::remove_dir_all(&dir);
                fs::create_dir_all(&dir).unwrap();
                let s = Scratch(fs::canonicalize(&dir).unwrap());
                s.file("a.img", 4096, 1);
                s.file("b.img", 4096, 2);
                s.file("base.erofs", 8192, 3);
                s
            }

            /// A file here, `len` bytes of `fill`.
            fn file(&self, name: &str, len: usize, fill: u8) -> PathBuf {
                let path = self.0.join(name);
                fs::write(&path, vec![fill; len]).unwrap();
                path
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        const RAM: [(u64, usize); 1] = [(0x8000_0000, 1 << 20)];

        /// Guest RAM whose every byte is `marker`.
        fn ram(marker: u8) -> GuestMemory {
            let mem = GuestMemory::anonymous(&RAM).unwrap();
            mem.access()
                .unwrap()
                .write(0x8000_0000, &vec![marker; 1 << 20])
                .unwrap();
            mem
        }

        /// The marker of a pinned generation's state and the first byte of its memory,
        /// which agree in a whole generation.
        fn markers(p: &Pinned) -> (u8, u8) {
            let mut first = [0u8; 1];
            platform::read_exact_at(&p.memory, &mut first, 0).unwrap();
            (p.snapshot.arch[0], first[0])
        }

        fn generations(dir: &Path) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n != LOCK)
                .collect();
            names.sort();
            names
        }

        #[test]
        fn round_trips_on_disk() {
            let s = Scratch::new("round-trip");
            let dir = s.0.join("snap");
            let mem = GuestMemory::anonymous(&RAM).unwrap();
            mem.access().unwrap().write(0x8000_0010, b"guest").unwrap();
            write(&dir, &sample(&s.0, 1), &mem).unwrap();
            assert!(exists(&dir));
            let p = read(&dir).unwrap();
            assert_eq!(p.snapshot, sample(&s.0, 1));
            let restored = GuestMemory::from_file(&RAM, &p.memory).unwrap();
            let mut buf = [0u8; 5];
            restored.access().unwrap().read(0x8000_0010, &mut buf).unwrap();
            assert_eq!(&buf, b"guest");
            // One generation, and the pointer to it.
            let names = generations(&dir);
            assert_eq!(names.len(), 2, "{names:?}");
            assert!(names.contains(&CURRENT.to_string()), "{names:?}");
            assert!(!exists(&s.0.join("elsewhere")));
            assert!(
                read(&s.0.join("elsewhere"))
                    .unwrap_err()
                    .contains("no snapshot here")
            );
        }

        /// A replacement that fails after any of its steps leaves the snapshot readable,
        /// and whole: the generation before, or, once the pointer moved, the one after,
        /// never one's state with the other's memory (audit A03).
        #[test]
        fn a_failed_replacement_leaves_one_whole_generation() {
            let steps = [
                Step::Memory,
                Step::State,
                Step::Staged,
                Step::Renamed,
                Step::Pointed,
            ];
            for step in steps {
                let s = Scratch::new("fail");
                let dir = s.0.join("snap");
                write(&dir, &sample(&s.0, 1), &ram(1)).unwrap();
                let failed = write_with(&dir, &sample(&s.0, 2), &ram(2), &mut |at| {
                    if at == step {
                        Err(io::Error::other("injected"))
                    } else {
                        Ok(())
                    }
                });
                assert!(failed.unwrap_err().contains("injected"), "{step:?}");
                let p = read(&dir).unwrap();
                let want = if step == Step::Pointed { 2 } else { 1 };
                assert_eq!(markers(&p), (want, want), "{step:?}");
                // Nothing is left of the failed write but, at worst, the generation it
                // published.
                assert_eq!(generations(&dir).len(), 2, "{step:?}: {:?}", generations(&dir));
                // And the next write works.
                write(&dir, &sample(&s.0, 3), &ram(3)).unwrap();
                assert_eq!(markers(&read(&dir).unwrap()), (3, 3), "{step:?}");
            }
        }

        /// What a writer that died left, a staging directory, a generation renamed into
        /// place but never pointed at, and a pointer never renamed, is not read, and the
        /// next write removes it (audit A03).
        #[test]
        fn a_dead_writers_leftovers_are_passed_over_then_removed() {
            let s = Scratch::new("dead");
            let dir = s.0.join("snap");
            write(&dir, &sample(&s.0, 1), &ram(1)).unwrap();
            let unpointed = fresh_name();
            fs::create_dir(dir.join(&unpointed)).unwrap();
            fs::create_dir(dir.join(format!(".{}.tmp", fresh_name()))).unwrap();
            fs::write(dir.join(format!(".{CURRENT}.{unpointed}.tmp")), &unpointed).unwrap();
            assert_eq!(markers(&read(&dir).unwrap()), (1, 1));
            write(&dir, &sample(&s.0, 2), &ram(2)).unwrap();
            assert_eq!(markers(&read(&dir).unwrap()), (2, 2));
            assert_eq!(generations(&dir).len(), 2, "{:?}", generations(&dir));
        }

        /// A pointer that names no generation is refused, however long it is.
        #[test]
        fn a_pointer_to_no_generation_is_refused() {
            let s = Scratch::new("pointer");
            let dir = s.0.join("snap");
            write(&dir, &sample(&s.0, 1), &ram(1)).unwrap();
            let name = current(&dir).unwrap();
            for bad in [
                format!("{name}{}\n", "0".repeat(64)),
                "../snap\n".into(),
                String::new(),
            ] {
                fs::write(dir.join(CURRENT), &bad).unwrap();
                assert!(!exists(&dir), "{bad:?}");
                assert!(
                    read(&dir).unwrap_err().contains("a pointer to no generation"),
                    "{bad:?}"
                );
            }
        }

        /// Readers racing writers pin one whole generation each time (audit A03).
        #[test]
        fn readers_racing_writers_see_whole_generations() {
            let s = Scratch::new("race");
            let dir = s.0.join("snap");
            let snaps: Vec<Snapshot> = (1..=4).map(|m| sample(&s.0, m)).collect();
            let rams: Vec<GuestMemory> = (1..=4).map(ram).collect();
            write(&dir, &snaps[0], &rams[0]).unwrap();
            let done = std::sync::atomic::AtomicBool::new(false);
            std::thread::scope(|scope| {
                let readers: Vec<_> = (0..3)
                    .map(|_| {
                        scope.spawn(|| {
                            let mut seen = 0;
                            while !done.load(Ordering::Relaxed) {
                                let p = read(&dir).unwrap();
                                let (state, memory) = markers(&p);
                                assert_eq!(state, memory, "a mixed snapshot");
                                seen += 1;
                            }
                            seen
                        })
                    })
                    .collect();
                for round in 0..60 {
                    let k = round % 4;
                    write(&dir, &snaps[k], &rams[k]).unwrap();
                }
                done.store(true, Ordering::Relaxed);
                for r in readers {
                    assert!(r.join().unwrap() > 0);
                }
            });
        }

        /// Writers to one snapshot take turns; afterwards it holds one whole generation.
        #[test]
        fn concurrent_writers_leave_one_whole_generation() {
            let s = Scratch::new("writers");
            let dir = s.0.join("snap");
            let snaps: Vec<Snapshot> = (1..=4).map(|m| sample(&s.0, m)).collect();
            let rams: Vec<GuestMemory> = (1..=4).map(ram).collect();
            std::thread::scope(|scope| {
                for k in 0..4 {
                    let (dir, snap, mem) = (&dir, &snaps[k], &rams[k]);
                    scope.spawn(move || {
                        for _ in 0..5 {
                            write(dir, snap, mem).unwrap();
                        }
                    });
                }
            });
            let (state, memory) = markers(&read(&dir).unwrap());
            assert_eq!(state, memory);
            assert_eq!(generations(&dir).len(), 2, "{:?}", generations(&dir));
        }

        /// A generation's state that names another generation is refused.
        #[test]
        fn a_state_moved_between_generations_is_refused() {
            let s = Scratch::new("moved-state");
            let dir = s.0.join("snap");
            write(&dir, &sample(&s.0, 1), &ram(1)).unwrap();
            let name = current(&dir).unwrap();
            let state = dir.join(&name).join(STATE);
            let mut bytes = fs::read(&state).unwrap();
            let at = bytes
                .windows(name.len())
                .position(|w| w == name.as_bytes())
                .unwrap();
            bytes[at + name.len() - 1] ^= 1;
            fs::write(&state, &bytes).unwrap();
            assert!(read(&dir).unwrap_err().contains("the state of generation"));
        }

        /// A write refuses a relative backing path before it writes anything, and a
        /// restore refuses another file under a backing file's name (audit A18).
        #[test]
        fn backing_files_must_be_the_files_the_snapshot_was_taken_with() {
            let s = Scratch::new("backing");
            let dir = s.0.join("snap");
            let mut relative = sample(&s.0, 1);
            relative.config.pmem = vec![PathBuf::from("base.erofs")];
            assert!(write(&dir, &relative, &ram(1)).unwrap_err().contains("absolute"));
            assert!(!dir.exists());

            // A read-only file that changed is refused; so is another file at a writable
            // disk's path. A writable disk that the guest wrote to is not.
            let snap = sample(&s.0, 1);
            write(&dir, &snap, &ram(1)).unwrap();
            let [(ro, _), (rw, _)] = [&snap.config.disks[0], &snap.config.disks[1]].map(|d| d.clone());
            fs::write(&rw, vec![7u8; 4096]).unwrap();
            read(&dir).unwrap();
            fs::write(&ro, vec![5u8; 8192]).unwrap();
            assert!(
                read(&dir)
                    .unwrap_err()
                    .contains("a.img: not the file this snapshot was taken with")
            );
            s.file("a.img", 4096, 1);
            write(&dir, &snap, &ram(1)).unwrap();
            // Made beside it and renamed over it, so it is surely another inode.
            fs::rename(s.file("b.new", 4096, 2), &rw).unwrap();
            assert!(
                read(&dir)
                    .unwrap_err()
                    .contains("b.img: not the file this snapshot was taken with")
            );
        }

        /// A file whose name is not UTF-8 backs a snapshot as any other does. APFS refuses
        /// such names.
        #[cfg(target_os = "linux")]
        #[test]
        fn a_file_named_in_bytes_that_are_not_utf8_backs_a_snapshot() {
            use std::os::unix::ffi::OsStrExt as _;
            let s = Scratch::new("bytes");
            let path = s.0.join(std::ffi::OsStr::from_bytes(b"pm\xffem"));
            fs::write(&path, [0u8; 4096]).unwrap();
            let mut snap = sample(&s.0, 1);
            snap.config.pmem = vec![path.clone()];
            let dir = s.0.join("snap");
            write(&dir, &snap, &ram(1)).unwrap();
            assert_eq!(read(&dir).unwrap().snapshot.config.pmem, vec![path]);
        }

        /// A working set longer than the guest has pages is not read (audit A16).
        #[test]
        fn a_working_set_longer_than_the_guest_is_not_read() {
            let s = Scratch::new("wset-cap");
            let dir = s.0.join("snap");
            write(&dir, &sample(&s.0, 1), &ram(1)).unwrap();
            let p = read(&dir).unwrap();
            let many: Vec<u64> = (0..20).map(|i| 0x8000_0000 + i * 16384).collect();
            write_working_set(&platform::open_dir(&p.path).unwrap(), &touches(&many), 16384).unwrap();
            assert_eq!(read_working_set(&p.path, 16384, 20).unwrap().unwrap().len(), 20);
            assert!(
                read_working_set(&p.path, 16384, 2)
                    .unwrap_err()
                    .contains("past the limit")
            );
        }

        /// A working set goes with the generation it was recorded from, wherever its
        /// directory has gone: the one a write returns, or a read pins.
        /// A working set a VM sends is written only as a restore would read it: for the
        /// generation it names while that is current, at this page size, within the guest.
        #[test]
        fn a_sent_working_set_is_written_only_as_a_restore_would_take_it() {
            let s = Scratch::new("accept");
            let dir = s.0.join("snap");
            write(&dir, &sample(&s.0, 1), &ram(1)).unwrap();
            let name = read(&dir).unwrap().name;
            let (page, max) = (0x4000, 64);
            let set = touches(&[0x8000_0000, 0x8000_4000, 0x8000_c000]);
            let bytes = encode_working_set(&set, page);
            let stored = || read_working_set(&read(&dir).unwrap().path, page, max).unwrap();
            // Another generation's, or recorded at another page size: nothing is written.
            assert_eq!(
                accept_working_set(&dir, "g-not-this", &bytes, page, |_| max),
                Ok(0)
            );
            let other_page = encode_working_set(&set, page * 2);
            assert_eq!(accept_working_set(&dir, &name, &other_page, page, |_| max), Ok(0));
            assert_eq!(stored(), None);
            // Past the guest, or not a working set: refused, and nothing is written.
            assert!(accept_working_set(&dir, &name, &bytes, page, |_| 2).is_err());
            assert!(accept_working_set(&dir, &name, b"not a working set", page, |_| max).is_err());
            assert_eq!(stored(), None);
            // The set that fits: written, and read back as sent.
            assert_eq!(accept_working_set(&dir, &name, &bytes, page, |_| max), Ok(3));
            assert_eq!(stored(), Some(set));
        }

        #[test]
        fn working_sets_land_in_their_generation() {
            let pages = touches(&[0x8000_4000]);
            let s = Scratch::new("wset");
            let dir = s.0.join("snap");
            let written = write(&dir, &sample(&s.0, 1), &ram(1)).unwrap();
            let p = read(&dir).unwrap();
            assert_eq!(read_working_set(&p.path, 16384, 8).unwrap(), None, "none saved");
            let moved = s.0.join("moved");
            fs::rename(&dir, &moved).unwrap();
            write_working_set(&written, &pages, 16384).unwrap();
            let again = read(&moved).unwrap();
            assert_eq!(
                read_working_set(&again.path, 16384, 8).unwrap().unwrap(),
                pages,
                "written where the generation went"
            );
            // A newer generation starts without one.
            write(&moved, &sample(&s.0, 2), &ram(2)).unwrap();
            let newer = read(&moved).unwrap();
            assert_eq!(read_working_set(&newer.path, 16384, 8).unwrap(), None);
        }

        /// Working sets saved at once into one generation each land whole (a pool's
        /// restores record together).
        #[test]
        fn working_sets_saved_at_once_each_land_whole() {
            let s = Scratch::new("wset-race");
            let dir = s.0.join("snap");
            write(&dir, &sample(&s.0, 1), &ram(1)).unwrap();
            let p = read(&dir).unwrap();
            let sets: Vec<Vec<Touch>> = (1..=8u64)
                .map(|n| touches(&(0..n * 64).map(|i| 0x8000_0000 + i * 16384).collect::<Vec<_>>()))
                .collect();
            let generation = platform::open_dir(&p.path).unwrap();
            std::thread::scope(|scope| {
                for set in &sets {
                    let generation = &generation;
                    scope.spawn(move || {
                        for _ in 0..20 {
                            write_working_set(generation, set, 16384).unwrap();
                        }
                    });
                }
            });
            let saved = read_working_set(&p.path, 16384, 1 << 20).unwrap().unwrap();
            assert!(sets.contains(&saved), "{} pages", saved.len());
            let names = generations(&dir.join(current(&dir).unwrap()));
            assert_eq!(names, [MEMORY, STATE, WORKING_SET], "no temporary files are left");
        }
    }
}
