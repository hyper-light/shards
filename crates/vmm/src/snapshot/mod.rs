//! VM snapshots on disk. A snapshot is a directory holding two files:
//!
//! - `state`: a versioned header, the machine configuration restore rebuilds, the
//!   architecture's CPU and interrupt-controller state, and the devices' state
//! - `memory`: guest RAM, one region after another, with all-zero pages as holes
//!
//! Each file is written under a temporary name, synced, then renamed, so the directory
//! holds a complete snapshot or none.

pub mod codec;

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use codec::{DecodeError, Reader, Writer};

use crate::memory::GuestMemory;

pub const STATE: &str = "state";
pub const MEMORY: &str = "memory";
const MAGIC: [u8; 8] = *b"SHRDSNAP";
/// 2: MachineConfig records whether the machine has a vsock device.
/// 3: and its virtio-pmem files.
const VERSION: u32 = 3;
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
