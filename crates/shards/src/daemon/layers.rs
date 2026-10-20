//! Each run's writable layer (D190): an ext4 file system in a file of its own, which its
//! microVM maps as a pmem device and its guest mounts with DAX under its root's overlay.
//!
//! A microVM booted for a run is given one made empty here, of the run's size; one
//! restored from a template is given an empty file, which it fills with a copy of the
//! layer its snapshot kept, as it reads the snapshot, so that the two are always one
//! generation's. Either is made in the home's `layers` and unlinked once the microVM is
//! ready, holding it open and mapped: what the run wrote goes with the microVM, as its
//! swap does (D159), and nothing is left of it whatever ends the daemon or the microVM.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Where layers are made: in the home, beside the files they are copied from, so that a
/// copy may share their blocks.
const LAYERS: &str = "layers";

/// A layer's file is whole multiples of this: a pmem device's (vmm `pmem::ALIGN`).
const STEP: u64 = 2 << 20;

/// A microVM's writable layer's file, made for it, until it holds it: unlinked as this
/// goes, which the daemon lets go of once the microVM is ready or has failed to start.
#[derive(Debug)]
pub(super) struct LayerFile(PathBuf);

impl LayerFile {
    /// An empty file, which a restoring microVM fills with its snapshot's layer.
    pub(super) fn empty(home: &Path) -> Result<LayerFile, String> {
        let path = fresh(home)?;
        create(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(LayerFile(path))
    }

    /// A file holding an empty ext4 file system that holds `holds` bytes, for a microVM
    /// that boots.
    pub(super) fn made(home: &Path, holds: u64) -> Result<LayerFile, String> {
        let bytes = size(holds)?;
        let path = fresh(home)?;
        let layer = LayerFile(path);
        let at = |e: &dyn std::fmt::Display| format!("{}: {e}", layer.0.display());
        let mut file = create(&layer.0).map_err(|e| at(&e))?;
        file.set_len(bytes).map_err(|e| at(&e))?;
        let mut ids = [0u8; 32];
        shards_vmm::platform::fill_random(&mut ids).map_err(|e| at(&e))?;
        let (mut uuid, mut hash_seed) = ([0u8; 16], [0u8; 16]);
        uuid.copy_from_slice(ids.get(..16).unwrap_or_default());
        hash_seed.copy_from_slice(ids.get(16..).unwrap_or_default());
        // A random UUID's version and variant (RFC 9562 §5.4), as mke2fs gives one.
        if let Some(version) = uuid.get_mut(6) {
            *version = (*version & 0x0F) | 0x40;
        }
        if let Some(variant) = uuid.get_mut(8) {
            *variant = (*variant & 0x3F) | 0x80;
        }
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let options = shards_ext4::Options {
            bytes,
            uuid,
            hash_seed,
            time,
        };
        shards_ext4::format(&mut file, &options).map_err(|e| at(&e))?;
        Ok(layer)
    }

    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for LayerFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The bytes of a writable layer's file that holds `holds` bytes: its file system's,
/// whole pmem steps.
pub(super) fn size(holds: u64) -> Result<u64, String> {
    shards_ext4::size_for(holds, STEP).map_err(|e| e.to_string())
}

/// What the layers directory held of microVMs before this daemon: nothing any of them
/// still needs by name, as each held its own unlinked.
pub(super) fn clear(home: &Path) {
    let dir = home.join(LAYERS);
    for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let _ = std::fs::remove_file(entry.path());
    }
}

/// A name in the home's layers no other file has had.
fn fresh(home: &Path) -> Result<PathBuf, String> {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let dir = home.join(LAYERS);
    shards_vmm::platform::create_private_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let n = COUNT.fetch_add(1, Ordering::Relaxed);
    Ok(dir.join(format!("{}-{n}", std::process::id())))
}

/// A new file at `path`, read-write, its user's alone.
fn create(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A layer made for a boot is an ext4 file system that holds what it was asked to, and
    /// goes as its handle does; an empty one for a restore is empty.
    #[test]
    fn a_layer_is_made_and_goes_with_its_handle() {
        let home = shards_testdir::TempDir::new("layers").unwrap();
        let made = LayerFile::made(&home, 64 << 20).unwrap();
        let path = made.path().to_path_buf();
        let len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(len, size(64 << 20).unwrap());
        assert_eq!(len % STEP, 0);
        let mut magic = [0u8; 2];
        shards_vmm::platform::read_exact_at(&std::fs::File::open(&path).unwrap(), &mut magic, 1024 + 0x38)
            .unwrap();
        assert_eq!(u16::from_le_bytes(magic), 0xEF53);
        drop(made);
        assert!(!path.exists());

        let empty = LayerFile::empty(&home).unwrap();
        assert_eq!(std::fs::metadata(empty.path()).unwrap().len(), 0);
        // What a daemon before left is cleared.
        std::fs::write(home.join(LAYERS).join("1-0"), b"").unwrap();
        clear(&home);
        assert!(!home.join(LAYERS).join("1-0").exists() && !empty.path().exists());
    }
}
