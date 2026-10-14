//! Cache mounts kept across builds (docs/design/architecture.md D114, D134): each record a
//! cache's content, as layers of EROFS images, the lowest first, taken by a step as
//! BuildKit's cache mount manager takes a cache directory (dockerfile/1.27.1
//! solver/llbsolver/mounts/mount.go `getRefCacheDir`): by its key, shared, private or
//! locked, among every build of the store, whichever process runs it.
//!
//! A record is `cachemounts/v2/records/<id>/`: `record.json`; `lock`, whose lock (flock(2)
//! where there is one) holds it for a step, shared by `sharing=shared`'s takers and
//! exclusively by `private`'s and `locked`'s; and `layers/`, its images, each `<n>.erofs`:
//! a step's changes as overlayfs left them in its upper directory, which the builder
//! writes (D134), its first content (`from=`), or layers merged. What a step writes goes to
//! a `.part` file there first, and is named once kept. A key's records are looked through,
//! made and written under the lock of `cachemounts/v2/keys/<SHA-256 of the key>`, as
//! BuildKit looks through and makes them under its `cacheRefsLocker` of the key. What is no
//! step and looks whether one holds a record (a listing, a collection's sweep, a removal)
//! looks under that lock too, so that its own moment's hold on the record passes for no
//! step's, to a step looking for one or to another look ([`Store::looked_at`]).
//!
//! A record keeps few layers, and few bytes past what it shows ([`plan`]): what its steps'
//! changes hide below them goes as layers are merged, which a step that keeps changes does
//! as it lets the record go, while it still holds it.

use std::fs::{self, File, TryLockError};
use std::io::{self, BufWriter, Write as _};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use super::{Partial, Store, sync_dir, sync_durable, unix_now};
use crate::{Error, bad};

pub(super) const VERSION: u32 = 2;

/// How a step shares a cache with others' steps (pb.CacheSharingOpt).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sharing {
    /// With any other step that shares it.
    Shared,
    /// With none: a record no step holds, or a new one.
    Private,
    /// With none: a record no step holds, waited for while every one is held.
    Locked,
}

/// A layer of a cache's content: an EROFS image in its record's `layers/`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MountLayer {
    /// Its file's name there.
    pub name: String,
    /// Its bytes.
    pub size: u64,
    /// The bytes of the regular files below it that it hides, each a whole number of
    /// blocks: what merging it into them would free.
    pub hidden: u64,
}

/// A cache's root directory, which no layer records: its mode, owner and group. A new
/// one's is an empty snapshot's, `0755` and root's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MountRoot {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Default for MountRoot {
    fn default() -> MountRoot {
        MountRoot {
            mode: 0o755,
            uid: 0,
            gid: 0,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct MountRecord {
    key: String,
    layers: Vec<MountLayer>,
    root: MountRoot,
    /// BuildKit's description of it: `cached mount DEST from exec ARGS`.
    description: String,
    created: i64,
    last_used: i64,
    usage: u64,
    /// Let go of while a step held it ([`Store::prune_mounts`]): no step takes it again.
    pruned: bool,
    /// The number the next layer's file is named by: none is named twice.
    next: u64,
}

/// A record a step holds, until it is dropped: its id, and its content as it was taken,
/// each layer's file open, so that a merge meanwhile, which removes the files of the
/// layers it merged, never takes what the step reads.
#[derive(Debug)]
pub struct HeldMount {
    pub id: String,
    pub layers: Vec<MountLayer>,
    files: Vec<File>,
    pub root: MountRoot,
    /// Whether it was made for this step: none of the key's was free.
    pub fresh: bool,
    _lock: File,
}

impl HeldMount {
    /// Each layer, the lowest first, and its file, open for reading.
    pub fn layer_files(&self) -> impl Iterator<Item = (&MountLayer, &File)> {
        self.layers.iter().zip(&self.files)
    }
}

/// A run of a record's layers to merge ([`plan`]): whether it starts at the lowest, its
/// layers' names, and their files, each with the bytes below it that it hides.
struct Run {
    bottom: bool,
    names: Vec<String>,
    files: Vec<(File, u64)>,
}

/// A file of a record's that a step's changes are written to ([`Store::mount_part`]), until
/// they are kept: removed if they are not.
#[derive(Debug)]
pub struct MountPart {
    path: PathBuf,
    file: File,
    /// Whether it is a layer now, under another name.
    kept: bool,
}

impl MountPart {
    /// The file, open for reading and writing.
    pub fn file(&self) -> &File {
        &self.file
    }
}

impl Drop for MountPart {
    fn drop(&mut self) {
        if !self.kept {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// A step's changes to a cache, written to a [`MountPart`]: an image of `len` bytes from
/// its start, which hides `hidden` bytes of the record's regular files.
#[derive(Debug)]
pub struct MountChanges {
    pub part: MountPart,
    pub len: u64,
    pub hidden: u64,
}

/// A record as `system df` and `builder prune` see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub id: String,
    pub description: String,
    /// The bytes of its layers: what removing it frees. Its first content, where it is
    /// from something, is among them: it is a copy of its own, where BuildKit's is its
    /// source's snapshot, shared.
    pub size: u64,
    pub created: i64,
    pub last_used: i64,
    pub usage: u64,
    /// Whether a step holds it now.
    pub in_use: bool,
}

/// How often a `locked` taker looks again while every record of its key is held, as
/// BuildKit's does (mount.go, 100 ms).
const LOOK_AGAIN: Duration = Duration::from_millis(100);

/// What a record's `record.json` is renamed to as it goes ([`Store::remove_mount`]).
const GONE: &str = "gone";

/// Whether a lock was got, or another holds the file.
fn try_lock(file: &File, shared: bool) -> Result<bool, Error> {
    let tried = if shared {
        file.try_lock_shared()
    } else {
        file.try_lock()
    };
    match tried {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

fn open_lock(path: &Path) -> Result<File, Error> {
    Ok(File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?)
}

/// A listed record's lock, opened; none where the record has gone since its listing
/// ([`Store::remove_mount`]).
fn listed_lock(path: &Path) -> Result<Option<File>, Error> {
    match File::options().write(true).open(path) {
        Ok(lock) => Ok(Some(lock)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// A record's id: lowercase letters and digits, as BuildKit's identity.NewID makes them,
/// so that it is one name of the store's own.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

impl Store {
    pub(super) fn mounts_dir(&self) -> PathBuf {
        self.root.join(format!("cachemounts/v{VERSION}"))
    }

    fn records_dir(&self) -> PathBuf {
        self.mounts_dir().join("records")
    }

    fn mount_dir(&self, id: &str) -> Result<PathBuf, Error> {
        if !valid_id(id) {
            return bad(format!("{id}: not a cache mount's id"));
        }
        Ok(self.records_dir().join(id))
    }

    /// Where record `id`'s layers are.
    fn layers_dir(&self, id: &str) -> Result<PathBuf, Error> {
        Ok(self.mount_dir(id)?.join("layers"))
    }

    /// The lock of `key`'s records, held while they are looked through, made or written.
    fn key_lock(&self, key: &str) -> Result<File, Error> {
        let hash: String = Sha256::digest(key.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let lock = open_lock(&self.mounts_dir().join("keys").join(hash))?;
        lock.lock()?;
        Ok(lock)
    }

    /// Record `id` as it is with its key's lock held, that lock, and the record's lock
    /// file, open and not yet taken: how what is no step (a listing, a collection's sweep,
    /// a removal) looks whether a step holds a record. A step looks through a key's
    /// records and takes one under the key's lock ([`Store::take_mount`]), and lets go of
    /// none under it: so the look's own hold on the record, a moment's, passes for a step
    /// neither to a step looking for one nor to another look.
    fn looked_at(&self, id: &str) -> Result<Option<(MountRecord, File, File)>, Error> {
        let Some(record) = self.read_mount(id)? else {
            return Ok(None);
        };
        let keys = self.key_lock(&record.key)?;
        // As it is now: it is written under that lock alone.
        let Some(record) = self.read_mount(id)? else {
            return Ok(None);
        };
        let Some(lock) = listed_lock(&self.mount_dir(id)?.join("lock"))? else {
            return Ok(None);
        };
        Ok(Some((record, keys, lock)))
    }

    fn read_mount(&self, id: &str) -> Result<Option<MountRecord>, Error> {
        match fs::read(self.mount_dir(id)?.join("record.json")) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn write_mount(&self, id: &str, record: &MountRecord) -> Result<(), Error> {
        let bytes = serde_json::to_vec(record).map_err(|e| Error(e.to_string()))?;
        let dir = self.mount_dir(id)?;
        // Written through `ingest/`, which a collection empties: none runs meanwhile.
        let _lease = self.lease()?;
        let mut partial = Partial::create(&self.root.join("ingest"))?;
        partial.write_all(&bytes)?;
        partial.replace(&dir.join("record.json"))?;
        sync_dir(&dir)
    }

    /// Every record's id, in the order BuildKit's metadata index keeps them: by id.
    fn mount_ids(&self) -> Result<Vec<String>, Error> {
        let mut ids = Vec::new();
        for entry in fs::read_dir(self.records_dir())? {
            let name = entry?.file_name();
            if let Some(name) = name.to_str()
                && valid_id(name)
            {
                ids.push(name.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Takes a record of `key` for a step, as BuildKit's `getRefCacheDir` takes one:
    /// - `Shared`: the one others share now, else the first no step holds, else a new one;
    /// - `Private`: the first no step holds, else a new one;
    /// - `Locked`: the first no step holds, waited for while each is held, else a new one.
    ///
    /// A new record is `new_id`, described as `description`, and empty until its first
    /// content is set ([`Store::seed_mount`]). A record one of whose layers' files is gone is
    /// taken no more, as one let go of is not ([`Store::prune_mounts`]).
    pub fn take_mount(
        &self,
        key: &str,
        sharing: Sharing,
        description: &str,
        new_id: &str,
    ) -> Result<HeldMount, Error> {
        let shared = sharing == Sharing::Shared;
        let mut keys = self.key_lock(key)?;
        loop {
            let mut held_any = false;
            let mut free: Vec<(String, File)> = Vec::new();
            for id in self.mount_ids()? {
                if self.read_mount(&id)?.is_none_or(|r| r.key != key || r.pruned) {
                    continue;
                }
                let Some(lock) = listed_lock(&self.mount_dir(&id)?.join("lock"))? else {
                    continue;
                };
                if try_lock(&lock, false)? {
                    // Free. A shared taker first looks for the one others share.
                    if !shared {
                        if let Some(held) = self.held_mount(&id, lock)? {
                            return Ok(held);
                        }
                        continue;
                    }
                    lock.unlock()?;
                    free.push((id, lock));
                } else {
                    held_any = true;
                    if shared
                        && try_lock(&lock, true)?
                        && let Some(held) = self.held_mount(&id, lock)?
                    {
                        return Ok(held);
                    }
                }
            }
            for (id, lock) in free {
                if try_lock(&lock, true)?
                    && let Some(held) = self.held_mount(&id, lock)?
                {
                    return Ok(held);
                }
                held_any = true;
            }
            if sharing == Sharing::Locked && held_any {
                drop(keys);
                std::thread::sleep(LOOK_AGAIN);
                keys = self.key_lock(key)?;
                continue;
            }
            break;
        }
        // None to take: a new one, held before the key is let go.
        let dir = self.mount_dir(new_id)?;
        fs::create_dir(&dir)?;
        let lock = open_lock(&dir.join("lock"))?;
        if shared {
            lock.lock_shared()?;
        } else {
            lock.lock()?;
        }
        let now = unix_now();
        self.write_mount(
            new_id,
            &MountRecord {
                key: key.to_string(),
                layers: Vec::new(),
                root: MountRoot::default(),
                description: description.to_string(),
                created: now,
                last_used: now,
                usage: 0,
                pruned: false,
                next: 0,
            },
        )?;
        sync_dir(&self.records_dir())?;
        drop(keys);
        Ok(HeldMount {
            id: new_id.to_string(),
            layers: Vec::new(),
            files: Vec::new(),
            root: MountRoot::default(),
            fresh: true,
            _lock: lock,
        })
    }

    /// `lock`, held, as record `id`'s, with what it holds now, each layer's file opened;
    /// none where it went between its listing and its lock ([`Store::remove_mount`]), or
    /// where a layer's file is gone, which leaves it let go of.
    fn held_mount(&self, id: &str, lock: File) -> Result<Option<HeldMount>, Error> {
        let Some(mut record) = self.read_mount(id)? else {
            return Ok(None);
        };
        let dir = self.layers_dir(id)?;
        let mut files = Vec::with_capacity(record.layers.len());
        for l in &record.layers {
            match File::open(dir.join(&l.name)) {
                Ok(f) => files.push(f),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    record.pruned = true;
                    self.write_mount(id, &record)?;
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(Some(HeldMount {
            id: id.to_string(),
            layers: record.layers,
            files,
            root: record.root,
            fresh: false,
            _lock: lock,
        }))
    }

    /// The most bytes a step's changes to a cache may take: what the store's limits let an
    /// image hold, within what its filesystem has free past what is to be left free (audit
    /// A10). A whole number of blocks, one at least.
    pub fn mount_room(&self, limits: &super::Limits) -> Result<u64, Error> {
        let free = (limits.available)(&self.root)?;
        let room = limits.bytes.min(free.saturating_sub(limits.keep_free));
        Ok((room - room % crate::erofs::BLOCK).max(crate::erofs::BLOCK))
    }

    /// A file for a step's changes to `held`, of `len` bytes, none of them written yet:
    /// `<name>.part` among its layers, which are named by lowercase letters and digits.
    pub fn mount_part(&self, held: &HeldMount, name: &str, len: u64) -> Result<MountPart, Error> {
        if !valid_id(name) {
            return bad(format!("{name}: not a part's name"));
        }
        let dir = self.layers_dir(&held.id)?;
        fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{name}.part"));
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        let part = MountPart {
            path,
            file,
            kept: false,
        };
        part.file.set_len(len)?;
        Ok(part)
    }

    /// Sets the first content of `held`, a fresh record of `key`: `image`, an image of
    /// what its mount is from, and that one's root.
    pub fn seed_mount(
        &self,
        key: &str,
        held: &mut HeldMount,
        image: MountChanges,
        root: MountRoot,
    ) -> Result<(), Error> {
        let (part, len) = (image.part, image.len);
        finish(&part, len)?;
        let _keys = self.key_lock(key)?;
        let Some(mut record) = self.read_mount(&held.id)? else {
            return bad(format!("cache mount {} is gone", held.id));
        };
        if !record.layers.is_empty() {
            return bad(format!("cache mount {} has content already", held.id));
        }
        let layer = self.keep_part(&held.id, &mut record, part, len, 0)?;
        record.layers.push(layer.clone());
        record.root = root;
        self.write_mount(&held.id, &record)?;
        held.files = vec![File::open(self.layers_dir(&held.id)?.join(&layer.name))?];
        held.layers = vec![layer];
        held.root = root;
        Ok(())
    }

    /// Counts a step's use of `held`, a record of `key`: its changes, if any, the record's
    /// next layer, and `root` its root from now on. Then merges its layers as [`plan`]
    /// says, while the step still holds it.
    pub fn used_mount(
        &self,
        key: &str,
        held: &HeldMount,
        changes: Option<MountChanges>,
        root: MountRoot,
    ) -> Result<(), Error> {
        // Its layers are as the last merge left them, unless a layer was kept.
        if self.keep_layer(key, held, changes, root)? {
            self.merge_mount(key, &held.id)?;
        }
        Ok(())
    }

    /// [`Store::used_mount`]'s count, its merges aside: whether a layer was kept.
    fn keep_layer(
        &self,
        key: &str,
        held: &HeldMount,
        changes: Option<MountChanges>,
        root: MountRoot,
    ) -> Result<bool, Error> {
        let changes = changes.filter(|c| c.len > 0);
        if let Some(c) = &changes {
            finish(&c.part, c.len)?;
        }
        let kept = changes.is_some();
        let _keys = self.key_lock(key)?;
        let Some(mut record) = self.read_mount(&held.id)? else {
            return bad(format!("cache mount {} is gone", held.id));
        };
        if let Some(c) = changes {
            let layer = self.keep_part(&held.id, &mut record, c.part, c.len, c.hidden)?;
            record.layers.push(layer);
        }
        record.root = root;
        record.last_used = unix_now();
        record.usage = record.usage.saturating_add(1);
        self.write_mount(&held.id, &record)?;
        Ok(kept)
    }

    /// Names `part`, `len` bytes of an image, record `id`'s next layer, which hides
    /// `hidden` bytes: called with the key's lock held, and `record` written after.
    fn keep_part(
        &self,
        id: &str,
        record: &mut MountRecord,
        mut part: MountPart,
        len: u64,
        hidden: u64,
    ) -> Result<MountLayer, Error> {
        let name = format!("{}.erofs", record.next);
        let dir = self.layers_dir(id)?;
        fs::rename(&part.path, dir.join(&name))?;
        part.kept = true;
        sync_dir(&dir)?;
        record.next = record.next.saturating_add(1);
        Ok(MountLayer {
            name,
            size: len,
            hidden,
        })
    }

    /// Merges record `id`'s layers as [`plan`] says until it says no more: each run planned
    /// with the key's lock held and its files opened, merged without it, and put in the
    /// run's place under it again if no other merge took the run meanwhile.
    fn merge_mount(&self, key: &str, id: &str) -> Result<(), Error> {
        while let Some(run) = self.plan_merge(key, id)? {
            let (part, merged) = self.merge_run(id, &run)?;
            // Another merge took the run: this one's goes, and the record is planned again.
            self.commit_merge(key, id, &run, part, merged)?;
        }
        Ok(())
    }

    /// The run of record `id`'s layers to merge next, if any ([`plan`]), its files opened
    /// with the key's lock held, so that a merge that removes them meanwhile takes nothing
    /// this one reads.
    fn plan_merge(&self, key: &str, id: &str) -> Result<Option<Run>, Error> {
        let _keys = self.key_lock(key)?;
        let Some(record) = self.read_mount(id)? else {
            return Ok(None);
        };
        let Some(planned) = plan(&record.layers) else {
            return Ok(None);
        };
        let dir = self.layers_dir(id)?;
        let layers = record.layers.get(planned.clone()).unwrap_or_default();
        let mut files = Vec::with_capacity(layers.len());
        for l in layers {
            files.push((File::open(dir.join(&l.name))?, l.hidden));
        }
        Ok(Some(Run {
            bottom: *planned.start() == 0,
            names: layers.iter().map(|l| l.name.clone()).collect(),
            files,
        }))
    }

    /// `run` merged into a file of record `id`'s, and what it came to.
    fn merge_run(&self, id: &str, run: &Run) -> Result<(MountPart, super::merge::Merged), Error> {
        let part = self.merge_part(id)?;
        let merged = {
            let mut out = BufWriter::new(&part.file);
            let mut parts = vec!["merge", id];
            parts.extend(run.names.iter().map(String::as_str));
            let merged = super::merge::merge(&run.files, run.bottom, layer_uuid(&parts), &mut out)?;
            out.flush()?;
            merged
        };
        finish(&part, merged.size)?;
        Ok((part, merged))
    }

    /// Puts `part`, `run` merged, in the run's place in record `id`, unless another merge
    /// took the run meanwhile: whether it did. Layers are kept above a run and merged only
    /// by a merge, so if its first is where it was planned and the rest follow it, it is
    /// whole. The run's files go once the record no longer names them: a step that holds
    /// them open still reads them.
    fn commit_merge(
        &self,
        key: &str,
        id: &str,
        run: &Run,
        part: MountPart,
        merged: super::merge::Merged,
    ) -> Result<bool, Error> {
        let _keys = self.key_lock(key)?;
        let Some(mut record) = self.read_mount(id)? else {
            return Ok(false);
        };
        let at = record
            .layers
            .iter()
            .position(|l| run.names.first() == Some(&l.name))
            .filter(|&at| {
                (!run.bottom || at == 0)
                    && record
                        .layers
                        .get(at..at + run.names.len())
                        .is_some_and(|r| r.iter().map(|l| &l.name).eq(run.names.iter()))
            });
        let Some(at) = at else {
            return Ok(false);
        };
        let layer = self.keep_part(id, &mut record, part, merged.size, merged.hidden)?;
        record.layers.splice(at..at + run.names.len(), [layer]);
        self.write_mount(id, &record)?;
        let dir = self.layers_dir(id)?;
        for name in &run.names {
            let _ = fs::remove_file(dir.join(name));
        }
        Ok(true)
    }

    /// A file for a merge of record `id`'s layers: `.merge-<process>-<n>.part`, which a
    /// collection removes once no step holds the record, should the merge not finish.
    fn merge_part(&self, id: &str) -> Result<MountPart, Error> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = self
            .layers_dir(id)?
            .join(format!(".merge-{}-{n}.part", std::process::id()));
        // One a crashed process of this one's id left is no one's.
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        Ok(MountPart {
            path,
            file,
            kept: false,
        })
    }

    /// Every record, in use or not: in use where a step holds it ([`Store::looked_at`]).
    pub fn mount_entries(&self) -> Result<Vec<MountEntry>, Error> {
        let mut out = Vec::new();
        for id in self.mount_ids()? {
            let Some((record, keys, lock)) = self.looked_at(&id)? else {
                continue;
            };
            let in_use = !try_lock(&lock, false)?;
            drop((lock, keys));
            out.push(MountEntry {
                description: record.description,
                size: record.layers.iter().fold(0u64, |n, l| n.saturating_add(l.size)),
                created: record.created,
                last_used: record.last_used,
                usage: record.usage,
                in_use,
                id,
            });
        }
        Ok(out)
    }

    /// Removes record `id` unless a step holds it: whether it went, its layers with it.
    pub fn remove_mount(&self, id: &str) -> Result<bool, Error> {
        let dir = self.mount_dir(id)?;
        let Some((_, keys, lock)) = self.looked_at(id)? else {
            return Ok(false);
        };
        if !try_lock(&lock, false)? {
            return Ok(false);
        }
        // Gone first, while its lock is held: a step that locks it after this finds no
        // record there ([`Store::held_mount`]), and nothing lists it.
        match fs::rename(dir.join("record.json"), dir.join(GONE)) {
            Ok(()) => sync_dir(&dir)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        }
        drop((lock, keys));
        // Then its directory, once its lock is let go of: Windows moves no directory with a
        // file open in it.
        self.take_away(id)?;
        Ok(true)
    }

    /// Gone record `id`'s directory, moved aside and removed; left, gone, where a file in it
    /// is open (Windows refuses the move, ERROR_ACCESS_DENIED), as a step that listed it
    /// before it went holds its lock open until it finds no record there, for the next
    /// collection ([`Store::sweep_mounts`]).
    fn take_away(&self, id: &str) -> Result<(), Error> {
        let aside = self.records_dir().join(format!(".{id}-{}", std::process::id()));
        match fs::rename(self.mount_dir(id)?, &aside) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound
                ) =>
            {
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        fs::remove_dir_all(&aside)?;
        sync_dir(&self.records_dir())
    }

    /// What removals and steps left, taken away as a collection starts: records gone that a
    /// step held open as they went, what a removal that stopped had moved aside, and in a
    /// record no step holds, files no layer of it is: a step's changes it never kept, a
    /// merge that never finished, layers a merge left.
    pub(super) fn sweep_mounts(&self) -> Result<(), Error> {
        let records = self.records_dir();
        let entries = match fs::read_dir(&records) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                // Its removal's, which another may be finishing: left to it where it is.
                let _ = fs::remove_dir_all(entry.path());
            } else if valid_id(name) && entry.path().join(GONE).exists() {
                self.take_away(name)?;
            } else if valid_id(name) {
                self.sweep_layers(name)?;
            }
        }
        Ok(())
    }

    /// The files in record `id`'s `layers/` that no layer of it is, removed unless a step
    /// holds it.
    fn sweep_layers(&self, id: &str) -> Result<(), Error> {
        let Some((record, _keys, lock)) = self.looked_at(id)? else {
            return Ok(());
        };
        if !try_lock(&lock, false)? {
            return Ok(());
        }
        let entries = match fs::read_dir(self.layers_dir(id)?) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            if !record
                .layers
                .iter()
                .any(|l| name.to_str() == Some(l.name.as_str()))
            {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    /// Lets go of the records of cache `id` before a build whose step mounting it is not to
    /// be answered from the cache (`--no-cache`, `--no-cache-filter`), as BuildKit's solver
    /// does (llbsolver `detectPrunedCacheID`, the worker's `PruneCacheMounts`): those of a
    /// cache mounted from something (`id:` and its source's key) where `from`, else those
    /// of `id` alone. One no step holds goes; one a step holds stays its until it is done,
    /// and no step takes it again.
    pub fn prune_mounts(&self, id: &str, from: bool) -> Result<(), Error> {
        let prefix = format!("{id}:");
        let ids = self.mount_ids()?;
        for rid in ids {
            let Some(record) = self.read_mount(&rid)? else {
                continue;
            };
            let named = if from {
                record.key.starts_with(&prefix)
            } else {
                record.key == id
            };
            if !named || record.pruned || self.remove_mount(&rid)? {
                continue;
            }
            let _keys = self.key_lock(&record.key)?;
            if let Some(mut record) = self.read_mount(&rid)? {
                record.pruned = true;
                self.write_mount(&rid, &record)?;
            }
        }
        Ok(())
    }
}

/// A layer's volume UUID, from what names it: unique among a record's layers, as overlayfs
/// needs of the lower layers it follows copied-up files into (D134), and the same for the
/// same layer made twice, of which one is kept.
pub fn layer_uuid(parts: &[&str]) -> [u8; 16] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    let digest = h.finalize();
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(digest.get(..16).unwrap_or(&[0; 16]));
    uuid
}

/// `len` bytes of `part`'s image kept, the rest of the file let go, and made durable before
/// any record names it: a crash never leaves a record naming a torn image.
fn finish(part: &MountPart, len: u64) -> Result<(), Error> {
    part.file.set_len(len)?;
    sync_durable(&part.file)?;
    Ok(())
}

/// The run of `layers` to merge next, if any, so that a record keeps few layers and few
/// bytes past what it shows:
///
/// - Each layer weighs more than twice the one above it, its weight its bytes and those it
///   hides: as the logarithmic method keeps a set of sorted runs (Bentley and Saxe,
///   "Decomposable searching problems I. Static-to-dynamic transformation", J. Algorithms
///   1(4), 1980), the topmost layer that weighs at least half the one below it is merged
///   into it, and the two into the next below while they weigh at least half of it. A
///   record of n bytes then has at most about log2(n / 4 KiB) layers, a step mounts them all,
///   and each byte is merged into another layer at most as many times.
/// - What its layers hide is less than half its bytes, as a copying collector holds at most
///   twice what it keeps (Cheney, "A nonrecursive list compacting algorithm", CACM 13(11),
///   1970): past it, every layer is merged into the lowest, and what is hidden goes. That
///   merge copies what is shown, no more than what is hidden: each byte hidden was written
///   once, and is gone after.
fn plan(layers: &[MountLayer]) -> Option<RangeInclusive<usize>> {
    let top = layers.len().checked_sub(1).filter(|&t| t > 0)?;
    let size = layers.iter().fold(0u64, |n, l| n.saturating_add(l.size));
    let hidden = layers
        .iter()
        .skip(1)
        .fold(0u64, |n, l| n.saturating_add(l.hidden));
    if hidden.saturating_mul(2) >= size {
        return Some(0..=top);
    }
    let weight = |i: usize| layers.get(i).map_or(0, |l| l.size.saturating_add(l.hidden));
    let violated = (1..=top)
        .rev()
        .find(|&i| weight(i).saturating_mul(2) >= weight(i - 1))?;
    let mut from = violated - 1;
    let mut acc = weight(violated).saturating_add(weight(from));
    while from > 0 && acc.saturating_mul(2) >= weight(from - 1) {
        from -= 1;
        acc = acc.saturating_add(weight(from));
    }
    Some(from..=violated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::erofs;

    /// A store of its own, removed when dropped.
    fn store() -> (shards_testdir::TempDir, Store) {
        let dir = shards_testdir::TempDir::new("mounts").unwrap();
        let store = Store::open(&dir).unwrap();
        (dir, store)
    }

    fn meta(mode: u16) -> erofs::Meta {
        erofs::Meta {
            mode,
            mtime: 1_700_000_000,
            ..erofs::Meta::default()
        }
    }

    /// A layer's tree: each file named and filled with its byte, `n` of them; a name with
    /// no byte a whiteout.
    fn tree(files: &[(&str, Option<u8>, usize)]) -> (erofs::Tree, Vec<u8>) {
        let mut t = erofs::Tree::new(meta(0o755));
        let mut bytes = Vec::new();
        for &(name, fill, n) in files {
            let kind = match fill {
                Some(b) => {
                    let offset = bytes.len() as u64;
                    bytes.extend(std::iter::repeat_n(b, n));
                    erofs::Kind::File {
                        size: n as u64,
                        data: erofs::DataRef { source: 0, offset },
                    }
                }
                None => erofs::Kind::CharDevice { major: 0, minor: 0 },
            };
            t.insert(
                erofs::Tree::ROOT,
                name.as_bytes(),
                erofs::Node {
                    kind,
                    meta: meta(0o644),
                },
            )
            .unwrap();
        }
        (t, bytes)
    }

    struct Bytes(Vec<u8>);

    impl erofs::Source for Bytes {
        fn read_at(&mut self, data: erofs::DataRef, at: u64, buf: &mut [u8]) -> io::Result<()> {
            let from = (data.offset + at) as usize;
            buf.copy_from_slice(&self.0[from..from + buf.len()]);
            Ok(())
        }
    }

    /// A step's changes to `held`: an image of `files`, which hides `hidden` bytes, written
    /// into a part of 1 MiB, as the builder writes one.
    fn changes(
        s: &Store,
        held: &HeldMount,
        name: &str,
        files: &[(&str, Option<u8>, usize)],
        hidden: u64,
    ) -> MountChanges {
        let part = s.mount_part(held, name, 1 << 20).unwrap();
        let (t, bytes) = tree(files);
        let mut out = part.file();
        let written = erofs::write(&t, &mut Bytes(bytes), &mut out).unwrap();
        MountChanges {
            part,
            len: written.blocks * erofs::BLOCK,
            hidden,
        }
    }

    /// What a held record shows: each name's bytes, its layers stacked as overlayfs stacks
    /// them, read from the files it holds open.
    fn shown(held: &HeldMount) -> Vec<(String, Vec<u8>)> {
        let mut shown = std::collections::BTreeMap::new();
        for (_, file) in held.layer_files() {
            let t = erofs::read_tree(&mut &*file, 0).unwrap();
            for (name, id) in t.entries(erofs::Tree::ROOT) {
                let name = String::from_utf8_lossy(name).into_owned();
                match &t.node(id).unwrap().kind {
                    erofs::Kind::File { size, data } => {
                        let mut b = vec![0; *size as usize];
                        crate::layer::read_exact_at(file, &mut b, data.offset).unwrap();
                        shown.insert(name, b);
                    }
                    _ => {
                        shown.remove(&name);
                    }
                }
            }
        }
        shown.into_iter().collect()
    }

    /// mount.go's choices, step by step: what a key's takers get while others hold it.
    #[test]
    fn records_are_taken_as_buildkit_takes_cache_directories() {
        let (_d, s) = store();
        let take = |sharing, id: &str| s.take_mount("/k", sharing, "cached mount /c", id).unwrap();
        // None yet: a new one.
        let a = take(Sharing::Shared, "a");
        assert!(a.fresh);
        // Another shared taker joins it.
        let b = take(Sharing::Shared, "b");
        assert_eq!((b.id.as_str(), b.fresh), ("a", false));
        // A private one finds it held: a new one.
        let p = take(Sharing::Private, "p");
        assert_eq!((p.id.as_str(), p.fresh), ("p", true));
        drop((a, b));
        // Free again: a private taker has `a`, the first by id.
        let q = take(Sharing::Private, "q");
        assert_eq!(q.id, "a");
        // `a` and `p` held privately: a shared taker gets a new one.
        let r = take(Sharing::Shared, "r");
        assert_eq!((r.id.as_str(), r.fresh), ("r", true));
        // Another key's records are not this one's.
        let other = s.take_mount("/other", Sharing::Private, "x", "o").unwrap();
        assert!(other.fresh);
        drop((p, q, r, other));
        // A shared taker prefers the one shared now over a free one before it.
        let first = take(Sharing::Private, "z1");
        assert_eq!(first.id, "a");
        let sharing = take(Sharing::Shared, "z2");
        assert_eq!(sharing.id, "p");
        drop(first);
        let joined = take(Sharing::Shared, "z3");
        assert_eq!(joined.id, "p");
    }

    /// What looks whether a step holds a record looks under the record's key's lock, as a
    /// step takes a key's records (a listing, as a collection's sweep is looking; a sweep,
    /// as a step is taking one): a listing never counts a look's moment as a step's hold,
    /// nor a step a sweep's, which would make it a new record.
    #[test]
    fn a_look_at_a_record_passes_for_no_step() {
        let (_d, s) = store();
        drop(s.take_mount("/k", Sharing::Private, "c", "a").unwrap());
        // A look as a sweep makes one: the key's lock, then the record's.
        let (_, keys, lock) = s.looked_at("a").unwrap().unwrap();
        assert!(try_lock(&lock, false).unwrap());
        std::thread::scope(|scope| {
            let listing = scope.spawn(|| s.mount_entries().unwrap());
            std::thread::sleep(Duration::from_millis(300));
            assert!(!listing.is_finished(), "listed during the look");
            drop((lock, keys));
            let entries = listing.join().unwrap();
            assert_eq!(entries.len(), 1);
            assert!(!entries[0].in_use, "the look counted as a step's");
        });
        // A sweep waits for a step's taking, which holds the key's lock.
        let taking = s.key_lock("/k").unwrap();
        std::thread::scope(|scope| {
            let sweep = scope.spawn(|| s.sweep_mounts().unwrap());
            std::thread::sleep(Duration::from_millis(300));
            assert!(!sweep.is_finished(), "swept during a taking");
            drop(taking);
            sweep.join().unwrap();
        });
        let again = s.take_mount("/k", Sharing::Private, "c", "b").unwrap();
        assert_eq!((again.id.as_str(), again.fresh), ("a", false));
    }

    /// A locked taker waits while every record is held, and takes the first let go.
    #[test]
    fn a_locked_taker_waits_for_a_record() {
        let (_d, s) = store();
        let held = s.take_mount("/k", Sharing::Locked, "c", "a").unwrap();
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| s.take_mount("/k", Sharing::Locked, "c", "b").unwrap());
            std::thread::sleep(Duration::from_millis(300));
            assert!(!waiter.is_finished());
            drop(held);
            let got = waiter.join().unwrap();
            assert_eq!((got.id.as_str(), got.fresh), ("a", false));
        });
    }

    #[test]
    fn records_keep_their_content_and_go_unless_held() {
        let (_d, s) = store();
        let mut held = s
            .take_mount("/k", Sharing::Shared, "cached mount /c", "a")
            .unwrap();
        assert_eq!(held.root, MountRoot::default());
        let seeded = MountRoot {
            mode: 0o700,
            uid: 1000,
            gid: 1000,
        };
        let seed = changes(&s, &held, "s", &[("x", Some(b'x'), 100_000)], 0);
        s.seed_mount("/k", &mut held, seed, seeded).unwrap();
        // A change of less than half its bytes: no merge.
        let next = changes(&s, &held, "c1", &[("y", Some(b'y'), 3)], 0);
        s.used_mount("/k", &held, Some(next), seeded).unwrap();
        // None, and the root changed.
        let chowned = MountRoot { uid: 7, ..seeded };
        s.used_mount("/k", &held, None, chowned).unwrap();
        // Changes of no bytes: none kept, their file gone.
        let empty = changes(&s, &held, "c2", &[], 0);
        let empty = MountChanges { len: 0, ..empty };
        s.used_mount("/k", &held, Some(empty), chowned).unwrap();
        let entries = s.mount_entries().unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        let again = |id| s.take_mount("/k", Sharing::Shared, "c", id).unwrap();
        let other = again("b");
        assert_eq!(other.id, "a", "shared with the step that holds it");
        let size: u64 = other.layers.iter().map(|l| l.size).sum();
        assert_eq!((e.size, e.usage, e.in_use), (size, 3, true));
        assert_eq!(other.layers.len(), 2);
        assert_eq!(other.root, chowned);
        assert_eq!(
            shown(&other),
            vec![("x".into(), vec![b'x'; 100_000]), ("y".into(), b"yyy".to_vec())]
        );
        let mut names: Vec<_> = fs::read_dir(s.layers_dir("a").unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["0.erofs", "1.erofs"]);
        assert!(!s.remove_mount("a").unwrap());
        drop((held, other));
        assert!(s.remove_mount("a").unwrap());
        assert!(s.mount_entries().unwrap().is_empty());
        assert!(!s.remove_mount("a").unwrap());
        assert!(s.remove_mount("../x").is_err());
        assert!(s.layers_dir("a").is_ok_and(|d| !d.exists()));
    }

    /// The run to merge: the topmost layer that weighs at least half the one below it, on
    /// down while the run does; everything into the lowest once what the layers hide is
    /// half their bytes.
    #[test]
    fn layers_are_planned_as_runs_twice_the_one_above() {
        let l = |size, hidden| MountLayer {
            name: String::new(),
            size,
            hidden,
        };
        assert_eq!(plan(&[]), None);
        assert_eq!(plan(&[l(100, 0)]), None);
        // Each more than twice the one above.
        assert_eq!(plan(&[l(100, 0), l(40, 0), l(15, 0), l(7, 0)]), None);
        // The top weighs half the next: those two, and the next below, which their 25
        // weigh half of, but not the lowest, which their 65 do not.
        assert_eq!(plan(&[l(1000, 0), l(40, 0), l(15, 0), l(10, 0)]), Some(1..=3));
        assert_eq!(plan(&[l(100, 0), l(40, 0), l(15, 0), l(10, 0)]), Some(0..=3));
        // Its bytes, and those it hides.
        assert_eq!(plan(&[l(1000, 0), l(10, 0), l(2, 3)]), Some(1..=2));
        // A violation below the top: the topmost one is merged first.
        assert_eq!(plan(&[l(100, 0), l(60, 0), l(20, 0), l(1, 0)]), Some(0..=1));
        // What they hide half their bytes, though each weighs more than twice the one
        // above it: all into the lowest. Less than half: none.
        assert_eq!(plan(&[l(100, 0), l(1, 48), l(1, 22)]), Some(0..=2));
        assert_eq!(plan(&[l(100, 0), l(30, 15), l(10, 10)]), None);
    }

    /// A record kept by a step at a time keeps few layers, each more than twice the one
    /// above it, so no more than the logarithm of its bytes over the least a layer has;
    /// its content what its steps left, every file a merge left gone.
    #[test]
    fn a_record_keeps_few_layers_as_it_grows() {
        let (_d, s) = store();
        let names: Vec<String> = (0..40).map(|i| format!("f{i:02}")).collect();
        for (i, name) in names.iter().enumerate() {
            let held = s.take_mount("/k", Sharing::Private, "c", "a").unwrap();
            assert_eq!(plan(&held.layers), None, "after {i} steps: {:?}", held.layers);
            let total: u64 = held.layers.iter().map(|l| l.size).sum();
            let least = held.layers.iter().map(|l| l.size).min().unwrap_or(1);
            assert!(
                held.layers.len() <= (total / least).max(1).ilog2() as usize + 1,
                "after {i} steps: {:?}",
                held.layers
            );
            let c = changes(
                &s,
                &held,
                &format!("p{i}"),
                &[(name, Some(b'a' + (i % 26) as u8), 100)],
                0,
            );
            s.used_mount("/k", &held, Some(c), MountRoot::default()).unwrap();
        }
        let held = s.take_mount("/k", Sharing::Private, "c", "a").unwrap();
        let shown = shown(&held);
        assert_eq!(shown.len(), 40);
        assert!(
            shown
                .iter()
                .enumerate()
                .all(|(i, (n, b))| *n == names[i] && b.len() == 100)
        );
        let files = fs::read_dir(s.layers_dir("a").unwrap()).unwrap().count();
        assert_eq!(files, held.layers.len(), "the merged layers' files are gone");
    }

    /// What a step's changes hide goes: once half of what a record holds is hidden, its
    /// layers are merged into the lowest, and what is hidden is gone.
    #[test]
    fn what_a_record_hides_goes() {
        let (_d, s) = store();
        let held = s.take_mount("/k", Sharing::Private, "c", "a").unwrap();
        let big = changes(
            &s,
            &held,
            "c0",
            &[("big", Some(b'b'), 200_000), ("keep", Some(b'k'), 10)],
            0,
        );
        s.used_mount("/k", &held, Some(big), MountRoot::default())
            .unwrap();
        drop(held);
        let held = s.take_mount("/k", Sharing::Private, "c", "a").unwrap();
        let before = held.layers[0].size;
        // A step removed `big`: a whiteout, which hides its 200,000 bytes' blocks.
        let gone = changes(
            &s,
            &held,
            "c1",
            &[("big", None, 0)],
            200_000u64.next_multiple_of(4096),
        );
        s.used_mount("/k", &held, Some(gone), MountRoot::default())
            .unwrap();
        drop(held);
        let held = s.take_mount("/k", Sharing::Private, "c", "a").unwrap();
        assert_eq!(held.layers.len(), 1);
        assert_eq!(held.layers[0].hidden, 0);
        assert!(
            held.layers[0].size < before / 10,
            "{} of {before}",
            held.layers[0].size
        );
        assert_eq!(shown(&held), vec![("keep".into(), vec![b'k'; 10])]);
    }

    /// A step reads the layers it took while another's merge removes their files.
    #[test]
    fn a_step_reads_what_it_took_while_a_merge_removes_it() {
        let (_d, s) = store();
        let first = s.take_mount("/k", Sharing::Shared, "c", "a").unwrap();
        let c = changes(&s, &first, "c0", &[("x", Some(b'x'), 10)], 0);
        s.used_mount("/k", &first, Some(c), MountRoot::default()).unwrap();
        drop(first);
        let reader = s.take_mount("/k", Sharing::Shared, "c", "r").unwrap();
        let writer = s.take_mount("/k", Sharing::Shared, "c", "w").unwrap();
        assert_eq!((reader.id.as_str(), writer.id.as_str()), ("a", "a"));
        // Two layers alike: merged, the first's file removed.
        let c = changes(&s, &writer, "c1", &[("y", Some(b'y'), 10)], 0);
        s.used_mount("/k", &writer, Some(c), MountRoot::default())
            .unwrap();
        assert!(!s.layers_dir("a").unwrap().join("0.erofs").exists());
        assert_eq!(shown(&reader), vec![("x".into(), vec![b'x'; 10])]);
        drop((reader, writer));
        let after = s.take_mount("/k", Sharing::Shared, "c", "n").unwrap();
        assert_eq!(after.layers.len(), 1);
        assert_eq!(
            shown(&after),
            vec![("x".into(), vec![b'x'; 10]), ("y".into(), vec![b'y'; 10])]
        );
    }

    /// Two merges of one run, planned alike, as two steps that hold a record each plan one:
    /// the first is kept, the second, whose run the first took, goes, its file with it, and
    /// the record is as the first left it.
    #[test]
    fn a_merge_whose_run_another_took_goes() {
        let (_d, s) = store();
        let held = s.take_mount("/k", Sharing::Shared, "c", "a").unwrap();
        for (i, name) in ["x", "y"].into_iter().enumerate() {
            let c = changes(&s, &held, &format!("c{i}"), &[(name, Some(b'a'), 10)], 0);
            assert!(s.keep_layer("/k", &held, Some(c), MountRoot::default()).unwrap());
        }
        let first = s.plan_merge("/k", "a").unwrap().unwrap();
        let second = s.plan_merge("/k", "a").unwrap().unwrap();
        assert_eq!(first.names, ["0.erofs", "1.erofs"]);
        assert_eq!(second.names, first.names);
        let (part, merged) = s.merge_run("a", &first).unwrap();
        assert!(s.commit_merge("/k", "a", &first, part, merged).unwrap());
        let (part, merged) = s.merge_run("a", &second).unwrap();
        let path = part.path.clone();
        assert!(!s.commit_merge("/k", "a", &second, part, merged).unwrap());
        assert!(!path.exists(), "the merge whose run was taken leaves nothing");
        drop(held);
        let again = s.take_mount("/k", Sharing::Shared, "c", "b").unwrap();
        assert_eq!(again.layers.len(), 1);
        assert_eq!(again.layers[0].name, "2.erofs");
        assert_eq!(
            shown(&again),
            vec![("x".into(), vec![b'a'; 10]), ("y".into(), vec![b'a'; 10])]
        );
        let files = fs::read_dir(s.layers_dir("a").unwrap()).unwrap().count();
        assert_eq!(files, 1);
    }

    /// A run is put in its place only where it is whole: its first layer where it was, and
    /// the rest after it, as a merge above it would leave it otherwise; and a run from the
    /// lowest only there. Runs made by hand, as the planner makes none that overlap so.
    #[test]
    fn a_run_is_put_in_its_place_only_where_it_is_whole() {
        let (_d, s) = store();
        let held = s.take_mount("/k", Sharing::Shared, "c", "a").unwrap();
        for (i, name) in ["w", "x", "y", "z"].into_iter().enumerate() {
            let c = changes(
                &s,
                &held,
                &format!("c{i}"),
                &[(name, Some(b'a' + i as u8), 10)],
                0,
            );
            assert!(s.keep_layer("/k", &held, Some(c), MountRoot::default()).unwrap());
        }
        let dir = s.layers_dir("a").unwrap();
        let run = |bottom: bool, names: &[&str]| Run {
            bottom,
            names: names.iter().map(|n| n.to_string()).collect(),
            files: names
                .iter()
                .map(|n| (File::open(dir.join(n)).unwrap(), 0))
                .collect(),
        };
        let below = run(false, &["1.erofs", "2.erofs"]);
        let lowest = run(true, &["0.erofs", "1.erofs"]);
        let above = run(false, &["2.erofs", "3.erofs"]);
        let (part, merged) = s.merge_run("a", &above).unwrap();
        assert!(s.commit_merge("/k", "a", &above, part, merged).unwrap());
        // `1.erofs` is where it was, but `2.erofs` no longer follows it.
        let (part, merged) = s.merge_run("a", &below).unwrap();
        assert!(!s.commit_merge("/k", "a", &below, part, merged).unwrap());
        drop(held);
        let held = s.take_mount("/k", Sharing::Shared, "c", "b").unwrap();
        let names: Vec<&str> = held.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["0.erofs", "1.erofs", "4.erofs"]);
        assert_eq!(
            shown(&held).into_iter().map(|(n, _)| n).collect::<Vec<_>>(),
            ["w", "x", "y", "z"]
        );
        // A run from the lowest whose first layer is no longer the lowest goes too.
        let shifted = run(true, &["1.erofs", "4.erofs"]);
        let (part, merged) = s.merge_run("a", &shifted).unwrap();
        assert!(!s.commit_merge("/k", "a", &shifted, part, merged).unwrap());
        // And one whole, kept.
        let (part, merged) = s.merge_run("a", &lowest).unwrap();
        assert!(s.commit_merge("/k", "a", &lowest, part, merged).unwrap());
        drop(held);
        let held = s.take_mount("/k", Sharing::Shared, "c", "c").unwrap();
        let names: Vec<&str> = held.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["5.erofs", "4.erofs"]);
        assert_eq!(
            shown(&held).into_iter().map(|(n, _)| n).collect::<Vec<_>>(),
            ["w", "x", "y", "z"]
        );
    }

    /// A record one of whose layers' files is gone is taken no more: a step gets a fresh
    /// one.
    #[test]
    fn a_record_missing_a_layer_is_taken_no_more() {
        let (_d, s) = store();
        let held = s.take_mount("/k", Sharing::Private, "c", "a").unwrap();
        let c = changes(&s, &held, "c0", &[("x", Some(b'x'), 10)], 0);
        s.used_mount("/k", &held, Some(c), MountRoot::default()).unwrap();
        drop(held);
        fs::remove_file(s.layers_dir("a").unwrap().join("0.erofs")).unwrap();
        let next = s.take_mount("/k", Sharing::Private, "c", "b").unwrap();
        assert_eq!((next.id.as_str(), next.fresh), ("b", true));
    }

    /// What a removal leaves where Windows refused to move a record a step held open as it
    /// went, gone, and where a removal stopped, aside: neither listed nor taken, and taken
    /// away as the next collection starts.
    #[test]
    fn what_a_removal_leaves_goes_at_the_next_collection() {
        let (_d, s) = store();
        drop(s.take_mount("/k", Sharing::Private, "c", "a").unwrap());
        let dir = s.mount_dir("a").unwrap();
        fs::rename(dir.join("record.json"), dir.join(GONE)).unwrap();
        let aside = s.records_dir().join(".b-1");
        fs::create_dir_all(aside.join("x")).unwrap();
        assert!(s.mount_entries().unwrap().is_empty());
        assert!(!s.remove_mount("a").unwrap(), "gone already");
        let fresh = s.take_mount("/k", Sharing::Private, "c", "c").unwrap();
        assert!(fresh.fresh, "a record gone is taken no more");
        // What steps left in records: a part never kept, in one no step holds and in one
        // a step holds.
        let left = s.mount_part(&fresh, "x", 10).unwrap();
        let left_path = s.layers_dir("c").unwrap().join("x.part");
        std::mem::forget(left);
        let held = s.take_mount("/h", Sharing::Private, "c", "h").unwrap();
        let writing = s.mount_part(&held, "y", 10).unwrap();
        drop(fresh);
        drop(s.collect().unwrap());
        assert!(!dir.exists() && !aside.exists());
        assert!(!left_path.exists(), "a part no step holds goes");
        assert!(
            s.layers_dir("h").unwrap().join("y.part").exists(),
            "one a step holds stays"
        );
        drop((writing, held));
        let ids: Vec<String> = s.mount_entries().unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["c", "h"]);
    }

    /// A step not answered from the cache starts its caches empty: records no step holds
    /// go, one held is taken no more; another key's, or one from something, stay.
    #[test]
    fn a_step_not_answered_from_the_cache_lets_its_records_go() {
        let (_d, s) = store();
        let held = s.take_mount("/k", Sharing::Private, "c", "a").unwrap();
        drop(s.take_mount("/k", Sharing::Private, "c", "b").unwrap());
        drop(s.take_mount("/k:from", Sharing::Private, "c", "f").unwrap());
        drop(s.take_mount("/other", Sharing::Private, "c", "o").unwrap());
        s.prune_mounts("/k", false).unwrap();
        let ids: Vec<String> = s.mount_entries().unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["a", "f", "o"]);
        drop(held);
        // `a`, let go of while held, is taken no more: a new record.
        let next = s.take_mount("/k", Sharing::Shared, "c", "n").unwrap();
        assert_eq!((next.id.as_str(), next.fresh), ("n", true));
        // From something: the records of `id:` alone.
        s.prune_mounts("/k", true).unwrap();
        let ids: Vec<String> = s.mount_entries().unwrap().into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["a", "n", "o"]);
    }
}
