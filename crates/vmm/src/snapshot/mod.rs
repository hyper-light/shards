//! VM snapshots on disk. A snapshot is a directory holding two files:
//!
//! - `state`: a versioned header, the machine configuration restore rebuilds, the
//!   architecture's CPU and interrupt-controller state, and the devices' state
//! - `memory`: guest RAM, one region after another, with all-zero pages as holes
//!
//! and, once a VM resumed from the snapshot has recorded one, a third:
//!
//! - `working-set`: the guest pages that VM touched after the snapshot, which restores
//!   ahead of their request prefetch (PM M30)
//!
//! Each file is written under a temporary name, synced, then renamed, so the directory
//! holds a complete snapshot or none.

pub mod codec;

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use codec::{DecodeError, Reader, Writer};

use crate::hv::Touch;
use crate::memory::GuestMemory;

pub const STATE: &str = "state";
pub const MEMORY: &str = "memory";
pub const WORKING_SET: &str = "working-set";
const WORKING_SET_MAGIC: [u8; 8] = *b"SHRDWSET";
const WORKING_SET_VERSION: u32 = 1;
/// A working set lists at most every page of 1 TiB.
const MAX_WORKING_SET: usize = 1 << 26;
const MAGIC: [u8; 8] = *b"SHRDSNAP";
/// 2: MachineConfig records whether the machine has a vsock device.
/// 3: and its virtio-pmem files.
/// 4: virtio devices' own state follows their queues' (vsock: the streams a restore resets).
const VERSION: u32 = 4;
/// The snapshot format this build writes and reads: what a snapshot kept for reuse is
/// keyed by.
pub const FORMAT: u32 = VERSION;
/// A state file is kilobytes; anything past this is not one of ours.
const MAX_STATE: u64 = 64 << 20;
const MAX_DISKS: usize = 64;
const MAX_PATH: usize = 4096;

/// What restore needs to rebuild the machine the snapshot came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineConfig {
    pub vcpus: u32,
    pub memory_mib: u64,
    /// virtio-blk disks by path, in guest order. A restore reopens the same files, which
    /// must hold what the guest's page cache expects (as with Firecracker).
    pub disks: Vec<(PathBuf, bool)>,
    /// Read-only virtio-pmem files, in guest order, after the disks. A restore maps the
    /// same files, which must be unchanged.
    pub pmem: Vec<PathBuf>,
    /// Whether a vsock device follows the pmem devices. Its host socket path is not
    /// recorded: a restored VM needs a path of its own.
    pub vsock: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub config: MachineConfig,
    /// The architecture's machine state, as its machine module encodes it.
    pub arch: Vec<u8>,
    /// The device bus's state.
    pub devices: Vec<u8>,
}

fn encode(s: &Snapshot) -> Vec<u8> {
    let mut w = Writer::default();
    MAGIC.iter().for_each(|&b| w.u8(b));
    w.u32(VERSION);
    w.bytes(std::env::consts::ARCH.as_bytes());
    w.u32(s.config.vcpus);
    w.u64(s.config.memory_mib);
    w.seq(&s.config.disks, |w, (path, ro)| {
        w.bytes(path.to_string_lossy().as_bytes());
        w.bool(*ro);
    });
    w.seq(&s.config.pmem, |w, path| {
        w.bytes(path.to_string_lossy().as_bytes())
    });
    w.bool(s.config.vsock);
    w.bytes(&s.arch);
    w.bytes(&s.devices);
    w.into_bytes()
}

fn decode(bytes: &[u8]) -> codec::Result<Snapshot> {
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
    let vcpus = r.u32()?;
    let memory_mib = r.u64()?;
    let disks = r.seq(MAX_DISKS, |r| {
        let path = std::str::from_utf8(r.bytes(MAX_PATH)?)
            .map_err(|_| DecodeError("disk path is not UTF-8".into()))?;
        Ok((PathBuf::from(path), r.bool()?))
    })?;
    let pmem = r.seq(MAX_DISKS, |r| {
        let path = std::str::from_utf8(r.bytes(MAX_PATH)?)
            .map_err(|_| DecodeError("pmem path is not UTF-8".into()))?;
        Ok(PathBuf::from(path))
    })?;
    let vsock = r.bool()?;
    let arch_state = r.bytes(usize::MAX)?.to_vec();
    let devices = r.bytes(usize::MAX)?.to_vec();
    r.finish()?;
    Ok(Snapshot {
        config: MachineConfig {
            vcpus,
            memory_mib,
            disks,
            pmem,
            vsock,
        },
        arch: arch_state,
        devices,
    })
}

/// Creates `dir/name` durably: written to a temporary sibling, synced, renamed.
fn write_durably(
    dir: &Path,
    name: &str,
    fill: impl FnOnce(&File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{name}.tmp"));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    fill(&file)?;
    crate::platform::sync_durable(&file)?;
    drop(file);
    std::fs::rename(&tmp, dir.join(name))
}

/// Writes a snapshot of a paused VM into `dir`, creating it if needed.
pub fn write(dir: &Path, snap: &Snapshot, memory: &GuestMemory) -> Result<(), String> {
    let at = |e: std::io::Error| format!("{}: {e}", dir.display());
    std::fs::create_dir_all(dir).map_err(at)?;
    write_durably(dir, MEMORY, |f| memory.save(f)).map_err(at)?;
    let state = encode(snap);
    write_durably(dir, STATE, |mut f| f.write_all(&state)).map_err(at)?;
    // The renames are durable once the directory itself is.
    #[cfg(unix)]
    File::open(dir).and_then(|d| d.sync_all()).map_err(at)?;
    Ok(())
}

/// Reads the snapshot in `dir`; returns it with its memory file, open for mapping.
pub fn read(dir: &Path) -> Result<(Snapshot, File), String> {
    let at = |name: &str, e: &dyn std::fmt::Display| format!("{}: {e}", dir.join(name).display());
    let state = File::open(dir.join(STATE)).map_err(|e| at(STATE, &e))?;
    let len = state.metadata().map_err(|e| at(STATE, &e))?.len();
    if len > MAX_STATE {
        return Err(at(STATE, &format!("{len} bytes is not a snapshot state")));
    }
    let mut bytes = vec![0u8; len as usize];
    crate::platform::read_exact_at(&state, &mut bytes, 0).map_err(|e| at(STATE, &e))?;
    let snap = decode(&bytes).map_err(|e| at(STATE, &e))?;
    let memory = File::open(dir.join(MEMORY)).map_err(|e| at(MEMORY, &e))?;
    let expected = snap.config.memory_mib.saturating_mul(1 << 20);
    let actual = memory.metadata().map_err(|e| at(MEMORY, &e))?.len();
    if actual != expected {
        return Err(at(
            MEMORY,
            &format!("{actual} bytes; the snapshot's guest has {expected}"),
        ));
    }
    Ok((snap, memory))
}

fn encode_working_set(pages: &[Touch], page: u64) -> Vec<u8> {
    let mut w = Writer::default();
    WORKING_SET_MAGIC.iter().for_each(|&b| w.u8(b));
    w.u32(WORKING_SET_VERSION);
    w.bytes(std::env::consts::ARCH.as_bytes());
    w.u64(page);
    w.seq(pages, |w, t| w.u64(t.gpa | u64::from(t.written)));
    w.into_bytes()
}

/// A working set recorded at stage-2 pages of `page` bytes; `None` for one recorded at
/// another page size.
fn decode_working_set(bytes: &[u8], page: u64) -> codec::Result<Option<Vec<Touch>>> {
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
    let pages = r.seq(MAX_WORKING_SET, |r| {
        let entry = r.u64()?;
        Ok(Touch {
            gpa: entry & !1,
            written: entry & 1 != 0,
        })
    })?;
    r.finish()?;
    Ok((recorded == page).then_some(pages))
}

/// Saves `pages`, recorded at stage-2 pages of `page` bytes, as the working set of the
/// snapshot in the directory `dir` holds open. The directory may have been renamed since.
pub fn write_working_set(dir: &File, pages: &[Touch], page: u64) -> Result<(), String> {
    crate::platform::write_in(dir, WORKING_SET, &encode_working_set(pages, page))
        .map_err(|e| format!("the working set: {e}"))
}

/// The working set saved with the snapshot in `dir`, if it has one recorded at stage-2
/// pages of `page` bytes.
pub fn read_working_set(dir: &Path, page: u64) -> Result<Option<Vec<Touch>>, String> {
    let path = dir.join(WORKING_SET);
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(at(&e)),
    };
    decode_working_set(&bytes, page).map_err(|e| at(&e))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        Snapshot {
            config: MachineConfig {
                vcpus: 4,
                memory_mib: 1,
                disks: vec![
                    (PathBuf::from("/data/a.img"), true),
                    (PathBuf::from("b.img"), false),
                ],
                pmem: vec![PathBuf::from("/images/base.erofs")],
                vsock: true,
            },
            arch: vec![1, 2, 3],
            devices: vec![9; 100],
        }
    }

    #[test]
    fn round_trips_on_disk() {
        let dir = std::env::temp_dir().join(format!("shards-snap-{}", std::process::id()));
        let ranges = [(0x8000_0000u64, 1usize << 20)];
        let mem = GuestMemory::anonymous(&ranges).unwrap();
        mem.write(0x8000_0010, b"guest").unwrap();
        write(&dir, &sample(), &mem).unwrap();
        let (snap, file) = read(&dir).unwrap();
        assert_eq!(snap, sample());
        let restored = GuestMemory::from_file(&ranges, &file).unwrap();
        let mut buf = [0u8; 5];
        restored.read(0x8000_0010, &mut buf).unwrap();
        assert_eq!(&buf, b"guest");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn working_sets_round_trip_and_reject_damage() {
        let pages = [
            Touch {
                gpa: 0x8000_4000,
                written: true,
            },
            Touch {
                gpa: 0xc000_0000,
                written: false,
            },
        ];
        let good = encode_working_set(&pages, 16384);
        assert_eq!(decode_working_set(&good, 16384).unwrap().unwrap(), pages);
        assert_eq!(
            decode_working_set(&good, 4096).unwrap(),
            None,
            "recorded at another page size"
        );
        for cut in 0..good.len() {
            assert!(decode_working_set(&good[..cut], 16384).is_err());
        }
        let mut bad_magic = good.clone();
        bad_magic[0] ^= 1;
        assert!(decode_working_set(&bad_magic, 16384).is_err());
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(decode_working_set(&trailing, 16384).is_err());
    }

    /// Where snapshots are written (unix): through the directory held open, wherever the
    /// directory has gone.
    #[cfg(unix)]
    #[test]
    fn working_sets_land_where_their_snapshot_went() {
        let pages = [Touch {
            gpa: 0x8000_4000,
            written: true,
        }];
        let dir = std::env::temp_dir().join(format!("shards-wset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(read_working_set(&dir, 16384).unwrap(), None, "none saved");
        let held = File::open(&dir).unwrap();
        let moved = dir.with_extension("moved");
        std::fs::rename(&dir, &moved).unwrap();
        write_working_set(&held, &pages, 16384).unwrap();
        assert_eq!(
            read_working_set(&moved, 16384).unwrap().unwrap(),
            pages,
            "written where the directory went"
        );
        let _ = std::fs::remove_dir_all(moved);
    }

    #[test]
    fn rejects_foreign_or_damaged_state() {
        let good = encode(&sample());
        assert_eq!(decode(&good).unwrap(), sample());
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
    }
}
