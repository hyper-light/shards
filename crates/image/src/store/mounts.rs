//! Cache mounts kept across builds (docs/design/architecture.md D114): each record a
//! cache's content, as the layers its steps' changes made, in order, taken by a step as
//! BuildKit's cache mount manager takes a cache directory (dockerfile/1.27.1
//! solver/llbsolver/mounts/mount.go `getRefCacheDir`): by its key, shared, private or
//! locked, among every build of the store, whichever process runs it.
//!
//! A record is `cachemounts/v1/records/<id>/`: `record.json`, and `lock`, whose lock
//! (flock(2) where there is one) holds it for a step, shared by `sharing=shared`'s takers
//! and exclusively by `private`'s and `locked`'s. A key's records are looked through, made
//! and written under the lock of `cachemounts/v1/keys/<SHA-256 of the key>`, as BuildKit
//! looks through and makes them under its `cacheRefsLocker` of the key.

use std::fs::{self, File, TryLockError};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use super::{Partial, Store, sync_dir, unix_now};
use crate::{Error, bad};

pub(super) const VERSION: u32 = 1;

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

/// A layer of a cache's content: its blob, as the store keeps it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MountLayer {
    pub blob: String,
    pub diff_id: String,
    pub media_type: String,
    pub size: u64,
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
    /// How many of the first layers are its first content's (`from=`), not its own.
    seed: usize,
    #[serde(default)]
    root: MountRoot,
    /// BuildKit's description of it: `cached mount DEST from exec ARGS`.
    description: String,
    created: i64,
    last_used: i64,
    usage: u64,
    /// Let go of while a step held it ([`Store::prune_mounts`]): no step takes it again.
    #[serde(default)]
    pruned: bool,
}

/// A record a step holds, until it is dropped: its id, and its content as it was taken.
#[derive(Debug)]
pub struct HeldMount {
    pub id: String,
    pub layers: Vec<MountLayer>,
    pub root: MountRoot,
    /// Whether it was made for this step: none of the key's was free.
    pub fresh: bool,
    _lock: File,
}

/// A record as `system df` and `builder prune` see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub id: String,
    pub description: String,
    /// The bytes of its own layers, its first content's not counted: what removing it
    /// frees, as a mutable ref's size is its own snapshot's.
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
    /// content is set ([`Store::seed_mount`]).
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
                let lock = open_lock(&self.mount_dir(&id)?.join("lock"))?;
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
                seed: 0,
                root: MountRoot::default(),
                description: description.to_string(),
                created: now,
                last_used: now,
                usage: 0,
                pruned: false,
            },
        )?;
        sync_dir(&self.records_dir())?;
        drop(keys);
        Ok(HeldMount {
            id: new_id.to_string(),
            layers: Vec::new(),
            root: MountRoot::default(),
            fresh: true,
            _lock: lock,
        })
    }

    /// `lock`, held, as record `id`'s, with what it holds now; none where it went between
    /// its listing and its lock ([`Store::remove_mount`]).
    fn held_mount(&self, id: &str, lock: File) -> Result<Option<HeldMount>, Error> {
        Ok(self.read_mount(id)?.map(|record| HeldMount {
            id: id.to_string(),
            layers: record.layers,
            root: record.root,
            fresh: false,
            _lock: lock,
        }))
    }

    /// Sets the first content of `held`, a fresh record of `key`: the layers of what its
    /// mount is from, and that one's root.
    pub fn seed_mount(
        &self,
        key: &str,
        held: &mut HeldMount,
        layers: Vec<MountLayer>,
        root: MountRoot,
    ) -> Result<(), Error> {
        let _keys = self.key_lock(key)?;
        let Some(mut record) = self.read_mount(&held.id)? else {
            return bad(format!("cache mount {} is gone", held.id));
        };
        if !record.layers.is_empty() {
            return bad(format!("cache mount {} has content already", held.id));
        }
        record.seed = layers.len();
        record.layers.clone_from(&layers);
        record.root = root;
        self.write_mount(&held.id, &record)?;
        held.layers = layers;
        held.root = root;
        Ok(())
    }

    /// Counts a step's use of `held`, a record of `key`: its changes, if any, the record's
    /// next layer, and `root` its root from now on.
    pub fn used_mount(
        &self,
        key: &str,
        held: &HeldMount,
        changes: Option<MountLayer>,
        root: MountRoot,
    ) -> Result<(), Error> {
        let _keys = self.key_lock(key)?;
        let Some(mut record) = self.read_mount(&held.id)? else {
            return bad(format!("cache mount {} is gone", held.id));
        };
        record.layers.extend(changes);
        record.root = root;
        record.last_used = unix_now();
        record.usage = record.usage.saturating_add(1);
        self.write_mount(&held.id, &record)
    }

    /// Every record, in use or not.
    pub fn mount_entries(&self) -> Result<Vec<MountEntry>, Error> {
        let mut out = Vec::new();
        for id in self.mount_ids()? {
            let Some(record) = self.read_mount(&id)? else {
                continue;
            };
            let lock = open_lock(&self.mount_dir(&id)?.join("lock"))?;
            let in_use = !try_lock(&lock, false)?;
            drop(lock);
            out.push(MountEntry {
                description: record.description,
                size: record
                    .layers
                    .iter()
                    .skip(record.seed)
                    .fold(0u64, |n, l| n.saturating_add(l.size)),
                created: record.created,
                last_used: record.last_used,
                usage: record.usage,
                in_use,
                id,
            });
        }
        Ok(out)
    }

    /// Removes record `id` unless a step holds it: whether it went. Its blobs go at the
    /// next collection, unless held otherwise.
    pub fn remove_mount(&self, id: &str) -> Result<bool, Error> {
        let dir = self.mount_dir(id)?;
        let lock = match File::options().write(true).open(dir.join("lock")) {
            Ok(lock) => lock,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if !try_lock(&lock, false)? {
            return Ok(false);
        }
        // Moved aside first: a taker that locks it after this finds no record there.
        let gone = self.records_dir().join(format!(".{id}-{}", std::process::id()));
        fs::rename(&dir, &gone)?;
        drop(lock);
        fs::remove_dir_all(&gone)?;
        sync_dir(&self.records_dir())?;
        Ok(true)
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

    /// The blobs every record's layers are, which a collection keeps.
    pub(super) fn mount_blobs(&self) -> Result<Vec<String>, Error> {
        let mut out = Vec::new();
        for id in self.mount_ids()? {
            if let Some(record) = self.read_mount(&id)? {
                out.extend(record.layers.into_iter().map(|l| l.blob));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A store of its own, removed when dropped.
    struct Temp(PathBuf);

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn store() -> (Temp, Store) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("shards-mounts-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir).unwrap();
        (Temp(dir), store)
    }

    fn layer(n: u8) -> MountLayer {
        MountLayer {
            blob: format!("sha256:{}", format!("{n:02x}").repeat(32)),
            diff_id: format!("sha256:{}", format!("{n:02x}").repeat(32)),
            media_type: "application/vnd.oci.image.layer.v1.tar".into(),
            size: u64::from(n) * 10,
        }
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
        s.seed_mount("/k", &mut held, vec![layer(1)], seeded).unwrap();
        s.used_mount("/k", &held, Some(layer(2)), seeded).unwrap();
        let chowned = MountRoot { uid: 7, ..seeded };
        s.used_mount("/k", &held, None, chowned).unwrap();
        let entries = s.mount_entries().unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        // Its own layer counted, its first content's not.
        assert_eq!((e.size, e.usage, e.in_use), (20, 2, true));
        assert!(!s.remove_mount("a").unwrap());
        drop(held);
        let again = s.take_mount("/k", Sharing::Shared, "c", "b").unwrap();
        assert_eq!(again.layers, vec![layer(1), layer(2)]);
        assert_eq!(again.root, chowned);
        assert_eq!(s.mount_blobs().unwrap(), vec![layer(1).blob, layer(2).blob]);
        drop(again);
        assert!(s.remove_mount("a").unwrap());
        assert!(s.mount_entries().unwrap().is_empty());
        assert!(!s.remove_mount("a").unwrap());
        assert!(s.remove_mount("../x").is_err());
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
