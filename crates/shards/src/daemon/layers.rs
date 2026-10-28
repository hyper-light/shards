//! Each run's writable layer (D190): an ext4 file system in a file of its own, which its
//! microVM maps as a pmem device and its guest mounts with DAX under its root's overlay.
//!
//! A microVM booted for a run is given one made empty here, of the run's size; one
//! restored from a snapshot (a template's, or a run's own, D176) is given a copy-on-write
//! clone of the layer the snapshot's generation kept, where the home's file system clones
//! files, else an empty file, which it fills with a copy of that layer as it reads the
//! snapshot, so that the two are always one generation's. Either is made in the home's
//! `layers`, and stays there until the run it
//! serves ends: a container that stays keeps it in its directory once its microVM has
//! gone ([`LayerFile::keep_at`]), its guest having written its journal in place, and every
//! other goes with its handle, its microVM holding it open and mapped meanwhile, as a
//! run's swap does (D159).
//! What a daemon before left in `layers` is no run's by the next.
//!
//! A kept layer is read here, on the host ([`pack`]): what `commit` takes of a stopped
//! container, and what a container's next run, or a visit to its files, puts back.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use shards_ext4::journal;
use shards_ext4::read::{self, Kind, Volume, Walker};

/// Where layers are made: in the home, beside the files they are copied from, so that a
/// copy may share their blocks.
const LAYERS: &str = "layers";

/// A layer's file is whole multiples of this: a pmem device's (vmm `pmem::ALIGN`).
const STEP: u64 = 2 << 20;

/// A microVM's writable layer's file, made for it, until it holds it: unlinked as this
/// goes, which the daemon lets go of once the microVM is ready or has failed to start.
#[derive(Debug)]
pub(super) struct LayerFile {
    path: PathBuf,
    /// The snapshot generation whose layer it is a clone of, for a restore of it.
    of: Option<String>,
}

/// What of `from` holds data, copied into a new sparse file at `to` of its length: a
/// restore's layer, where Windows makes no clone of the generation's.
#[cfg(windows)]
fn copied(from: &Path, to: &Path) -> std::io::Result<()> {
    let source = std::fs::File::open(from)?;
    let len = source.metadata()?.len();
    let target = create(to)?;
    shards_vmm::platform::sparse(&target)?;
    shards_vmm::platform::copy_used(&source, &target, len)
}

impl LayerFile {
    /// An empty file, which a restoring microVM fills with its snapshot's layer.
    pub(super) fn empty(home: &Path) -> Result<LayerFile, String> {
        let path = fresh(home)?;
        create(&path)
            .and_then(|f| shards_vmm::platform::sparse(&f))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(LayerFile { path, of: None })
    }

    /// The file a restore of the snapshot generation at `generation` is given for its
    /// layer: a copy-on-write clone of the generation's own where the home's file system
    /// makes one (APFS; btrfs and XFS on Linux), which shares its blocks until its run
    /// writes them, so that the restore copies nothing and on macOS writes nothing back
    /// (PM M278); else an [`empty`](Self::empty) one. A generation replaced before the
    /// microVM reads it is no matter: the microVM fills the file from the generation it
    /// resumes unless that is the one it is a clone of ([`cloned_of`](Self::cloned_of)).
    pub(super) fn for_restore(home: &Path, generation: &Path) -> Result<LayerFile, String> {
        let name = generation.file_name().and_then(|n| n.to_str());
        if let Some(name) = name {
            let path = fresh(home)?;
            let from = generation.join(shards_vmm::snapshot::LAYER);
            let cloned = std::fs::File::open(&from).and_then(|f| shards_build::host::clone_file(&f, &path));
            match cloned {
                Ok(true) => {
                    let layer = LayerFile {
                        path,
                        of: Some(name.to_string()),
                    };
                    // Its user's alone, as every layer's file is.
                    #[cfg(unix)]
                    std::fs::set_permissions(
                        &layer.path,
                        std::os::unix::fs::PermissionsExt::from_mode(0o600),
                    )
                    .map_err(|e| format!("{}: {e}", layer.path.display()))?;
                    return Ok(layer);
                }
                // No clone on this host's file system (NTFS): a copy of what holds data in the
                // generation's, made here, where the file system says what that is (a
                // confined VM may ask it nothing, D141), so that the restore copies nothing,
                // its file sparse as the generation's is.
                #[cfg(windows)]
                Ok(false) => match copied(&from, &path) {
                    Ok(()) => {
                        return Ok(LayerFile {
                            path,
                            of: Some(name.to_string()),
                        });
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&path);
                        super::log(format!(
                            "{}: copying it for a restore: {e}; the restore copies it",
                            from.display()
                        ));
                    }
                },
                #[cfg(not(windows))]
                Ok(false) => {}
                // Replaced meanwhile: the microVM reads the generation that replaced it.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => super::log(format!(
                    "{}: cloning it for a restore: {e}; the restore copies it",
                    from.display()
                )),
            }
        }
        Self::empty(home)
    }

    /// The generation whose layer this file is a clone of, if it is one.
    pub(super) fn cloned_of(&self) -> Option<&str> {
        self.of.as_deref()
    }

    /// A file holding an empty ext4 file system that holds `holds` bytes, for a microVM
    /// that boots.
    pub(super) fn made(home: &Path, holds: u64) -> Result<LayerFile, String> {
        let bytes = size(holds)?;
        let path = fresh(home)?;
        let layer = LayerFile { path, of: None };
        let at = |e: &dyn std::fmt::Display| format!("{}: {e}", layer.path.display());
        let mut file = create(&layer.path).map_err(|e| at(&e))?;
        // Sparse, as a Unix file system makes a file a length set leaves unwritten: the
        // blocks its file system writes alone are held, and a copy of it reads them alone
        // (`platform::data_ranges`).
        shards_vmm::platform::sparse(&file).map_err(|e| at(&e))?;
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
        &self.path
    }

    /// Keeps the file as `at`, a container's, in place of what was there: no longer the
    /// handle's to remove.
    pub(super) fn keep_at(mut self, at: &Path) -> std::io::Result<()> {
        std::fs::rename(&self.path, at)?;
        self.path = PathBuf::new();
        Ok(())
    }
}

impl Drop for LayerFile {
    fn drop(&mut self) {
        // None, once kept.
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Where a container keeps its writable layer once its microVM has gone (D190), in its
/// directory, beside its log.
pub(super) const KEPT: &str = "layer.ext4";

/// Opens the writable layer kept at `at` for reading, its microVM gone. Its guest wrote
/// its journal in place as its run ended (init's `pmem::checkpoint_layer`), and it is read
/// as it stands; where its microVM went first (killed), or its guest's kernel only
/// committed it, its journal is replayed in place first (shards_ext4::journal, as its
/// kernel replays one as it mounts it): what its guest committed is in place, and what it
/// never committed stays out.
pub(super) fn open_kept(at: &Path) -> Result<std::fs::File, String> {
    let file = std::fs::File::open(at).map_err(|e| format!("{}: {e}", at.display()))?;
    if !journal::needs_replay(&file).map_err(|e| e.to_string())? {
        return Ok(file);
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(at)
        .map_err(|e| format!("{}: {e}", at.display()))?;
    let done = journal::replay(&file).map_err(|e| format!("replaying its journal: {e}"))?;
    if let Some(done) = done {
        super::log(format!(
            "{}: replayed {} transactions of its journal, {} blocks, {} revoked",
            at.display(),
            done.transactions,
            done.blocks,
            done.revoked
        ));
    }
    Ok(file)
}

/// Writes the writable layer kept in `file` (D190) to `out` as an OCI layer: its overlay's
/// upper directory, whiteouts as `.wh.` entries, without what init made of every
/// container (shards_abi::run::MADE) and the directories its guest found held nothing
/// else (run::MADE_ONLY): the archive its guest's own pack of it makes (init's layer.rs).
pub(super) fn pack(file: std::fs::File, out: impl Write) -> Result<(), String> {
    let volume = Volume::open(file).map_err(|e| e.to_string())?;
    let mut walker = Walker::new(volume);
    let mut exclude: Vec<Vec<u8>> = shards_abi::run::MADE
        .iter()
        .map(|m| m.as_bytes().to_vec())
        .collect();
    exclude.extend(made_only(&mut walker)?);
    let opts = shards_archive::PackOptions {
        exclude_patterns: exclude,
        whiteout: shards_archive::WhiteoutFormat::Overlay,
        ..Default::default()
    };
    shards_archive::pack_from(&mut Files(walker), b"/upper", &opts, out)
        .map(drop)
        .map_err(|e| e.to_string())
}

/// [`pack`] of the layer kept in `file` into a file of its own, unlinked from `dir`, where
/// it was made, as soon as it is: what a joiner's run (D119) puts back, its join disk
/// taking the archive's whole 512-byte sectors, as a tar archive's are, where a pipe has
/// none to take.
pub(super) fn pack_to_file(file: std::fs::File, dir: &Path) -> Result<std::fs::File, String> {
    use std::io::{Seek as _, SeekFrom};
    let at = dir.join("layer.in");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut out = options.open(&at).map_err(|e| format!("{}: {e}", at.display()))?;
    let _ = std::fs::remove_file(&at);
    pack(file, &mut out)?;
    out.seek(SeekFrom::Start(0))
        .map_err(|e| format!("{}: {e}", at.display()))?;
    Ok(out)
}

/// The directories its guest found held nothing but init's files, as the image has them
/// (run::MADE_ONLY): of those, only the directories of [`MADE`](shards_abi::run::MADE)'s
/// files, whatever the file says, as the guest wrote it.
fn made_only(walker: &mut Walker<std::fs::File>) -> Result<Vec<Vec<u8>>, String> {
    let at = format!("/{}", shards_abi::run::MADE_ONLY);
    let Some(listed) = walker.lookup(at.as_bytes()).map_err(|e| e.to_string())? else {
        return Ok(Vec::new());
    };
    if listed.kind != Kind::File || listed.size > 4096 {
        return Ok(Vec::new());
    }
    let mut text = vec![0u8; listed.size as usize];
    let n = walker
        .volume()
        .read_at(&listed, 0, &mut text)
        .map_err(|e| e.to_string())?;
    text.truncate(n);
    let dirs: Vec<&[u8]> = shards_abi::run::MADE
        .iter()
        .filter_map(|m| m.rsplit_once('/').map(|(d, _)| d.as_bytes()))
        .collect();
    Ok(text
        .split(|&c| c == b'\n')
        .filter(|d| dirs.contains(d))
        .map(<[u8]>::to_vec)
        .collect())
}

/// A kept layer's files, as a pack reads them ([`shards_archive::Files`]): through
/// shards-ext4, its guest's bytes checked as they are read.
struct Files(Walker<std::fs::File>);

fn not_there(path: &[u8]) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("{}: no such file", String::from_utf8_lossy(path)),
    )
}

impl Files {
    fn inode(&mut self, path: &[u8]) -> std::io::Result<read::Inode> {
        self.0
            .lookup(path)
            .map_err(|e| std::io::Error::other(e.0))?
            .ok_or_else(|| not_there(path))
    }
}

impl shards_archive::Files for Files {
    fn lstat(&mut self, path: &[u8]) -> std::io::Result<shards_archive::Stat> {
        use shards_archive::FileKind;
        let inode = self.inode(path)?;
        let (major, minor) = match inode.kind {
            Kind::Char | Kind::Block => inode.device(),
            _ => (0, 0),
        };
        Ok(shards_archive::Stat {
            kind: match inode.kind {
                Kind::File => FileKind::File,
                Kind::Dir => FileKind::Dir,
                Kind::Symlink => FileKind::Symlink,
                Kind::Char => FileKind::Char,
                Kind::Block => FileKind::Block,
                Kind::Fifo => FileKind::Fifo,
                Kind::Socket => FileKind::Socket,
            },
            mode: u32::from(inode.mode),
            uid: inode.uid,
            gid: inode.gid,
            size: inode.size,
            mtime: shards_archive::tar::Time::unix(inode.mtime.sec, i64::from(inode.mtime.nsec)),
            ino: u64::from(inode.ino),
            dev: 0,
            nlink: u64::from(inode.links),
            major: i64::from(major),
            minor: i64::from(minor),
        })
    }

    fn read_dir(&mut self, path: &[u8]) -> std::io::Result<Vec<(Vec<u8>, bool)>> {
        self.0.list(path).map_err(|e| std::io::Error::other(e.0))
    }

    fn readlink(&mut self, path: &[u8]) -> std::io::Result<Vec<u8>> {
        let inode = self.inode(path)?;
        self.0
            .volume()
            .read_link(&inode)
            .map_err(|e| std::io::Error::other(e.0))
    }

    fn xattr(&mut self, path: &[u8], name: &[u8]) -> std::io::Result<Option<Vec<u8>>> {
        let inode = self.inode(path)?;
        let all = self
            .0
            .volume()
            .xattrs(&inode)
            .map_err(|e| std::io::Error::other(e.0))?;
        Ok(all.into_iter().find(|(n, _)| n == name).map(|(_, v)| v))
    }

    fn open(&mut self, path: &[u8]) -> std::io::Result<Box<dyn std::io::Read + '_>> {
        let inode = self.inode(path)?;
        let reader = self
            .0
            .volume()
            .reader(&inode)
            .map_err(|e| std::io::Error::other(e.0))?;
        Ok(Box::new(reader))
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

/// A new file at `path`, read-write, its user's alone, sparse: a run's layer holds 10 GiB
/// unless it asks for another size (`run::LAYER_HOLDS`), of which it takes what its guest
/// writes (PM M349).
fn create(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(path)?;
    shards_vmm::platform::make_sparse(&file)?;
    Ok(file)
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

    /// A run's layer takes of its disk what its file system wrote, not what it holds: the
    /// 10 GiB a run's holds by default took all 10 GiB of an NTFS disk's free space before
    /// its file was sparse (PM M349), and eight runs at once filled a test host.
    #[test]
    fn a_layer_takes_of_its_disk_what_is_written() {
        let home = shards_testdir::TempDir::new("layers-sparse").unwrap();
        let made = LayerFile::made(&home, crate::run::LAYER_HOLDS).unwrap();
        let meta = std::fs::metadata(made.path()).unwrap();
        let on_disk = crate::os::on_disk(made.path(), &meta);
        assert!(
            on_disk < meta.len() / 64,
            "{on_disk} of {} bytes on disk",
            meta.len()
        );
    }

    /// A restore's file is a clone of its generation's layer, said to be that generation's,
    /// where the home's file system clones files (always on macOS's APFS), else empty; one
    /// for a generation gone is empty; and a file kept stays as its handle goes.
    #[test]
    fn a_restores_layer_is_its_generations_clone_where_files_clone() {
        let home = shards_testdir::TempDir::new("layers-clone").unwrap();
        let generation = home.join("template").join("g-3");
        std::fs::create_dir_all(&generation).unwrap();
        let bytes: Vec<u8> = (0..4u32 << 20).map(|i| (i % 251) as u8).collect();
        std::fs::write(generation.join(shards_vmm::snapshot::LAYER), &bytes).unwrap();
        let given = LayerFile::for_restore(&home, &generation).unwrap();
        match given.cloned_of() {
            Some(of) => {
                assert_eq!(of, "g-3");
                assert_eq!(std::fs::read(given.path()).unwrap(), bytes);
            }
            None => assert_eq!(std::fs::metadata(given.path()).unwrap().len(), 0),
        }
        #[cfg(target_os = "macos")]
        assert!(given.cloned_of().is_some());
        let gone = LayerFile::for_restore(&home, &home.join("template").join("g-4")).unwrap();
        assert_eq!(gone.cloned_of(), None);
        assert_eq!(std::fs::metadata(gone.path()).unwrap().len(), 0);
        let (path, kept) = (given.path().to_path_buf(), home.join("kept.ext4"));
        given.keep_at(&kept).unwrap();
        assert!(kept.exists() && !path.exists());
    }
}
