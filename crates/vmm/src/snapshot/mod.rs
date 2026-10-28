//! VM snapshots on disk. A snapshot directory holds generations, each a complete snapshot
//! in a directory of its own, and one pointer naming the generation in use:
//!
//! - `current`: the name of the generation in use
//! - `g-<id>/state`: a versioned header, the generation's name, the machine configuration
//!   restore rebuilds and the identity of each file backing it, the architecture's CPU and
//!   interrupt-controller state, and the devices' state
//! - `g-<id>/memory`: guest RAM, one region after another, then the memory a virtio-mem
//!   device had plugged, one run of its blocks after another, with all-zero pages as holes
//! - `g-<id>/swap`: in a snapshot of a run, the run's swap as its guest had it (D176)
//! - `g-<id>/layer-head` and `g-<id>/layer`: of a machine with a writable layer (D190), its
//!   device's head as memory, all-zero pages as holes, and its ext4 file system as the guest
//!   had it, which each run restored from the snapshot is given a copy of
//! - `g-<id>/window-heads`: of a machine with DAX windows (D189), their heads, the page
//!   structures its guest's kernel keeps there, kept as a layer's head is
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
/// A run's swap, as its guest had it, where a snapshot of a run keeps one (D176).
const SWAP: &str = "swap";
/// A writable layer's head, the page structures its guest's kernel wrote there (D190).
pub const LAYER_HEAD: &str = "layer-head";
/// A writable layer's file system, as its guest had it (D190).
pub const LAYER: &str = "layer";
/// A machine's DAX windows' heads, the page structures its guest's kernel wrote there
/// (D189).
pub const WINDOW_HEADS: &str = "window-heads";
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
/// 10: and how many shared directories (virtio-fs) follow it (D38).
/// 11: and whether a join disk follows the disks (D119).
/// 12: and whether a join share follows the shared directories (D119).
/// 13: and the memory the machine may have plugged, in MiB, none of it plugged (D148).
/// 14: and whether a virtio-balloon device gives back the memory the guest reports free
///     (D151).
/// 15: and the machine's GPU, by its window's length, if it has one (D150).
/// 16: and its swap window's length in MiB, none of it a run's (D159).
/// 17: the memory plugged, run by run, its bytes after the RAM's in the memory file; and
///     virtio-mem's sizes, what it asks for and what is plugged (D176).
/// 18: a swap window's state keeps the length of the run's swap mapped, whose bytes a
///     snapshot of a run keeps beside it (D176).
/// 19: a join disk's state keeps its ranges, each let go, so that a microVM containers
///     joined is saved once they end (D176).
/// 20: a share's state keeps its server's tables, so that a microVM with a volume mounted
///     is saved (D176).
/// 21: a share's tables keep the bytes of the files its guest holds open that no path
///     reaches, which a restored server holds anew (D176).
/// 22: and the length of each shared directory's DAX window in MiB, 0 for none (D189).
/// 23: a share's state keeps its DAX window's mappings, which a restore maps again from
///     its new server (D189, D176).
/// 24: and the length of its writable layer's file system in MiB, 0 for none, whose head and
///     file system the generation keeps beside it (D190).
/// 25: each DAX window has a head before it, its guest's page structures, which the
///     generation keeps beside it (D189).
/// 26: and a record of which of its pages its host holds after it, which nothing keeps
///     (D189).
const VERSION: u32 = 26;
/// The oldest format this build reads too where the machine has no DAX windows: for such
/// a machine each since says what this one says, so that a microVM saved by shards 0.6.0
/// (24) restores (D176).
const VERSION_WITHOUT_WINDOWS: u32 = 24;
/// The snapshot format this build writes and reads: what a snapshot kept for reuse is
/// keyed by.
pub const FORMAT: u32 = VERSION;
/// A state file is kilobytes; anything past this is not one of ours.
pub(crate) const MAX_STATE: u64 = 64 << 20;
const MAX_DISKS: usize = 64;
/// The most runs of plugged memory a state holds: one for every other 2 MiB block of 1
/// TiB to plug.
const MAX_PLUGGED_RUNS: usize = 1 << 18;
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
    /// How many shared directories (virtio-fs devices) follow: their directories are the
    /// restore's own (D38).
    pub shares: u32,
    /// Whether a join disk follows the disks (D119): empty in every snapshot, and the
    /// restore's own.
    pub join: bool,
    /// Whether a join share follows the shared directories (D119): served by none in every
    /// snapshot, and the restore's own.
    pub join_share: bool,
    /// Whether the guest has virtualization of its own (nested: on arm64, EL2). Never in a
    /// snapshot: its state at EL2 is not saved, so a snapshot of it is refused.
    pub nested: bool,
    /// Memory the guest may have plugged, in MiB, through a virtio-mem device (D133).
    /// What was plugged is the snapshot's [`plugged`](Snapshot::plugged): a template's
    /// none (D148), a running machine's its own (D176).
    pub hotplug_mib: u64,
    /// Whether a virtio-balloon device gives back the memory the guest reports free
    /// (D151), last of its devices.
    pub reporting: bool,
    /// A virtio-gpu device (D144), by the length of the window its blobs are mapped into,
    /// as the guest saw it: a restore's renderer must offer at least as much (D150).
    pub gpu: Option<u64>,
    /// The length of the guest's swap window in MiB (D159), 0 for none: a virtio-pmem
    /// device a run's swap file is mapped into, last of its devices. A snapshot keeps it
    /// with no run's file.
    pub swap_mib: u64,
    /// The length of each shared directory's DAX window in MiB (D189), 0 for none: a shared
    /// memory region (virtio-fs's cache) its guest maps the files it reads into, each share's
    /// and the join share's its own, after the swap window.
    pub share_window_mib: u64,
    /// The length of its writable layer's file system in MiB (D190), 0 for none: a
    /// virtio-pmem device after the pmem files, its head and file system kept beside the
    /// snapshot, and a restore's own copy of the file system mapped there.
    pub layer_mib: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub config: MachineConfig,
    /// The memory a virtio-mem device had plugged, `(gpa, len)` run by run in the order of
    /// their addresses, whose bytes follow the RAM's in the memory file (D176).
    pub plugged: Vec<(u64, u64)>,
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

/// The files a restore of a generation maps, held open by its restorer between restores
/// ([`hold`]): its memory, its writable layer's and DAX windows' heads where it has them,
/// and the files its guest only reads, its root filesystem's among them. A file no process
/// holds open gives up its vnode, and the pages its host had cached of it with them, to the
/// next file its host looks at once the host's vnode table is full; a Mac's of 128 GiB held
/// 263,168, recycling thousands a second, and a restore then read its whole working set
/// back from the disk a page at a time, and its guest its programs (PM M315).
#[derive(Debug)]
pub struct Held {
    generation: String,
    _files: Vec<File>,
}

impl Held {
    /// Whether it holds the generation `dir` points at now.
    pub fn is_current(&self, dir: &Path) -> bool {
        current(dir).is_ok_and(|name| name == self.generation)
    }
}

/// Holds the files of the generation `dir` points at open ([`Held`]).
pub fn hold(dir: &Path) -> Result<Held, String> {
    let pinned = read(dir)?;
    let mut files = Vec::new();
    for name in [LAYER_HEAD, WINDOW_HEADS] {
        match platform::open_beneath_input(dir, &[&pinned.name, name]) {
            Ok(file) => files.push(file),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", pinned.path.join(name).display())),
        }
    }
    for (path, read_only) in backing(&pinned.snapshot.config) {
        if read_only {
            files.push(File::open(path).map_err(|e| format!("{}: {e}", path.display()))?);
        }
    }
    files.push(pinned.memory);
    Ok(Held {
        generation: pinned.name,
        _files: files,
    })
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
            // Which file it is, as a Unix file's device and inode say: a file of the same
            // bytes and times renamed over it is another (audit A18).
            let (dev, ino) = platform::input_file_id(path)?;
            // 100 ns intervals since 1601.
            let written = m.last_write_time();
            Ok(Identity {
                dev,
                ino,
                size: m.len(),
                mtime_s: i64::try_from(written / 10_000_000).unwrap_or(i64::MAX),
                mtime_ns: u32::try_from(written % 10_000_000 * 100).unwrap_or(0),
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

/// Whether the snapshot in `dir` keeps a writable layer (D190): a VM restoring it is given
/// a file to copy it into.
pub fn has_layer(dir: &Path) -> Result<bool, String> {
    Ok(read(dir)?.snapshot.config.layer_mib > 0)
}

/// The files of the generation the snapshot in `dir` is at, for a VM in App Sandbox to be
/// granted before it reads the snapshot (shards `grant`; PM M70): its state and memory,
/// then its working set, its writable layer's head and file system (D190) and its DAX
/// windows' heads (D189), which it may not have. Its pointer is read as the VM's input.
pub fn generation_files(dir: &Path) -> Result<([PathBuf; 2], [PathBuf; 4]), String> {
    let name = current(dir).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => format!("{}: no snapshot here", dir.display()),
        _ => format!("{}: {CURRENT}: {e}", dir.display()),
    })?;
    let generation = dir.join(name);
    Ok((
        [generation.join(STATE), generation.join(MEMORY)],
        [
            generation.join(WORKING_SET),
            generation.join(LAYER_HEAD),
            generation.join(LAYER),
            generation.join(WINDOW_HEADS),
        ],
    ))
}

/// The directory of the generation the snapshot in `dir` is at, whose files a VM may not
/// read but its spawner may: a run's swap and what the spawner added (D176).
pub fn generation(dir: &Path) -> Result<PathBuf, String> {
    let name = current(dir).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => format!("{}: no snapshot here", dir.display()),
        _ => format!("{}: {CURRENT}: {e}", dir.display()),
    })?;
    Ok(dir.join(name))
}

/// The swap a snapshot of a run kept, in generation `generation` (D176).
pub fn swap_of(generation: &Path) -> PathBuf {
    generation.join(SWAP)
}

/// The writable layer's file system a snapshot kept, in generation `generation` (D190):
/// what each VM restored from it is given a copy of.
pub fn layer_of(generation: &Path) -> PathBuf {
    generation.join(LAYER)
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
    if s.config.nested {
        return Err(
            "a snapshot of a VM with virtualization of its own is refused: its EL2 state is not saved".into(),
        );
    }
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
    w.u32(s.config.shares);
    w.bool(s.config.join);
    w.bool(s.config.join_share);
    w.u64(s.config.hotplug_mib);
    w.bool(s.config.reporting);
    w.bool(s.config.gpu.is_some());
    w.u64(s.config.gpu.unwrap_or(0));
    w.u64(s.config.swap_mib);
    w.u64(s.config.share_window_mib);
    w.u64(s.config.layer_mib);
    w.seq(&s.plugged, |w, (gpa, len)| {
        w.u64(*gpa);
        w.u64(*len);
    });
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
    if !(VERSION_WITHOUT_WINDOWS..=VERSION).contains(&version) {
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
    let shares = r.u32()?;
    if shares as usize > MAX_DISKS {
        return Err(DecodeError(format!("{shares} shared directories")));
    }
    let join = r.bool()?;
    let join_share = r.bool()?;
    let hotplug_mib = r.u64()?;
    let reporting = r.bool()?;
    let has_gpu = r.bool()?;
    let gpu_window = r.u64()?;
    let swap_mib = r.u64()?;
    let share_window_mib = r.u64()?;
    // Whose windows were laid out otherwise (D189).
    if version != VERSION && share_window_mib > 0 && (shares > 0 || join_share) {
        return Err(DecodeError(format!(
            "snapshot format {version} of a machine with DAX windows; this shards reads {VERSION}"
        )));
    }
    let layer_mib = r.u64()?;
    // At most a run for every other block of the most a guest may plug.
    let plugged = r.seq(MAX_PLUGGED_RUNS, 16, |r| Ok((r.u64()?, r.u64()?)))?;
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
                shares,
                join,
                join_share,
                nested: false,
                hotplug_mib,
                reporting,
                gpu: has_gpu.then_some(gpu_window),
                swap_mib,
                share_window_mib,
                layer_mib,
            },
            plugged,
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

/// The generation `dir`'s pointer names, its newest version's (platform `write_in`). A
/// pointer is a name and a newline; no more of one is read.
fn current(dir: &Path) -> io::Result<String> {
    let mut bytes = Vec::new();
    platform::open_newest_beneath_input(dir, &[CURRENT])?
        .take(MAX_NAME as u64 + 2)
        .read_to_end(&mut bytes)?;
    let name = std::str::from_utf8(&bytes)
        .ok()
        .map(|text| text.trim_end_matches('\n'))
        .filter(|name| is_generation(name))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "a pointer to no generation"))?;
    Ok(name.to_string())
}

/// Creates `path`, a name no other file had, and fills it; its caller makes it durable
/// later. Only its user's: a generation holds a guest's memory.
fn create_unsynced(path: &Path, fill: impl FnOnce(&File) -> io::Result<()>) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(path)?;
    fill(&file)?;
    Ok(file)
}

/// Points `dir` at the generation `name`: a new pointer, synced, renamed into place, and the
/// directory synced ([`platform::write_in`]: over the old one, or on Windows, which in holds
/// of a minute and more refused renames over a file it had lately been given, as its next
/// version, PM M345). Its leftovers, `.current.…` and the versions Windows would not yet let
/// go of, [`tidy`] removes.
fn point(dir: &Path, name: &str) -> io::Result<()> {
    platform::write_in(&platform::open_dir(dir)?, CURRENT, format!("{name}\n").as_bytes())
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
#[cfg(test)]
fn write(dir: &Path, snap: &Snapshot, memory: &GuestMemory) -> Result<File, String> {
    write_with(dir, snap, memory, &mut |_| Ok(()))
}

#[cfg(test)]
fn write_with(
    dir: &Path,
    snap: &Snapshot,
    memory: &GuestMemory,
    after: &mut dyn FnMut(Step) -> io::Result<()>,
) -> Result<File, String> {
    let staged = stage_with(dir, snap, memory, Beside::default(), after)?;
    let generation = staged.generation()?;
    staged.commit_with(after)?;
    Ok(generation)
}

/// A snapshot written, but not yet durable nor in use: all a paused VM must wait for. A
/// VM that goes on runs again before [`Staged::commit`] makes it durable and points `dir`
/// at it, since its memory and state are in their files by then (PM M63). Until then it
/// is a staging directory, which no reader takes for a generation; dropped uncommitted,
/// it goes with its files, and what a dead writer left the next write removes.
#[derive(Debug)]
pub struct Staged {
    dir: PathBuf,
    name: String,
    staging: PathBuf,
    /// Its files, made durable by the commit: its memory and state, a run's swap where it
    /// keeps one (D176), its writable layer's head and file system (D190), and its DAX
    /// windows' heads (D189).
    files: Vec<File>,
    /// Which of its files is its writable layer's head as [`stage`] wrote it, where it
    /// keeps one.
    layer_head: Option<usize>,
    /// Which is its DAX windows' heads as [`stage`] wrote them, where it keeps them.
    window_heads: Option<usize>,
    /// Whether its commit has run, which leaves nothing of it the staging's to remove.
    committed: bool,
    /// Writers to one snapshot take turns, this one until its commit; none where its
    /// spawner staged it, which holds the lock.
    _lock: Option<File>,
    /// Its spawner, where the generation is staged by it for this process, which commits
    /// it ([`platform::Stager`], D196).
    by: Option<&'static dyn platform::Stager>,
}

/// What a snapshot keeps beside a machine's memory: its writable layer (D190) and its DAX
/// windows' heads (D189), where it has them.
#[derive(Debug, Clone, Copy, Default)]
pub struct Beside<'a> {
    pub layer: Option<&'a crate::devices::virtio::pmem::Layer>,
    pub window_heads: Option<&'a crate::devices::virtio::head::WindowHeads>,
}

/// Stages a snapshot of a paused VM in `dir`, creating `dir` if needed ([`Staged`]), with
/// what it keeps `beside` its memory.
pub fn stage(
    dir: &Path,
    snap: &Snapshot,
    memory: &GuestMemory,
    beside: Beside<'_>,
) -> Result<Staged, String> {
    match platform::stager() {
        Some(by) => stage_by(by, dir, snap, memory, beside),
        None => stage_with(dir, snap, memory, beside, &mut |_| Ok(())),
    }
}

/// [`stage`] for a process that writes in no directory of its own: its spawner stages the
/// generation in `dir` and hands it the generation's files, its memory and state and what
/// the machine keeps `beside` them, which it fills as [`stage_with`] fills its own, and its
/// commit has the spawner make durable and the one in use (D196).
fn stage_by(
    by: &'static dyn platform::Stager,
    dir: &Path,
    snap: &Snapshot,
    memory: &GuestMemory,
    beside: Beside<'_>,
) -> Result<Staged, String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", dir.display());
    let kept = platform::Kept {
        layer: beside.layer.is_some(),
        window_heads: beside.window_heads.is_some(),
    };
    let (name, files) = by.stage(dir, kept).map_err(|e| at(&e))?;
    if files.len() != kept.files() {
        return Err(at(&format!(
            "its spawner staged {} files of the {} it keeps",
            files.len(),
            kept.files()
        )));
    }
    let file = |i: usize| {
        files
            .get(i)
            .ok_or_else(|| at(&"a file its spawner did not stage"))
    };
    fill(&name, snap, memory, file(0)?, file(1)?).map_err(|e| at(&e))?;
    let mut next = 2;
    let mut layer_head = None;
    if let Some(layer) = beside.layer {
        layer.save(file(next)?, file(next + 1)?).map_err(|e| at(&e))?;
        layer_head = Some(next);
        next += 2;
    }
    let mut window_heads = None;
    if let Some(heads) = beside.window_heads {
        heads.save(file(next)?).map_err(|e| at(&e))?;
        window_heads = Some(next);
    }
    Ok(Staged {
        dir: dir.to_path_buf(),
        staging: dir.join(format!(".{name}.tmp")),
        name,
        files,
        layer_head,
        window_heads,
        committed: false,
        _lock: None,
        by: Some(by),
    })
}

fn stage_with(
    dir: &Path,
    snap: &Snapshot,
    memory: &GuestMemory,
    beside: Beside<'_>,
    after: &mut dyn FnMut(Step) -> io::Result<()>,
) -> Result<Staged, String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", dir.display());
    // The state first: a configuration a restore could not use fails before anything is
    // written.
    let name = fresh_name();
    let state = state_of(snap, &name).map_err(|e| at(&e))?;
    let mut staged = staging(dir, name)?;
    let files = (|| -> io::Result<(Vec<File>, Option<usize>, Option<usize>)> {
        let memory_file = create_unsynced(&staged.staging.join(MEMORY), |f| memory.save(f))?;
        after(Step::Memory)?;
        let state_file = create_unsynced(&staged.staging.join(STATE), |mut f| f.write_all(&state))?;
        after(Step::State)?;
        let mut files = vec![memory_file, state_file];
        let mut layer_head = None;
        if let Some(layer) = beside.layer {
            let head = create_unsynced(&staged.staging.join(LAYER_HEAD), |_| Ok(()))?;
            let data = create_unsynced(&staged.staging.join(LAYER), |_| Ok(()))?;
            layer.save(&head, &data).map_err(io::Error::other)?;
            layer_head = Some(files.len());
            files.extend([head, data]);
        }
        let mut window_heads = None;
        if let Some(heads) = beside.window_heads {
            let kept = create_unsynced(&staged.staging.join(WINDOW_HEADS), |f| heads.save(f))?;
            window_heads = Some(files.len());
            files.push(kept);
        }
        Ok((files, layer_head, window_heads))
    })();
    match files {
        Ok((files, layer_head, window_heads)) => {
            staged.layer_head = layer_head;
            staged.window_heads = window_heads;
            staged.files = files;
            Ok(staged)
        }
        Err(e) => {
            // Under the lock, which the staging holds until it goes.
            tidy(dir);
            Err(at(&e))
        }
    }
}

/// Stages an empty generation in `dir`, creating `dir` if needed, for a VM to fill
/// ([`fill`]) that writes only the files it is handed (D30): its memory and state and, for
/// a `run`'s, the run's swap (D176), its writable layer's head and file system (D190) and
/// its DAX windows' heads (D189), which a machine without them leaves empty. The VM's are [`Staged::handed`]; the commit
/// makes the generation durable and the one in use once the VM has filled them.
pub fn prepare(dir: &Path, run: bool) -> Result<Staged, String> {
    let names = [MEMORY, STATE, SWAP, LAYER_HEAD, LAYER, WINDOW_HEADS];
    prepare_named(
        dir,
        names.get(..if run { names.len() } else { 2 }).unwrap_or_default(),
    )
}

/// Stages an empty generation in `dir` for a VM whose snapshots its spawner stages (D196):
/// its memory and state, and what it keeps beside them (`kept`: its layer's head and file
/// system, its windows' heads), in that order, which the VM fills ([`stage`]).
pub fn prepare_kept(dir: &Path, kept: platform::Kept) -> Result<Staged, String> {
    let mut names = vec![MEMORY, STATE];
    if kept.layer {
        names.extend([LAYER_HEAD, LAYER]);
    }
    if kept.window_heads {
        names.push(WINDOW_HEADS);
    }
    prepare_named(dir, &names)
}

/// An empty generation in `dir` of the files `names`, each made sparse where it is the
/// memory or the layer's file system, as the VM that fills it may not make it (D196).
fn prepare_named(dir: &Path, names: &[&str]) -> Result<Staged, String> {
    let mut staged = staging(dir, fresh_name())?;
    let made = names
        .iter()
        .map(|&name| {
            // Its memory sparse already, as the VM that fills it may not make it (D196).
            create_unsynced(&staged.staging.join(name), |file| match name {
                MEMORY | LAYER => platform::sparse(file),
                _ => Ok(()),
            })
        })
        .collect::<io::Result<Vec<File>>>();
    match made {
        Ok(files) => {
            staged.files = files;
            Ok(staged)
        }
        Err(e) => {
            tidy(dir);
            Err(format!("{}: {e}", dir.display()))
        }
    }
}

/// Fills a generation [`prepare`] made, in the VM it is for: the state of `snap`, a paused
/// machine's, which names the generation `name`, and its memory (D176). Whoever prepared
/// it commits it.
pub fn fill(
    name: &str,
    snap: &Snapshot,
    memory: &GuestMemory,
    memory_file: &File,
    state_file: &File,
) -> Result<(), String> {
    if !is_generation(name) {
        return Err(format!("{name:?} names no generation"));
    }
    let state = state_of(snap, name)?;
    memory.save(memory_file).map_err(|e| format!("its memory: {e}"))?;
    write_whole(state_file, &state).map_err(|e| format!("its state: {e}"))
}

/// The state file of `snap` for generation `name`, with its backing files' identities as
/// they are now.
fn state_of(snap: &Snapshot, name: &str) -> Result<Vec<u8>, String> {
    let identities = backing(&snap.config)
        .map(|(path, _)| {
            path_bytes(path)?;
            Identity::of(path).map_err(|e| format!("{}: {e}", path.display()))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let state = encode(snap, name, &identities)?;
    // No more than a restore reads (D176): a share's tables, with the bytes of the files its
    // guest holds that no path reaches, are what grows it.
    if state.len() as u64 > MAX_STATE {
        return Err(format!(
            "the machine's state is {} bytes, more than a snapshot keeps ({MAX_STATE})",
            state.len()
        ));
    }
    Ok(state)
}

/// `bytes`, all of `file`, whatever its offset: one its spawner also holds shares it.
fn write_whole(file: &File, bytes: &[u8]) -> io::Result<()> {
    file.set_len(0)?;
    platform::write_all_at(file, bytes, 0)
}

/// A new generation's staging directory in `dir`, named for `name`, with `dir`'s lock
/// held: no files yet.
fn staging(dir: &Path, name: String) -> Result<Staged, String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", dir.display());
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
    Ok(Staged {
        dir: dir.to_path_buf(),
        name,
        staging,
        files: Vec::new(),
        layer_head: None,
        window_heads: None,
        committed: false,
        _lock: Some(lock),
        by: None,
    })
}

impl Staged {
    /// The generation's name, once it is in place.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The snapshot's directory, which the generation is staged in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The generation's directory, held open: it stays the generation's once the commit
    /// renames it into place.
    pub fn generation(&self) -> Result<File, String> {
        platform::open_dir(&self.staging).map_err(|e| format!("{}: {e}", self.staging.display()))
    }

    /// Its writable layer's head as [`stage`] wrote it (D190), where it keeps one: what the
    /// machine that goes on maps back ([`Layer::remap_head`]).
    ///
    /// [`Layer::remap_head`]: crate::devices::virtio::pmem::Layer::remap_head
    pub fn layer_head(&self) -> Option<&File> {
        self.layer_head.and_then(|i| self.files.get(i))
    }

    /// Its DAX windows' heads as [`stage`] wrote them (D189), where it keeps them: what the
    /// machine that goes on maps back ([`WindowHeads::remap`]).
    ///
    /// [`WindowHeads::remap`]: crate::devices::virtio::head::WindowHeads::remap
    pub fn window_heads(&self) -> Option<&File> {
        self.window_heads.and_then(|i| self.files.get(i))
    }

    /// Its files for the VM that fills them ([`prepare`]): its memory, its state, and a
    /// run's swap, layer head, layer and windows' heads, each a descriptor of its own.
    pub fn handed(&self) -> Result<Vec<File>, String> {
        self.files
            .iter()
            .map(File::try_clone)
            .collect::<io::Result<Vec<File>>>()
            .map_err(|e| format!("{}: {e}", self.staging.display()))
    }

    /// Adds a file of its spawner's own to the generation, `name` holding `bytes`: what
    /// no VM writes, made durable with the rest by the commit (D176). A name of the
    /// generation's own files is refused.
    pub fn add(&mut self, name: &str, bytes: &[u8]) -> Result<(), String> {
        if [STATE, MEMORY, SWAP, LAYER_HEAD, LAYER, WINDOW_HEADS, WORKING_SET].contains(&name)
            || name.contains(['/', '\\'])
        {
            return Err(format!("{name:?} names no file a generation's spawner adds"));
        }
        let at = self.staging.join(name);
        let file =
            create_unsynced(&at, |mut f| f.write_all(bytes)).map_err(|e| format!("{}: {e}", at.display()))?;
        self.files.push(file);
        Ok(())
    }

    /// Makes the snapshot durable and the one in use.
    pub fn commit(self) -> Result<(), String> {
        self.commit_with(&mut |_| Ok(()))
    }

    fn commit_with(mut self, after: &mut dyn FnMut(Step) -> io::Result<()>) -> Result<(), String> {
        self.committed = true;
        // Its spawner's to make durable and put in place, as it staged it, with none of its
        // files open here: NTFS moves no directory a file of which is open (PM M345).
        if let Some(by) = self.by {
            self.files.clear();
            return by
                .commit(&self.dir, &self.name)
                .map_err(|e| format!("{}: {e}", self.dir.display()));
        }
        let (dir, name, staging) = (&self.dir, &self.name, &self.staging);
        let committed = (|| -> io::Result<()> {
            for file in &self.files {
                platform::sync_durable(file)?;
            }
            // Closed once durable, before their directory moves: NTFS renames no directory a
            // file of which is open, ERROR_ACCESS_DENIED (PM M345).
            self.files.clear();
            platform::sync_dir(staging)?;
            after(Step::Staged)?;
            fs::rename(staging, dir.join(name))?;
            after(Step::Renamed)?;
            platform::sync_dir(dir)?;
            point(dir, name)?;
            after(Step::Pointed)
        })();
        tidy(dir);
        committed.map_err(|e| format!("{}: {e}", dir.display()))
    }
}

impl Drop for Staged {
    /// One dropped uncommitted, a save that failed, goes with its files, under the lock it
    /// holds until then: what it wrote is a guest's memory, which nothing will read.
    fn drop(&mut self) {
        // One its spawner staged is the spawner's to remove, as this process may not.
        if !self.committed && self.by.is_none() {
            let _ = fs::remove_dir_all(&self.staging);
        }
    }
}

/// Removes everything but what `current` names, which is whole: superseded generations,
/// pointers a newer one superseded (platform `write_in`'s versions), and what failed or
/// dead writers left. With no pointer to read it removes no generation: the one a pointer
/// lost to a crash or a fault named would go with the rest.
fn tidy(dir: &Path) {
    let keep = current(dir).ok();
    let pointer = platform::open_dir(dir)
        .and_then(|d| platform::newest_in(&d, CURRENT))
        .ok();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let entry_name = entry.file_name();
            let entry_name = entry_name.to_string_lossy();
            let generation =
                is_generation(&entry_name) && keep.as_deref().is_some_and(|keep| keep != entry_name);
            let leftover = entry_name.starts_with(".g-") || entry_name.starts_with(&format!(".{CURRENT}."));
            let superseded = platform::version_of(&entry_name) == CURRENT
                && pointer.as_deref().is_some_and(|newest| newest != entry_name);
            if generation || leftover || superseded {
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

/// The files the generation `dir` points at restores against, disks then pmem, each with
/// whether its guest only reads it, as its state names them, none of them looked at: for a
/// process that may look at no file it was not handed (Windows' AppContainer, PM M345) to
/// be handed them before [`read`] checks each is the file the snapshot was taken with.
pub fn backing_named(dir: &Path) -> Result<Vec<(PathBuf, bool)>, String> {
    let name = current(dir).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => format!("{}: no snapshot here", dir.display()),
        _ => format!("{}: {CURRENT}: {e}", dir.display()),
    })?;
    let state = dir.join(&name).join(STATE);
    let bytes = platform::read_beneath(dir, &[&name, STATE], MAX_STATE)
        .map_err(|e| format!("{}: {e}", state.display()))?
        .ok_or_else(|| format!("{}: gone", state.display()))?;
    let decoded = decode(&bytes).map_err(|e| format!("{}: {e}", state.display()))?;
    Ok(backing(&decoded.snapshot.config)
        .map(|(path, read_only)| (path.to_path_buf(), read_only))
        .collect())
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
    let bytes = platform::read_beneath(dir, &[name, STATE], MAX_STATE)
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
    let memory = platform::open_beneath_input(dir, &[name, MEMORY]).map_err(|e| lost(MEMORY, e))?;
    // The RAM, then each run of the memory plugged (D176).
    let expected = snapshot
        .plugged
        .iter()
        .try_fold(
            snapshot.config.memory_mib.saturating_mul(1 << 20),
            |sum, (_, len)| sum.checked_add(*len),
        )
        .ok_or_else(|| Open::Bad(at(STATE, &"memory plugged past what a file can hold")))?;
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
    let misaligned = pages
        .iter()
        .filter(|t| !page.is_power_of_two() || t.gpa % page != 0)
        .count();
    // Reserved as the entries were: a set the host cannot hold is refused, not an abort.
    let mut gpas: Vec<u64> = Vec::new();
    gpas.try_reserve_exact(pages.len())
        .map_err(|e| DecodeError(format!("a working set of {} pages: {e}", pages.len())))?;
    gpas.extend(pages.iter().map(|t| t.gpa));
    gpas.sort_unstable();
    let twice: Vec<u64> = gpas
        .windows(2)
        .filter_map(|w| match w {
            [a, b] if a == b => Some(*a),
            _ => None,
        })
        .collect();
    if misaligned > 0 || !twice.is_empty() {
        // Said whole, as a set refused is not kept: which of its pages broke it.
        return Err(DecodeError(format!(
            "a working set whose pages are not aligned and distinct: of {} pages, {misaligned} not on a page of {page} bytes and {} recorded twice{}",
            pages.len(),
            twice.len(),
            match (twice.first(), twice.last()) {
                (Some(first), Some(last)) => format!(", from {first:#x} to {last:#x}"),
                _ => String::new(),
            }
        )));
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
/// A generation keeps the first it is given, which every restore after maps ahead and
/// records none of (D25): the restores of a pool, made before it had one, each record one
/// and send it as their runs end, and a set sent after replaced the one before, once a
/// run's that gave memory back as it waited (PM M320). One a restore could not read is
/// replaced. Returns how many pages it wrote: 0 for a set recorded from a generation since
/// replaced, or at another page size, or for a generation that has one.
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
    let max_pages = max_pages(&pinned.snapshot);
    if matches!(read_working_set(&pinned.path, page, max_pages), Ok(Some(_))) {
        return Ok(0);
    }
    let Some(pages) = decode_working_set_bytes(bytes, page, max_pages)? else {
        return Ok(0);
    };
    let generation = platform::open_dir_beneath(dir, &[&pinned.name])
        .map_err(|e| format!("{}: {e}", pinned.path.display()))?;
    write_working_set(&generation, &pages, page)?;
    Ok(pages.len())
}

/// Whether the generation the snapshot in `dir` is at has a working set saved: one a
/// restore of it maps ahead, and records none of.
pub fn has_working_set(dir: &Path) -> bool {
    current(dir).is_ok_and(|name| platform::open_newest_beneath_input(dir, &[&name, WORKING_SET]).is_ok())
}

/// A working set as a restore hands it to its spawner (`--working-set FD`): the name of the
/// generation it was recorded from, a newline, then the working set
/// ([`encode_working_set`]); its spawner keeps it as the daemon keeps a warm VM's
/// ([`accept_working_set`]).
pub fn split_handed(handed: &[u8]) -> Result<(&str, &[u8]), String> {
    let at = handed
        .iter()
        .position(|&b| b == b'\n')
        .ok_or("a handed working set names no generation")?;
    let (name, rest) = handed.split_at(at);
    let name = std::str::from_utf8(name).map_err(|_| "a handed working set's generation is not text")?;
    Ok((name, rest.get(1..).unwrap_or_default()))
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
    let (Some(dir), Some(name)) = (
        generation.parent(),
        generation.file_name().and_then(|n| n.to_str()),
    ) else {
        return Err(at(&format!(
            "{}: not a generation's directory",
            generation.display()
        )));
    };
    let Some(bytes) =
        platform::read_newest_beneath(dir, &[name, WORKING_SET], max_bytes).map_err(|e| at(&e))?
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
                shares: 2,
                join: true,
                join_share: true,
                nested: false,
                hotplug_mib: 0,
                reporting: false,
                gpu: None,
                swap_mib: 0,
                share_window_mib: 0,
                layer_mib: 0,
            },
            plugged: Vec::new(),
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

    /// What a snapshot cannot hold is refused as it is written, not lost: a guest's EL2
    /// state. Memory to plug it holds: its size, and what was plugged (`sample`'s runs,
    /// D176), which the round trip below keeps.
    #[test]
    fn a_machine_with_state_a_snapshot_cannot_hold_is_refused() {
        let ids = vec![Identity::default(); 3];
        let mut nested = sample(&nowhere(), 1);
        nested.config.nested = true;
        let e = encode(&nested, "g-1", &ids).unwrap_err();
        assert!(e.contains("EL2 state is not saved"), "{e}");
        let mut plugging = sample(&nowhere(), 1);
        plugging.config.hotplug_mib = 2048;
        let decoded = decode(&encode(&plugging, "g-1", &ids).unwrap()).unwrap();
        assert_eq!(decoded.snapshot.config.hotplug_mib, 2048);
    }

    #[test]
    fn rejects_foreign_or_damaged_state() {
        // With runs of memory plugged, which the state keeps (D176).
        let mut snap = sample(&nowhere(), 1);
        snap.plugged = vec![(1 << 32, 2 << 20), (0x1_0040_0000, 4 << 20)];
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

    /// A state of the formats before this one, back to shards 0.6.0's, reads as it was
    /// written where its machine has no DAX windows; one with windows, laid out otherwise
    /// then, is refused, as is any older format (D189, D176).
    #[test]
    fn a_state_of_the_formats_before_reads_where_its_machine_has_no_dax_windows() {
        let ids = vec![Identity::default(); 3];
        let as_of = |snap: &Snapshot, version: u32| {
            let mut bytes = encode(snap, "g-1", &ids).unwrap();
            bytes[8..12].copy_from_slice(&version.to_le_bytes());
            bytes
        };
        for version in VERSION_WITHOUT_WINDOWS..VERSION {
            let snap = sample(&nowhere(), 1);
            assert_eq!(
                decode(&as_of(&snap, version)).unwrap().snapshot,
                snap,
                "{version}"
            );
        }
        let as_before = |snap: &Snapshot| as_of(snap, VERSION_WITHOUT_WINDOWS);
        let mut snap = sample(&nowhere(), 1);
        assert_eq!(decode(&as_before(&snap)).unwrap().snapshot, snap);
        // A length for windows, but no share to give one: none.
        snap.config.shares = 0;
        snap.config.join_share = false;
        snap.config.share_window_mib = 64;
        assert_eq!(decode(&as_before(&snap)).unwrap().snapshot, snap);
        snap.config.join_share = true;
        let e = decode(&as_before(&snap)).unwrap_err();
        assert!(e.0.contains("with DAX windows"), "{e:?}");
        assert_eq!(
            decode(&encode(&snap, "g-1", &ids).unwrap()).unwrap().snapshot,
            snap
        );
        let mut older = as_before(&sample(&nowhere(), 1));
        older[8..12].copy_from_slice(&(VERSION_WITHOUT_WINDOWS - 1).to_le_bytes());
        assert!(decode(&older).unwrap_err().0.contains("this shards reads"));
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
        // A set refused says which of its pages broke it, as it is not kept.
        let bytes = encode_working_set(
            &touches(&[0x8000_4000, 0xc000_0000, 0x8000_4000, 0x9000_0002]),
            16384,
        );
        let said = decode_working_set(&bytes, 16384, 8).unwrap_err().0;
        assert!(
            said.ends_with("of 4 pages, 1 not on a page of 16384 bytes and 1 recorded twice, from 0x80004000 to 0x80004000"),
            "{said}"
        );
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

    /// The store on disk, on every host: Windows' platform layer opens files beneath a
    /// directory and in one it holds as Unix's does (windows_files.rs).
    mod on_disk {
        use super::*;

        /// A directory of its own under the system's temporary directory, holding the
        /// files [`sample`] names, removed on drop.
        /// Its path, canonical, and the directory that goes with it.
        struct Scratch(
            PathBuf,
            #[expect(dead_code, reason = "held for its drop")] shards_testdir::TempDir,
        );

        impl Scratch {
            fn new(name: &str) -> Scratch {
                let dir = shards_testdir::TempDir::new(&format!("snap-{name}")).unwrap();
                let s = Scratch(fs::canonicalize(&dir).unwrap(), dir);
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

        /// A generation prepared for a VM that writes only what it is handed (D30, D176):
        /// filled through its files and committed, it reads as one the VM wrote, the run's
        /// swap and writable layer kept beside it; until the commit, the one before is in use.
        #[test]
        fn a_prepared_generation_filled_and_committed_reads_back() {
            let s = Scratch::new("prepared");
            let dir = s.0.join("snap");
            write(&dir, &sample(&s.0, 1), &ram(1)).unwrap();
            let mut staged = prepare(&dir, true).unwrap();
            let handed = staged.handed().unwrap();
            assert_eq!(handed.len(), 6);
            let name = staged.name().to_string();
            fill(&name, &sample(&s.0, 2), &ram(2), &handed[0], &handed[1]).unwrap();
            platform::write_all_at(&handed[2], b"swap", 0).unwrap();
            platform::write_all_at(&handed[3], b"head", 0).unwrap();
            platform::write_all_at(&handed[4], b"ext4", 0).unwrap();
            platform::write_all_at(&handed[5], b"heads", 0).unwrap();
            // And a file of the spawner's own, beside the VM's; none of theirs.
            staged.add("resume", b"tokens").unwrap();
            for theirs in [STATE, LAYER_HEAD, LAYER, WINDOW_HEADS] {
                assert!(staged.add(theirs, b"x").unwrap_err().contains("names no file"));
            }
            assert_eq!(markers(&read(&dir).unwrap()), (1, 1));
            // Its VM's files go before the commit, as a VM's go with its save: NTFS moves no
            // directory a file of which is open (PM M345).
            drop(handed);
            staged.commit().unwrap();
            let p = read(&dir).unwrap();
            assert_eq!(markers(&p), (2, 2));
            assert_eq!(p.name, name);
            assert_eq!(generation(&dir).unwrap(), p.path);
            assert_eq!(fs::read(swap_of(&p.path)).unwrap(), b"swap");
            assert_eq!(fs::read(p.path.join(LAYER_HEAD)).unwrap(), b"head");
            assert_eq!(fs::read(layer_of(&p.path)).unwrap(), b"ext4");
            assert_eq!(fs::read(p.path.join(WINDOW_HEADS)).unwrap(), b"heads");
            assert_eq!(fs::read(p.path.join("resume")).unwrap(), b"tokens");
            // Only its user's: a generation holds a guest's memory.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                for file in [MEMORY, STATE, SWAP, "resume"] {
                    let mode = fs::metadata(p.path.join(file)).unwrap().permissions().mode();
                    assert_eq!(mode & 0o777, 0o600, "{file}");
                }
            }
            assert_eq!(generations(&dir).len(), 2, "{:?}", generations(&dir));
        }

        /// A spawner that stages generations for a process that writes in no directory of
        /// its own (D196), as the shards crate's grants do over a link: each prepared and
        /// handed over, then committed on the process's word.
        #[derive(Debug, Default)]
        struct Spawner(std::sync::Mutex<Vec<Staged>>);

        impl platform::Stager for Spawner {
            fn stage(&self, dir: &Path, kept: platform::Kept) -> io::Result<(String, Vec<File>)> {
                let staged = prepare_kept(dir, kept).map_err(io::Error::other)?;
                let handed = staged.handed().map_err(io::Error::other)?;
                let name = staged.name().to_string();
                self.0.lock().unwrap().push(staged);
                Ok((name, handed))
            }

            fn data_ranges(&self, file: &File, len: u64) -> io::Result<Vec<(u64, u64)>> {
                let _ = file;
                Ok(vec![(0, len)])
            }

            fn commit(&self, _: &Path, name: &str) -> io::Result<()> {
                let mut staged = self.0.lock().unwrap();
                let at = staged
                    .iter()
                    .position(|s| s.name() == name)
                    .ok_or_else(|| io::Error::other(format!("{name}: not staged")))?;
                staged.swap_remove(at).commit().map_err(io::Error::other)
            }
        }

        /// A generation its spawner stages (D196): the process fills the two files it is
        /// handed, and its commit has the spawner put the generation in place, where it
        /// reads as one the process wrote itself; until then the one before is in use. One
        /// dropped uncommitted leaves its staging to the spawner, which removes it.
        #[test]
        fn a_generation_its_spawner_stages_reads_back_once_committed() {
            let s = Scratch::new("staged-by");
            let dir = s.0.join("snap");
            write(&dir, &sample(&s.0, 1), &ram(1)).unwrap();
            let by: &'static Spawner = Box::leak(Box::default());
            let staged = stage_by(by, &dir, &sample(&s.0, 4), &ram(4), Beside::default()).unwrap();
            assert_eq!(markers(&read(&dir).unwrap()), (1, 1));
            staged.commit().unwrap();
            let p = read(&dir).unwrap();
            assert_eq!(markers(&p), (4, 4));
            assert!(by.0.lock().unwrap().is_empty(), "committed, and so let go of");

            let dropped = stage_by(by, &dir, &sample(&s.0, 5), &ram(5), Beside::default()).unwrap();
            drop(dropped);
            assert_eq!(
                by.0.lock().unwrap().len(),
                1,
                "the spawner's staging is the spawner's"
            );
            assert_eq!(markers(&read(&dir).unwrap()), (4, 4));
            by.0.lock().unwrap().clear();
            assert_eq!(generations(&dir).len(), 2, "{:?}", generations(&dir));
        }

        /// A generation held holds what its restores map: its memory and the files its
        /// guest only reads, and not those it writes; a newer generation is not the one
        /// held, whose memory it keeps (PM M315).
        #[test]
        fn a_generation_held_holds_the_files_its_restores_map() {
            let s = Scratch::new("held");
            let dir = s.0.join("snap");
            write(&dir, &sample(&s.0, 1), &ram(1)).unwrap();
            let held = hold(&dir).unwrap();
            assert!(held.is_current(&dir));
            // Which file each is: its device and inode, or its volume and index.
            let id = |f: &File| -> (u64, u64) {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt as _;
                    let m = f.metadata().unwrap();
                    (m.dev(), m.ino())
                }
                #[cfg(windows)]
                platform::file_id(f).unwrap()
            };
            let ids: Vec<_> = held._files.iter().map(id).collect();
            let p = read(&dir).unwrap();
            assert!(ids.contains(&id(&p.memory)), "its memory");
            for (path, read_only) in backing(&p.snapshot.config) {
                let file = File::open(path).unwrap();
                assert_eq!(ids.contains(&id(&file)), read_only, "{}", path.display());
            }
            write(&dir, &sample(&s.0, 2), &ram(2)).unwrap();
            assert!(!held.is_current(&dir));
            assert!(hold(&dir).unwrap().is_current(&dir));
            // The memory it holds is still the generation's it held.
            let memory = held._files.iter().find(|f| id(f) == id(&p.memory)).unwrap();
            let mut first = [0u8; 1];
            platform::read_exact_at(memory, &mut first, 0).unwrap();
            assert_eq!(first, [1]);
        }

        /// One dropped before its commit, a save that failed, leaves nothing behind; and a
        /// fill under a name no generation has is refused.
        #[test]
        fn a_prepared_generation_dropped_leaves_nothing() {
            let s = Scratch::new("dropped");
            let dir = s.0.join("snap");
            let staged = prepare(&dir, false).unwrap();
            let handed = staged.handed().unwrap();
            assert_eq!(handed.len(), 2);
            assert_eq!(generations(&dir).len(), 1, "{:?}", generations(&dir));
            let e = fill("../g-1", &sample(&s.0, 1), &ram(1), &handed[0], &handed[1]).unwrap_err();
            assert!(e.contains("names no generation"), "{e}");
            drop(staged);
            assert_eq!(generations(&dir), Vec::<String>::new());
            assert!(!exists(&dir));
        }

        /// A memory file holds the RAM and then each run of the memory plugged (D176): one
        /// shorter than both is no generation's.
        #[test]
        fn a_memory_file_short_of_its_plugged_runs_is_refused() {
            let s = Scratch::new("plug-short");
            let dir = s.0.join("snap");
            let mut snap = sample(&s.0, 1);
            snap.plugged = vec![(1 << 32, 2 << 20)];
            write(&dir, &snap, &ram(1)).unwrap();
            let e = read(&dir).unwrap_err();
            assert!(e.contains("bytes; the snapshot's guest has"), "{e}");
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
            // One generation, and the pointer to it (a version of it on Windows, platform
            // `write_in`).
            let names = generations(&dir);
            assert_eq!(names.len(), 2, "{names:?}");
            assert!(
                names.iter().any(|n| platform::version_of(n) == CURRENT),
                "{names:?}"
            );
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
                platform::write_in(&platform::open_dir(&dir).unwrap(), CURRENT, bad.as_bytes()).unwrap();
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
            assert_eq!(stored(), Some(set.clone()));
            // Another restore's, sent after: the generation keeps its first.
            let fewer = encode_working_set(&touches(&[0x8000_4000]), page);
            assert_eq!(accept_working_set(&dir, &name, &fewer, page, |_| max), Ok(0));
            assert_eq!(stored(), Some(set.clone()));
            // One a restore cannot read is replaced.
            let generation = platform::open_dir(&read(&dir).unwrap().path).unwrap();
            platform::write_in(&generation, WORKING_SET, b"damaged").unwrap();
            assert!(read_working_set(&read(&dir).unwrap().path, page, max).is_err());
            assert_eq!(accept_working_set(&dir, &name, &fewer, page, |_| max), Ok(1));
            assert_eq!(stored(), Some(touches(&[0x8000_4000])));
        }

        /// A working set a restore hands its spawner is kept in the generation it names,
        /// and in no other; a newer generation starts without one.
        #[test]
        fn a_handed_working_set_is_kept_in_its_generation_and_no_other() {
            let s = Scratch::new("handed");
            let dir = s.0.join("snap");
            write(&dir, &sample(&s.0, 1), &ram(1)).unwrap();
            let name = read(&dir).unwrap().name;
            let (page, max) = (0x4000, 64);
            let set = touches(&[0x8000_0000, 0x8000_4000]);
            let handed = |name: &str| [name.as_bytes(), b"\n", &encode_working_set(&set, page)].concat();
            let keep = |h: &[u8]| {
                let (name, bytes) = split_handed(h)?;
                accept_working_set(&dir, name, bytes, page, |_| max)
            };
            assert!(!has_working_set(&dir));
            // Naming no generation, or another one: nothing kept.
            assert!(split_handed(b"g-0001").is_err());
            assert!(split_handed(b"\xff\n").is_err());
            assert_eq!(keep(&handed("g-not-this")), Ok(0));
            assert!(!has_working_set(&dir));
            // Its own generation's: kept there, and read back as handed.
            assert_eq!(keep(&handed(&name)), Ok(2));
            assert!(has_working_set(&dir));
            assert_eq!(
                read_working_set(&read(&dir).unwrap().path, page, max).unwrap(),
                Some(set.clone())
            );
            // A newer generation has none, and the older one's is not taken for it.
            write(&dir, &sample(&s.0, 2), &ram(2)).unwrap();
            assert!(!has_working_set(&dir));
            assert_eq!(keep(&handed(&name)), Ok(0));
            assert!(!has_working_set(&dir));
        }

        #[test]
        fn working_sets_land_in_their_generation() {
            let pages = touches(&[0x8000_4000]);
            let s = Scratch::new("wset");
            let dir = s.0.join("snap");
            let written = write(&dir, &sample(&s.0, 1), &ram(1)).unwrap();
            let p = read(&dir).unwrap();
            assert_eq!(read_working_set(&p.path, 16384, 8).unwrap(), None, "none saved");
            // Moved while its generation is held, on Unix: NTFS moves no directory a file of
            // which is open (PM M345), so on Windows it stays where it is.
            let moved = if cfg!(windows) {
                dir.clone()
            } else {
                s.0.join("moved")
            };
            #[cfg(unix)]
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
            // Each name once, a set's versions Windows would not yet let go of among them
            // (platform `write_in`, PM M345), which go with their generation.
            let mut names: Vec<String> = generations(&dir.join(current(&dir).unwrap()))
                .iter()
                .map(|n| platform::version_of(n).to_string())
                .collect();
            names.dedup();
            assert_eq!(names, [MEMORY, STATE, WORKING_SET], "no temporary files are left");
        }
    }
}
