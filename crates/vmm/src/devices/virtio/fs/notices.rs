//! What a share process tells of the changes its host makes beneath the directories it
//! shares (D235), for its guest's inotify: Linux's virtio-fs driver has no notification
//! queue (fs/fuse/virtio_fs.c negotiates no feature), so a host's edit wakes no watch in the
//! guest by itself. Each change is a path within its share, told once however often it
//! changed while it waited ([`Pending`]), to the share process's starter as records
//! (`shards_abi::run::notice`, its share's index for its name), which hands them to the
//! guest's init to touch (its NOTICE built-in).
//!
//! What is watched is each host's own mechanism ([`watch`]): on Linux an inotify watch of
//! each directory the guest holds, made as it looks one up and let go of as it forgets it,
//! all of the process's one instance (PM M257); on macOS one FSEvents stream of the shares'
//! roots, the changes this process makes for the guest left out (IgnoreSelf); on Windows
//! ReadDirectoryChangesW of each share's root and all beneath it. Where the whole tree is
//! watched (macOS, Windows), a change is told only where the guest holds its directory
//! ([`Server::holds_dir`]): what the guest never looked at it watches nothing of.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::sync::{Condvar, Mutex, PoisonError};

use super::server::Server;

/// What a host's watcher saw of a share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// What is at the path, within the share, changed.
    At(Vec<u8>),
    /// What changed beneath the path (a directory; empty for the share's root) was not each
    /// said: inotify's queue overflowed (`IN_Q_OVERFLOW`, inotify(7)), ReadDirectoryChangesW
    /// answered nothing as its buffer overflowed, or FSEvents said only that the directory
    /// must be scanned (`kFSEventStreamEventFlagMustScanSubDirs`, as it says of events the
    /// kernel or the stream dropped).
    Beneath(Vec<u8>),
}

/// The changes waiting to be told, each share's paths, each once, in the order first seen;
/// each share named in its records as its place among its process's shares says
/// ([`Pending::new`]).
///
/// How many of a share's wait, each told by its path, is bounded by the nodes its guest
/// holds ([`Server::held`]), as the share's server holds them now. A guest's inotify watch
/// holds its inode (fs/notify/inotify/inotify_user.c, `inotify_new_watch` takes the inode
/// the path names), and a FUSE inode its server's node until the guest forgets it, so
/// every watch the guest has is of a node held; told each of those, every watch is woken.
/// Past that bound, telling each path costs more than telling every node held, so the
/// share's changes are told as lost ([`Change::Beneath`] its root): when they are taken,
/// by every node its guest then holds beneath where they were lost
/// ([`Server::held_beneath`]), which wakes every watch there, a file's own among them,
/// as a burst a host's watcher lost or coalesced does too. The bound is never below one,
/// as the share's root is always held; and a share's places lost are as bounded.
#[derive(Debug, Default)]
pub struct Pending {
    names: Vec<Vec<u8>>,
    queued: Mutex<Queue>,
    ready: Condvar,
}

#[derive(Debug, Default)]
struct Queue {
    order: VecDeque<(u32, Vec<u8>)>,
    seen: HashSet<(u32, Vec<u8>)>,
    /// How many of `order` are each share's.
    counts: HashMap<u32, usize>,
    /// Each share's places beneath which changes were lost, none beneath another of its.
    lost: Vec<(u32, Vec<u8>)>,
}

/// Whether `path` is `place` or beneath it, both within a share, `place` empty for its root.
fn beneath(path: &[u8], place: &[u8]) -> bool {
    place.is_empty()
        || path
            .strip_prefix(place)
            .is_some_and(|rest| rest.is_empty() || rest.first() == Some(&b'/'))
}

impl Queue {
    /// What changed beneath `place` in `share` lost; `held`, the nodes its guest holds.
    fn lose(&mut self, share: u32, place: Vec<u8>, held: usize) {
        if self.lost.iter().any(|(s, l)| *s == share && beneath(&place, l)) {
            return;
        }
        self.lost.retain(|(s, l)| *s != share || !beneath(l, &place));
        let places = self.lost.iter().filter(|(s, _)| *s == share).count();
        // No more places lost than nodes held: past them, the root stands for them all.
        if places >= held {
            self.lost.retain(|(s, _)| *s != share);
            self.lost.push((share, Vec::new()));
        } else {
            self.lost.push((share, place));
        }
    }
}

impl Pending {
    /// Changes of the shares `names` name, by their places among the process's shares: the
    /// guest's names of them, their virtio-fs tags, which its init mounts them by.
    pub fn new(names: Vec<Vec<u8>>) -> Pending {
        Pending {
            names,
            ..Pending::default()
        }
    }

    /// Path `path` of share `share` changed, its guest holding `held` nodes now: told once,
    /// however often it changes before it is; with as many of the share's waiting as its
    /// guest holds nodes, the share's changes told as lost.
    pub fn tell(&self, share: u32, path: Vec<u8>, held: usize) {
        let mut q = self.queued.lock().unwrap_or_else(PoisonError::into_inner);
        if q.lost.iter().any(|(s, l)| *s == share && beneath(&path, l)) {
            return;
        }
        let key = (share, path);
        if q.seen.contains(&key) {
            return;
        }
        let count = q.counts.get(&share).copied().unwrap_or(0);
        if count >= held {
            q.lose(share, Vec::new(), held);
        } else {
            *q.counts.entry(share).or_insert(0) = count.saturating_add(1);
            q.seen.insert(key.clone());
            q.order.push_back(key);
        }
        self.ready.notify_one();
    }

    /// What changed beneath `place` in share `share` was lost ([`Change::Beneath`]), its
    /// guest holding `held` nodes now.
    pub fn lose(&self, share: u32, place: Vec<u8>, held: usize) {
        let mut q = self.queued.lock().unwrap_or_else(PoisonError::into_inner);
        q.lose(share, place, held);
        self.ready.notify_one();
    }

    /// `change` of share `share`, whose server is `server`.
    pub fn told(&self, share: u32, server: &Server, change: Change) {
        let held = server.held();
        match change {
            Change::At(path) => self.tell(share, path, held),
            Change::Beneath(place) => self.lose(share, place, held),
        }
    }

    /// [`take`](Self::take), for no longer than `wait`: none where none came.
    #[cfg(test)]
    fn take_within(&self, servers: &[Server], shares: &[u32], wait: std::time::Duration) -> Option<Vec<u8>> {
        let q = self.queued.lock().unwrap_or_else(PoisonError::into_inner);
        let (q, _) = self
            .ready
            .wait_timeout_while(q, wait, |q| q.order.is_empty() && q.lost.is_empty())
            .unwrap_or_else(PoisonError::into_inner);
        let empty = q.order.is_empty() && q.lost.is_empty();
        drop(q);
        (!empty).then(|| self.take(servers, shares))
    }

    /// The changes waiting, taken, as records: none until there is one. What was lost is
    /// told by every node its guest holds beneath where it was, the k-th of `servers` being
    /// share `shares[k]`'s, as it holds them now.
    fn take(&self, servers: &[Server], shares: &[u32]) -> Vec<u8> {
        let (order, lost) = {
            let mut q = self.queued.lock().unwrap_or_else(PoisonError::into_inner);
            while q.order.is_empty() && q.lost.is_empty() {
                q = self.ready.wait(q).unwrap_or_else(PoisonError::into_inner);
            }
            q.seen.clear();
            q.counts.clear();
            (std::mem::take(&mut q.order), std::mem::take(&mut q.lost))
        };
        let name = |share: u32| usize::try_from(share).ok().and_then(|i| self.names.get(i));
        let mut out = Vec::new();
        for (share, path) in order {
            if let Some(name) = name(share) {
                shards_abi::run::notice::encode(name, &path, &mut out);
            }
        }
        for (share, place) in lost {
            let server = shares
                .iter()
                .position(|s| *s == share)
                .and_then(|k| servers.get(k));
            if let (Some(name), Some(server)) = (name(share), server) {
                for path in server.held_beneath(&place) {
                    shards_abi::run::notice::encode(name, &path, &mut out);
                }
            }
        }
        out
    }

    /// Tells `out` each change of `servers`' shares (the k-th being share `shares[k]`) as
    /// it comes, those that came while it was written taken together, until it refuses
    /// one: its reader has gone.
    pub fn send(&self, servers: &[Server], shares: &[u32], out: &mut impl Write) -> io::Result<()> {
        loop {
            let records = self.take(servers, shares);
            out.write_all(&records)?;
            out.flush()?;
        }
    }
}

/// Watches the host's changes beneath each of `servers`' shares, the k-th told `pending`
/// as share `shares[k]`, its place among its process's shares, on threads of their own that
/// last as long as the process: each server's, and `pending`'s, for as long.
pub fn watch(
    servers: &'static [Server],
    shares: &'static [u32],
    pending: &'static Pending,
) -> Result<(), String> {
    if servers.len() != shares.len() {
        return Err("a share's place for each server".into());
    }
    host::watch(Watched {
        servers,
        shares,
        pending,
    })
}

/// What a host's watcher tells of: its servers, each one's place among its process's
/// shares, and where its changes wait.
#[derive(Clone, Copy)]
struct Watched {
    servers: &'static [Server],
    shares: &'static [u32],
    pending: &'static Pending,
}

/// The k-th server's change that the guest is told: a change at a path where it holds the
/// directory it is in, or it is that directory (D235); what was lost beneath a place,
/// whatever it holds there ([`Pending::take`] tells what it does).
#[cfg(not(target_os = "linux"))]
fn told(w: Watched, k: usize, change: Change) {
    let (Some(server), Some(&share)) = (w.servers.get(k), w.shares.get(k)) else {
        return;
    };
    if let Change::At(path) = &change {
        let dir = match path.iter().rposition(|&b| b == b'/') {
            Some(cut) => path.get(..cut).unwrap_or_default(),
            None => &[][..],
        };
        if !server.holds_dir(dir) && !server.holds_dir(path) {
            return;
        }
    }
    w.pending.told(share, server, change);
}

#[cfg(target_os = "linux")]
mod host {
    use super::*;

    /// Each server's directories the guest holds watched with the process's one inotify
    /// instance as it looks them up, their events sent on one channel, each event's paths
    /// told on a thread of its own.
    pub fn watch(w: Watched) -> Result<(), String> {
        let (sink, events) = std::sync::mpsc::channel();
        for (server, &share) in w.servers.iter().zip(w.shares) {
            server
                .watch_dirs(u64::from(share), sink.clone())
                .map_err(|e| format!("share {share}: watching its directory: {e}"))?;
        }
        drop(sink);
        std::thread::Builder::new()
            .name("notices".into())
            .spawn(move || {
                while let Ok(event) = events.recv() {
                    for (server, &share) in w.servers.iter().zip(w.shares) {
                        for change in server.noticed(&event) {
                            w.pending.told(share, server, change);
                        }
                    }
                }
            })
            .map(drop)
            .map_err(|e| format!("its notices' thread: {e}"))
    }
}

#[cfg(target_os = "macos")]
mod host {
    use super::*;

    /// One FSEvents stream of every share's root, its changes told on a thread of their
    /// own, each by its share and its path within it.
    pub fn watch(w: Watched) -> Result<(), String> {
        let roots: Vec<Vec<u8>> = w
            .servers
            .iter()
            .zip(w.shares)
            .map(|(s, share)| {
                s.root_path()
                    .ok_or_else(|| format!("share {share}: its directory's path"))
            })
            .collect::<Result<_, _>>()?;
        let (sink, changes) = std::sync::mpsc::channel();
        let paths: Vec<&[u8]> = roots.iter().map(Vec::as_slice).collect();
        let stream = shards_apple::fsevents::watch(&paths, sink)?;
        std::thread::Builder::new()
            .name("notices".into())
            .spawn(move || {
                // The stream, held for as long as its changes are read: the process's life.
                let _stream = stream;
                while let Ok(change) = changes.recv() {
                    for (k, root) in roots.iter().enumerate() {
                        if let Some(path) = within(root, &change.path) {
                            let change = if change.flags & shards_apple::fsevents::MUST_SCAN_SUBDIRS != 0 {
                                Change::Beneath(path)
                            } else {
                                Change::At(path)
                            };
                            told(w, k, change);
                        }
                    }
                }
            })
            .map(drop)
            .map_err(|e| format!("its notices' thread: {e}"))
    }

    /// `path`'s place beneath `root`, its names joined by `/`: empty for `root` itself, none
    /// for a path outside it.
    pub fn within(root: &[u8], path: &[u8]) -> Option<Vec<u8>> {
        let rest = path.strip_prefix(root)?;
        match rest {
            [] => Some(Vec::new()),
            [b'/', rest @ ..] => Some(rest.to_vec()),
            _ => None,
        }
    }
}

#[cfg(windows)]
mod host {
    use super::*;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
        FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_NOTIFY_INFORMATION,
        ReadDirectoryChangesW,
    };

    /// What a read of a share's changes takes at most: 64 KiB, what ReadDirectoryChangesW
    /// takes of a buffer over the network, its documented most (MSDN, "ReadDirectoryChangesW",
    /// "fails with ERROR_INVALID_PARAMETER when the buffer length is greater than 64 KB"),
    /// so that a share on a network drive is watched too.
    const BUF: usize = 64 * 1024;

    /// Each share's root and all beneath it, its changes read on a thread of its own and
    /// told by their paths within it.
    pub fn watch(w: Watched) -> Result<(), String> {
        for (k, (server, share)) in w.servers.iter().zip(w.shares).enumerate() {
            let root = server
                .changes_root()
                .map_err(|e| format!("share {share}: its directory, to watch: errno {e}"))?;
            std::thread::Builder::new()
                .name(format!("notices {share}"))
                .spawn(move || read(w, k, &root))
                .map(drop)
                .map_err(|e| format!("share {share}: its notices' thread: {e}"))?;
        }
        Ok(())
    }

    /// The k-th server's changes beneath `root`, read until it can be read no more.
    fn read(w: Watched, k: usize, root: &super::super::host::Owned) {
        // Words, for each record's alignment (DWORD).
        let mut buf = vec![0u32; BUF / 4];
        loop {
            let mut got = 0u32;
            // SAFETY: a synchronous read of the directory's changes into a buffer of its
            // length, on a handle this thread holds.
            let read = unsafe {
                ReadDirectoryChangesW(
                    root.as_raw_handle(),
                    buf.as_mut_ptr().cast(),
                    u32::try_from(BUF).unwrap_or(0),
                    1,
                    FILE_NOTIFY_CHANGE_FILE_NAME
                        | FILE_NOTIFY_CHANGE_DIR_NAME
                        | FILE_NOTIFY_CHANGE_ATTRIBUTES
                        | FILE_NOTIFY_CHANGE_SIZE
                        | FILE_NOTIFY_CHANGE_LAST_WRITE,
                    &mut got,
                    std::ptr::null_mut(),
                    None,
                )
            };
            if read == 0 {
                return;
            }
            // None: more changed than the buffer held, each one lost (MSDN,
            // "ReadDirectoryChangesW": the buffer overflowed and its changes were lost).
            if got == 0 {
                told(w, k, Change::Beneath(Vec::new()));
                continue;
            }
            let bytes: Vec<u8> = buf.iter().flat_map(|word| word.to_ne_bytes()).collect();
            let bytes = bytes.get(..usize::try_from(got).unwrap_or(0)).unwrap_or_default();
            for path in changes(bytes) {
                told(w, k, Change::At(path));
            }
        }
    }

    /// The paths `bytes`, FILE_NOTIFY_INFORMATION records, name, each relative to the root
    /// watched, its names a guest's, joined by `/`.
    pub fn changes(bytes: &[u8]) -> Vec<Vec<u8>> {
        use std::mem::offset_of;
        let word = |at: usize| -> Option<usize> {
            let b = bytes.get(at..at.checked_add(4)?)?;
            usize::try_from(u32::from_ne_bytes(b.try_into().ok()?)).ok()
        };
        let mut out = Vec::new();
        let mut at = 0usize;
        loop {
            let (Some(next), Some(len)) = (
                word(at),
                word(at.saturating_add(offset_of!(FILE_NOTIFY_INFORMATION, FileNameLength))),
            ) else {
                return out;
            };
            let from = at.saturating_add(offset_of!(FILE_NOTIFY_INFORMATION, FileName));
            let Some(name) = bytes.get(from..from.saturating_add(len)) else {
                return out;
            };
            let units: Vec<u16> = name
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_ne_bytes(*c))
                .collect();
            let parts: Vec<Vec<u8>> = units
                .split(|&c| c == u16::from(b'\\'))
                .filter(|p| !p.is_empty())
                .map(super::super::ntfs::guest_name)
                .collect();
            out.push(parts.join(&b'/'));
            if next == 0 {
                return out;
            }
            at = at.saturating_add(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every path `records` holds, each with its share's name.
    fn paths_of(records: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut paths = Vec::new();
        let mut rest = records;
        while let shards_abi::run::notice::Decoded::Record { name, path, rest: r } =
            shards_abi::run::notice::decode(rest)
        {
            paths.push((name.to_vec(), path.to_vec()));
            rest = r;
        }
        paths
    }

    /// A share of a directory of the test's own holding `d/f`, its guest having looked up
    /// `d` and `d/f`, so that it holds three nodes, its root's among them.
    fn held_share() -> (shards_testdir::TempDir, &'static [Server]) {
        use super::super::proto::op;
        let dir = shards_testdir::TempDir::new("notices-held").unwrap();
        std::fs::create_dir(dir.join("d")).unwrap();
        std::fs::write(dir.join("d").join("f"), b"f").unwrap();
        #[cfg(windows)]
        let opened: super::super::host::Owned = {
            use std::os::windows::fs::OpenOptionsExt as _;
            std::fs::File::options()
                .read(true)
                .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
                .open(dir.path())
                .unwrap()
                .into()
        };
        #[cfg(unix)]
        let opened: super::super::host::Owned = std::fs::File::open(dir.path()).unwrap().into();
        let server: &'static [Server] = Vec::leak(vec![Server::new(opened, false, None).unwrap()]);
        let lookup = |parent: u64, name: &[u8]| -> u64 {
            let mut r = Vec::new();
            r.extend_from_slice(&u32::try_from(40 + name.len() + 1).unwrap().to_le_bytes());
            r.extend_from_slice(&op::LOOKUP.to_le_bytes());
            r.extend_from_slice(&7u64.to_le_bytes());
            r.extend_from_slice(&parent.to_le_bytes());
            r.extend_from_slice(&[0u8; 16]);
            r.extend_from_slice(name);
            r.push(0);
            let answered = server[0].handle(&r).unwrap();
            assert_eq!(i32::from_le_bytes(answered[4..8].try_into().unwrap()), 0);
            u64::from_le_bytes(answered[16..24].try_into().unwrap())
        };
        let d = lookup(1, b"d");
        lookup(d, b"f");
        assert_eq!(server[0].held(), 3);
        (dir, server)
    }

    /// A change is told once however often it changes before it is, each share's apart;
    /// and records come of it.
    #[test]
    fn changes_are_told_once() {
        let pending = Pending::new(vec![b"shards0".to_vec(), b"shards-plug1".to_vec()]);
        pending.tell(0, b"a/b".to_vec(), 2);
        pending.tell(0, b"a/b".to_vec(), 2);
        pending.tell(1, b"a/b".to_vec(), 2);
        let records = pending.take(&[], &[]);
        let mut want = Vec::new();
        shards_abi::run::notice::encode(b"shards0", b"a/b", &mut want);
        shards_abi::run::notice::encode(b"shards-plug1", b"a/b", &mut want);
        assert_eq!(records, want);
    }

    /// confine-06 and confine-09: what a host's watcher lost beneath a place is told by every
    /// node the guest holds there, a file deep in the share among them, which a guest's
    /// watch of that file alone waits on; and a share's changes past as many as its guest
    /// holds nodes are told as lost, so that what waits follows what the guest holds, not
    /// a number of the host's.
    #[test]
    fn lost_changes_are_told_by_every_node_the_guest_holds_beneath() {
        let (_dir, server) = held_share();
        let shares = [0u32];
        let pending = Pending::new(vec![b"shards0".to_vec(), b"shards-plug1".to_vec()]);
        let told = |pending: &Pending| -> Vec<Vec<u8>> {
            let mut paths: Vec<_> = paths_of(&pending.take(server, &shares))
                .into_iter()
                .map(|(name, path)| {
                    assert_eq!(name, b"shards0");
                    path
                })
                .collect();
            paths.sort();
            paths
        };
        // Lost at its root (inotify's overflow, ReadDirectoryChangesW's empty answer).
        pending.told(0, &server[0], Change::Beneath(Vec::new()));
        assert_eq!(told(&pending), [&b""[..], b"d", b"d/f"]);
        // Lost beneath `d` (FSEvents' MustScanSubDirs of it): `d` and what is in it alone.
        pending.told(0, &server[0], Change::Beneath(b"d".to_vec()));
        pending.told(0, &server[0], Change::At(b"d/g".to_vec()));
        assert_eq!(told(&pending), [&b"d"[..], b"d/f"]);
        // As many as it holds nodes, each told by its path; one more, and the share's
        // changes are told as lost, by every node it holds, those already waiting too.
        for path in [&b"n0"[..], b"n1", b"n2"] {
            pending.told(0, &server[0], Change::At(path.to_vec()));
        }
        assert_eq!(told(&pending), [&b"n0"[..], b"n1", b"n2"]);
        for path in [&b"n0"[..], b"n1", b"n2", b"n3", b"n4"] {
            pending.told(0, &server[0], Change::At(path.to_vec()));
        }
        assert_eq!(told(&pending), [&b""[..], b"d", b"d/f", b"n0", b"n1", b"n2"]);
        // Another share's changes are its own: share 1 has no server here, so what it lost
        // is told by nothing, and share 0's waiting changes are not lost with it.
        pending.tell(0, b"a".to_vec(), 3);
        pending.lose(1, Vec::new(), 1);
        assert_eq!(told(&pending), [&b"a"[..]]);
        // Places lost are no more than the nodes held: past them, the root stands for all.
        for place in [&b"x"[..], b"y", b"z", b"w"] {
            pending.lose(0, place.to_vec(), 3);
        }
        assert_eq!(told(&pending), [&b""[..], b"d", b"d/f"]);
    }

    /// The host's changes beneath a share are told where the guest holds their directory,
    /// and none where it holds none, by each host's own watch; made by another process, as
    /// FSEvents tells none of this process's own (IgnoreSelf). On Unix a file moved out of
    /// the share is told as its name goes, and what then changes it is told nothing: Linux
    /// tells a directory's watch of the changes made through its own names (fsnotify_parent,
    /// of the dentry a change went through), and FSEvents tells a file's changes by its path
    /// now, which a rename moves (a second name outside the share it does not: there it
    /// told `held/k`, PM M550). containers.rs's DAX test moves a file its guest holds out of
    /// the share before it cuts it, so that its guest reads past the cut before anything
    /// tells it the size (D189); Windows refuses that cut wherever the file is (PM M412).
    #[test]
    fn the_hosts_changes_are_told_where_the_guest_looks() {
        use super::super::proto::op;
        let dir = shards_testdir::TempDir::new("notices").unwrap();
        // Its real path: FSEvents tells paths without the links on the way.
        let root = std::fs::canonicalize(&dir).unwrap();
        for d in ["held", "unheld"] {
            std::fs::create_dir(root.join(d)).unwrap();
        }
        // `held/k`, made before anything is watched.
        #[cfg(unix)]
        std::fs::write(root.join("held").join("k"), b"k").unwrap();
        #[cfg(windows)]
        let opened: super::super::host::Owned = {
            use std::os::windows::fs::OpenOptionsExt as _;
            std::fs::File::options()
                .read(true)
                .custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS)
                .open(&root)
                .unwrap()
                .into()
        };
        #[cfg(unix)]
        let opened: super::super::host::Owned = std::fs::File::open(&root).unwrap().into();
        let server: &'static [Server] = Vec::leak(vec![Server::new(opened, false, None).unwrap()]);
        let lookup = {
            let mut r = Vec::new();
            let body = b"held\0";
            r.extend_from_slice(&u32::try_from(40 + body.len()).unwrap().to_le_bytes());
            r.extend_from_slice(&op::LOOKUP.to_le_bytes());
            r.extend_from_slice(&7u64.to_le_bytes());
            r.extend_from_slice(&1u64.to_le_bytes());
            r.extend_from_slice(&[0u8; 16]);
            r.extend_from_slice(body);
            r
        };
        // Watched as its share process watches it, before its guest asks anything.
        let pending: &'static Pending = Box::leak(Box::new(Pending::new(vec![b"shards0".to_vec()])));
        watch(server, Vec::leak(vec![0]), pending).unwrap();
        let answered = server[0].handle(&lookup).unwrap();
        assert_eq!(i32::from_le_bytes(answered[4..8].try_into().unwrap()), 0);
        // Written by another process.
        let write = |path: &std::path::Path| {
            #[cfg(unix)]
            let wrote = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg("echo x > \"$1\"")
                .arg("sh")
                .arg(path)
                .status();
            // cmd's own quoting, which no argument of Rust's quoting is.
            #[cfg(windows)]
            let wrote = {
                use std::os::windows::process::CommandExt as _;
                std::process::Command::new("cmd")
                    .arg("/c")
                    .raw_arg(format!("echo x> \"{}\"", path.display()))
                    .status()
            };
            assert!(wrote.unwrap().success());
        };
        write(&root.join("unheld").join("u"));
        write(&root.join("held").join("h"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        // What is told until `until` is, in order.
        let told_until = |until: &[u8]| {
            let mut told: Vec<Vec<u8>> = Vec::new();
            while !told.iter().any(|p| p == until) {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                assert!(
                    !left.is_zero(),
                    "{} not told; told {told:?}",
                    String::from_utf8_lossy(until)
                );
                let records = pending.take_within(server, &[0], left).unwrap_or_default();
                let mut rest = records.as_slice();
                while let shards_abi::run::notice::Decoded::Record { name, path, rest: r } =
                    shards_abi::run::notice::decode(rest)
                {
                    assert_eq!(name, b"shards0");
                    told.push(path.to_vec());
                    rest = r;
                }
            }
            told
        };
        let told = told_until(b"held/h");
        assert!(!told.iter().any(|p| p.starts_with(b"unheld/")), "{told:?}");
        // `k` moved out by another process, told as its name goes; then written where it is
        // now, and `after` written within the share: each host tells its changes in the order
        // they come, so that `k`, were its write told, is told by `after`.
        #[cfg(unix)]
        {
            let outside = shards_testdir::TempDir::new("notices-o").unwrap();
            let moved = std::process::Command::new("/bin/mv")
                .arg(root.join("held").join("k"))
                .arg(outside.join("k"))
                .status();
            assert!(moved.unwrap().success());
            write(&root.join("held").join("moved"));
            let told = told_until(b"held/moved");
            assert!(told.iter().any(|p| p == b"held/k"), "{told:?}");
            write(&outside.join("k"));
            write(&root.join("held").join("after"));
            let told = told_until(b"held/after");
            assert!(!told.iter().any(|p| p == b"held/k"), "{told:?}");
        }
    }

    /// A path FSEvents tells is placed beneath the root it is within, and none outside it,
    /// one that only begins as the root does among them.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_change_is_placed_within_its_root() {
        assert_eq!(host::within(b"/a/b", b"/a/b/c/d"), Some(b"c/d".to_vec()));
        assert_eq!(host::within(b"/a/b", b"/a/b"), Some(Vec::new()));
        assert_eq!(host::within(b"/a/b", b"/a/bc"), None);
        assert_eq!(host::within(b"/a/b", b"/x"), None);
    }
}
