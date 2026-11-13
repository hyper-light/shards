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

use std::collections::{HashSet, VecDeque};
use std::io::{self, Write};
use std::sync::{Condvar, Mutex, PoisonError};

use super::server::Server;

/// The most changes waiting to be told: Linux's default queue of an inotify instance
/// (`fs.inotify.max_queued_events`, 16384; fs/notify/inotify/inotify_user.c), past which
/// the guest's own watcher would be told only that it lost events (`IN_Q_OVERFLOW`), so
/// that a change past it is told by its directory, which a watcher reads again whole.
pub const MOST: usize = 16384;

/// The changes waiting to be told, each share's paths, each once, in the order first seen;
/// each share named in its records as its place among its process's shares says
/// ([`Pending::new`]).
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

    /// Path `path` of share `share` changed: told once, however often it changes before it
    /// is; past [`MOST`] waiting, its nearest directory that is told or the share's root.
    pub fn tell(&self, share: u32, mut path: Vec<u8>) {
        let mut q = self.queued.lock().unwrap_or_else(PoisonError::into_inner);
        while q.order.len() >= MOST && !path.is_empty() && !q.seen.contains(&(share, path.clone())) {
            let cut = path.iter().rposition(|&b| b == b'/').unwrap_or(0);
            path.truncate(cut);
        }
        if q.seen.insert((share, path.clone())) {
            q.order.push_back((share, path));
            self.ready.notify_one();
        }
    }

    /// [`take`](Self::take), for no longer than `wait`: none where none came.
    #[cfg(test)]
    fn take_within(&self, wait: std::time::Duration) -> Option<Vec<u8>> {
        let q = self.queued.lock().unwrap_or_else(PoisonError::into_inner);
        let (q, _) = self
            .ready
            .wait_timeout_while(q, wait, |q| q.order.is_empty())
            .unwrap_or_else(PoisonError::into_inner);
        let empty = q.order.is_empty();
        drop(q);
        (!empty).then(|| self.take())
    }

    /// The changes waiting, taken, as records: none until there is one.
    fn take(&self) -> Vec<u8> {
        let mut q = self.queued.lock().unwrap_or_else(PoisonError::into_inner);
        while q.order.is_empty() {
            q = self.ready.wait(q).unwrap_or_else(PoisonError::into_inner);
        }
        q.seen.clear();
        let mut out = Vec::new();
        for (share, path) in q.order.drain(..) {
            let named = usize::try_from(share).ok().and_then(|i| self.names.get(i));
            if let Some(name) = named {
                shards_abi::run::notice::encode(name, &path, &mut out);
            }
        }
        out
    }

    /// Tells `out` each change as it comes, those that came while it was written taken
    /// together, until it refuses one: its reader has gone.
    pub fn send(&self, out: &mut impl Write) -> io::Result<()> {
        loop {
            let records = self.take();
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

/// The path of the k-th server's change at `path` that the guest is told, where it holds
/// the directory it is in, or it is that directory (D235).
#[cfg(not(target_os = "linux"))]
fn told(w: Watched, k: usize, path: Vec<u8>) {
    let (Some(server), Some(&share)) = (w.servers.get(k), w.shares.get(k)) else {
        return;
    };
    let dir = match path.iter().rposition(|&b| b == b'/') {
        Some(cut) => path.get(..cut).unwrap_or_default(),
        None => &[][..],
    };
    if server.holds_dir(dir) || server.holds_dir(&path) {
        w.pending.tell(share, path);
    }
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
                        for path in server.noticed(&event) {
                            w.pending.tell(share, path);
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
                            told(w, k, path);
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
            // None: more changed than the buffer held, each one lost; the root stands for
            // them, which a guest's watcher there reads again.
            if got == 0 {
                told(w, k, Vec::new());
                continue;
            }
            let bytes: Vec<u8> = buf.iter().flat_map(|word| word.to_ne_bytes()).collect();
            let bytes = bytes.get(..usize::try_from(got).unwrap_or(0)).unwrap_or_default();
            for path in changes(bytes) {
                told(w, k, path);
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

    /// A change is told once however often it changes before it is; past the most waiting,
    /// by its nearest directory waiting, else by its share's root, once; and records come
    /// of it.
    #[test]
    fn changes_are_told_once_and_within_bounds() {
        let pending = Pending::new(vec![b"shards0".to_vec(), b"shards-plug1".to_vec()]);
        pending.tell(0, b"a/b".to_vec());
        pending.tell(0, b"a/b".to_vec());
        pending.tell(1, b"a/b".to_vec());
        let records = pending.take();
        let mut want = Vec::new();
        shards_abi::run::notice::encode(b"shards0", b"a/b", &mut want);
        shards_abi::run::notice::encode(b"shards-plug1", b"a/b", &mut want);
        assert_eq!(records, want);
        for n in 0..MOST {
            pending.tell(0, format!("d/{n}").into_bytes());
        }
        pending.tell(0, b"d/7".to_vec());
        pending.tell(0, b"d/7/new".to_vec());
        pending.tell(0, b"d/new".to_vec());
        pending.tell(0, b"e/f/g".to_vec());
        pending.tell(1, b"e/f/g".to_vec());
        let mut paths = Vec::new();
        let records = pending.take();
        let mut rest = records.as_slice();
        while let shards_abi::run::notice::Decoded::Record { path, rest: r, .. } =
            shards_abi::run::notice::decode(rest)
        {
            paths.push(path.to_vec());
            rest = r;
        }
        assert_eq!(paths.len(), MOST + 2);
        // `d/7/new` stood for by `d/7`, waiting; `d/new` and `e/f/g` by the root, once a
        // share.
        assert_eq!(paths[MOST], Vec::<u8>::new());
        assert_eq!(paths[MOST + 1], Vec::<u8>::new());
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
                let records = pending.take_within(left).unwrap_or_default();
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
