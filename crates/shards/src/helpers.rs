//! The processes `shards` starts but is not: the VM process (`shards-vm`), one per
//! microVM, and the network process (`shards-net`), one per networked microVM (D36); and
//! where this build carries it, the GPU's VM process (`shards-vm-gpu`, D144), the VM
//! process of a microVM given a GPU.
//!
//! `shards` is the one binary there is to install. These two are carried inside it
//! (build.rs) and written out the first time a build of it needs them, into a directory
//! of this user's cache named by their digests, from which every later process of that
//! build starts them. They stay processes of their own binaries for two measured reasons:
//! - every VM maps the binary it runs, and a VM running all of `shards` would hold 1.2
//!   MiB more than one running its own (PM M34);
//! - on macOS the VM process runs in App Sandbox, whose entitlements are its binary's
//!   signature, which the daemon and the command must not share (D30).
//!
//! Each process that starts them holds its build's directory for as long as it lives, and
//! the first process of a build to write its directory out removes every other build's
//! that no process holds (D182): kept, they would grow the cache by a directory a build.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

mod carried {
    include!(concat!(env!("OUT_DIR"), "/helpers.rs"));
}

/// The lock file of the helpers' directory, and of each build's in it.
const LOCK: &str = "lock";

/// The VM process's binary, written out if this build's is not yet; or the one
/// `SHARDS_VM_BINARY` names, an absolute path, for a VMM being worked on or a tool
/// wrapped around the VM process (tests gate VMs' starts with one).
pub fn vm() -> Result<PathBuf, String> {
    if let Some(given) = given() {
        return given;
    }
    Ok(dir()?.join(format!("shards-vm{}", std::env::consts::EXE_SUFFIX)))
}

/// The VM process `SHARDS_VM_BINARY` names, an absolute path, if it names one.
fn given() -> Option<Result<PathBuf, String>> {
    let given = PathBuf::from(std::env::var_os("SHARDS_VM_BINARY").filter(|v| !v.is_empty())?);
    Some(if given.is_absolute() {
        Ok(given)
    } else {
        Err(format!(
            "SHARDS_VM_BINARY: {} is not an absolute path",
            given.display()
        ))
    })
}

/// The network process's binary, written out if this build's is not yet (D31, D142).
pub fn net() -> Result<PathBuf, String> {
    Ok(dir()?.join(format!("shards-net{}", std::env::consts::EXE_SUFFIX)))
}

/// The GPU's VM process's binary (D144), written out the first time a GPU VM needs it,
/// beside the others: none of its megabytes is written for a `shards` that runs no GPU.
/// `SHARDS_VM_BINARY` names one instead, as it does the VM process.
pub fn vm_gpu() -> Result<PathBuf, String> {
    if let Some(given) = given() {
        return given;
    }
    if carried::SHARDS_VM_GPU.is_empty() {
        return Err(
            "this shards was built without its GPU's VM process: scripts/gpu-libs builds the GPU's renderer, and SHARDS_GPU_LIBS names it to the build (D144)"
                .into(),
        );
    }
    let dir = dir()?;
    let name = format!("shards-vm-gpu{}", std::env::consts::EXE_SUFFIX);
    let path = dir.join(&name);
    let len = carried::SHARDS_VM_GPU.len() as u64;
    if fs::metadata(&path).is_ok_and(|m| m.len() == len) {
        return Ok(path);
    }
    // Written beside it, synced, then moved into place whole, as the others are.
    let temp = dir.join(format!(".{name}.{}", std::process::id()));
    let _ = fs::remove_file(&temp);
    let mut file = executable(&temp).map_err(|e| at(&temp, e))?;
    file.write_all(carried::SHARDS_VM_GPU).map_err(|e| at(&temp, e))?;
    file.sync_all().map_err(|e| at(&temp, e))?;
    fs::rename(&temp, &path).map_err(|e| at(&path, e))?;
    Ok(path)
}

/// What this build carries: each helper's name, bytes and SHA-256.
fn carried() -> [(&'static str, &'static [u8], &'static str); 2] {
    [
        ("shards-vm", carried::SHARDS_VM, carried::SHARDS_VM_SHA256),
        ("shards-net", carried::SHARDS_NET, carried::SHARDS_NET_SHA256),
    ]
}

/// This build's helpers' directory: written out by the first process that needs it, and
/// held by each that uses it for as long as it lives ([`hold`]).
fn dir() -> Result<PathBuf, String> {
    static DIR: OnceLock<Result<(PathBuf, Locked), String>> = OnceLock::new();
    DIR.get_or_init(|| {
        if !carried::PRESENT {
            return Err(
                "this shards was built without its VM and network processes (SHARDS_HELPERS=skip)".into(),
            );
        }
        let root = cache()?.join("helpers");
        // Named by every helper's digest, the GPU's VM process's among them, so that a
        // build of other ones never finds this build's.
        let id: String = carried()
            .iter()
            .map(|(_, _, sha)| *sha)
            .chain([carried::SHARDS_VM_GPU_SHA256])
            .flat_map(|sha| sha.get(..12).unwrap_or(sha).chars())
            .collect();
        let dir = root.join(&id);
        if complete(&dir)
            && let Some(held) = hold(&dir)?
        {
            return Ok((dir, held));
        }
        let (held, placed) = write_out(&root, &id)?;
        if placed {
            // Off this process's way: what a collection cut short leaves, the next removes.
            let _ = std::thread::Builder::new()
                .name("helpers".into())
                .spawn(move || collect(&root, &id));
        }
        Ok((dir, held))
    })
    .as_ref()
    .map(|(dir, _)| dir.clone())
    .map_err(Clone::clone)
}

/// Whether `dir` holds every helper at the size it was written: what the first process
/// of a build wrote, all of it, since a directory appears only once its files are synced.
fn complete(dir: &Path) -> bool {
    carried().iter().all(|(name, bytes, _)| {
        fs::metadata(dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
            .is_ok_and(|m| m.len() == bytes.len() as u64)
    })
}

/// A lock file, its lock (flock(2), LockFileEx on Windows) let go before it closes: a lock
/// is its open file's, which a program another thread starts meanwhile shares until its
/// exec closes it (PM M244).
struct Locked(fs::File);

impl Drop for Locked {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

impl std::ops::Deref for Locked {
    type Target = fs::File;

    fn deref(&self) -> &fs::File {
        &self.0
    }
}

/// `path`'s lock file, made empty where it is not there.
fn open_lock(path: &Path) -> io::Result<Locked> {
    Ok(Locked(
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?,
    ))
}

/// Holds `dir` for this process: its lock taken shared, which a collection, taking it
/// alone, cannot take while this process lives. None where a collection took it first.
fn hold(dir: &Path) -> Result<Option<Locked>, String> {
    let path = dir.join(LOCK);
    match open_lock(&path) {
        Ok(lock) => take(lock, dir),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(at(&path, e)),
    }
}

/// [`hold`]'s taking of `lock`, `dir`'s, opened: none where the directory was moved away
/// while this process waited for it, which its lock's mark says (or, where the disk had no
/// room for the mark, the files gone from where they were).
fn take(lock: Locked, dir: &Path) -> Result<Option<Locked>, String> {
    let path = dir.join(LOCK);
    lock.lock_shared().map_err(|e| at(&path, e))?;
    let moved = lock.metadata().map_err(|e| at(&path, e))?.len() > 0;
    Ok((!moved && complete(dir)).then_some(lock))
}

/// Writes the helpers into `root/id` where they are not there whole, and holds them; true
/// where this process's copy was placed. Writing takes the helpers' lock shared, and a
/// collection takes it alone, so that none looks at a directory being written, or at one
/// placed but not yet held.
fn write_out(root: &Path, id: &str) -> Result<(Locked, bool), String> {
    private_dir(root)?;
    let path = root.join(LOCK);
    let writing = open_lock(&path).map_err(|e| at(&path, e))?;
    writing.lock_shared().map_err(|e| at(&path, e))?;
    let dir = root.join(id);
    let placed = !complete(&dir) && place(root, id)?;
    let held = hold(&dir)?.ok_or_else(|| format!("{}: removed as it was written", dir.display()))?;
    drop(writing);
    Ok((held, placed))
}

/// Writes the helpers into a directory of this process's, each file synced, then moves it
/// into place whole as `root/id`; false where another process placed them meanwhile, whose
/// copy serves. A directory left incomplete (by a crash after a partial move, which a
/// rename cannot leave, or by hand) is replaced.
fn place(root: &Path, id: &str) -> Result<bool, String> {
    let dir = root.join(id);
    let temp = root.join(format!(".{id}.{}", std::process::id()));
    let _ = fs::remove_dir_all(&temp);
    private_dir(&temp)?;
    // Its lock, there as the directory is.
    let lock = temp.join(LOCK);
    fs::File::create(&lock).map_err(|e| at(&lock, e))?;
    for (name, bytes, _) in carried() {
        let path = temp.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        let mut file = executable(&path).map_err(|e| at(&path, e))?;
        file.write_all(bytes).map_err(|e| at(&path, e))?;
        file.sync_all().map_err(|e| at(&path, e))?;
    }
    match fs::rename(&temp, &dir) {
        Ok(()) => Ok(true),
        Err(_) if complete(&dir) => {
            let _ = fs::remove_dir_all(&temp);
            Ok(false)
        }
        Err(_) => {
            // There, but not whole: replaced.
            let _ = fs::remove_dir_all(&dir);
            let moved = fs::rename(&temp, &dir).map_err(|e| at(&dir, e));
            if moved.is_err() {
                let _ = fs::remove_dir_all(&temp);
            }
            moved.map(|()| true)
        }
    }
}

/// Removes every other build's directory that no process holds, and what a process that
/// ended while writing one or removing one left (D182), holding the helpers' lock alone so
/// that none is being written meanwhile. A directory whose lock this takes alone is moved
/// aside while that is held, then its lock marked, so that a process waiting for the lock
/// meanwhile finds it moved ([`take`]), then removed. Windows moves no directory with a
/// file open in it (as the image store's `take_away` finds): there the move is refused
/// while this holds the lock, so the lock is let go and the move tried again, which fails
/// while any process has the lock, or any file of the directory, open. What stays waits
/// for the next build's collection. A directory of a build from before these locks has
/// none, and goes as no process's: a daemon of such a build is stopped by the first client
/// of another build that finds it.
fn collect(root: &Path, id: &str) {
    let path = root.join(LOCK);
    let Ok(alone) = open_lock(&path) else {
        return;
    };
    if alone.lock().is_err() {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_str() == Some(id) || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let dir = entry.path();
        if name.to_string_lossy().starts_with('.') {
            // A copy whose process ended before moving it into place, or a directory a
            // collection that ended had moved aside.
            let _ = fs::remove_dir_all(&dir);
            continue;
        }
        let Ok(lock) = open_lock(&dir.join(LOCK)) else {
            continue;
        };
        if !matches!(lock.try_lock(), Ok(())) {
            // Held: a process of its build lives.
            continue;
        }
        let mut aside = std::ffi::OsString::from(".");
        aside.push(&name);
        aside.push(format!("-{}", std::process::id()));
        let aside = root.join(aside);
        let moved = match fs::rename(&dir, &aside) {
            Ok(()) => {
                let _ = lock.set_len(1);
                true
            }
            #[cfg(windows)]
            Err(_) => {
                drop(lock);
                fs::rename(&dir, &aside).is_ok()
            }
            #[cfg(not(windows))]
            Err(_) => false,
        };
        if moved {
            let _ = fs::remove_dir_all(&aside);
        }
    }
    drop(alone);
}

fn at(path: &Path, e: io::Error) -> String {
    format!("{}: {e}", path.display())
}

/// A new file at `path`, only this user's to read, write and run.
fn executable(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o700);
    }
    options.open(path)
}

/// `path`, made if it is not there, only this user's.
fn private_dir(path: &Path) -> Result<(), String> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Where the AppContainer profiles of this user's microVMs are recorded as they live, so
/// that one whose `shards` ended without deleting it is deleted by the next (D141).
#[cfg(windows)]
pub fn containers() -> Result<PathBuf, String> {
    let dir = cache()?.join("containers");
    private_dir(&dir)?;
    Ok(dir)
}

/// This user's cache: `SHARDS_CACHE` where it names a directory, else `~/Library/Caches/shards`
/// on macOS, `$XDG_CACHE_HOME/shards` or `~/.cache/shards` elsewhere on Unix,
/// `%LOCALAPPDATA%\shards` on Windows. A command a client of another machine runs there is
/// given that machine's daemon's (D180), as it is given no `HOME` of the owner's.
pub(crate) fn cache() -> Result<PathBuf, String> {
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if let Some(given) = var("SHARDS_CACHE").filter(|p| p.is_absolute()) {
        return Ok(given);
    }
    let base = if cfg!(target_os = "macos") {
        var("HOME").map(|h| h.join("Library").join("Caches"))
    } else if cfg!(windows) {
        var("LOCALAPPDATA")
    } else {
        var("XDG_CACHE_HOME")
            .filter(|p| p.is_absolute())
            .or_else(|| var("HOME").map(|h| h.join(".cache")))
    };
    base.map(|b| b.join("shards"))
        .ok_or_else(|| "no cache directory: HOME is not set".to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn names(root: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn helpers_are_written_whole_once_and_found_after() {
        if !carried::PRESENT {
            return;
        }
        let root_dir = shards_testdir::TempDir::new("helpers").unwrap();
        let root = root_dir.join("helpers");
        let (held, placed) = write_out(&root, "test").unwrap();
        assert!(placed);
        let dir = root.join("test");
        assert!(complete(&dir));
        for (name, bytes, _) in carried() {
            assert_eq!(
                fs::read(dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))).unwrap(),
                bytes
            );
        }
        // Again: the copy there serves, and no temporary directory is left.
        let (again, placed) = write_out(&root, "test").unwrap();
        assert!(!placed);
        assert_eq!(names(&root), ["lock", "test"]);
        // A copy cut short is replaced (Windows removes no file open in it: let go first).
        drop((held, again));
        let vm = dir.join(format!("shards-vm{}", std::env::consts::EXE_SUFFIX));
        fs::write(&vm, b"cut").unwrap();
        assert!(!complete(&dir));
        let (_held, placed) = write_out(&root, "test").unwrap();
        assert!(placed);
        assert!(complete(&dir));
    }

    #[test]
    fn other_builds_helpers_go_once_no_process_holds_them() {
        if !carried::PRESENT {
            return;
        }
        let root_dir = shards_testdir::TempDir::new("helpers").unwrap();
        let root = root_dir.join("helpers");
        private_dir(&root).unwrap();
        // A build a process holds, one none does, one a process has opened the lock of but
        // not yet taken, one of a build from before the locks, and a copy whose process
        // ended before it was moved into place.
        for id in ["held", "free", "waiting"] {
            assert!(place(&root, id).unwrap());
        }
        let held = hold(&root.join("held")).unwrap().unwrap();
        let waiting = open_lock(&root.join("waiting").join(LOCK)).unwrap();
        fs::create_dir(root.join("before")).unwrap();
        fs::write(root.join("before").join("shards-vm"), b"old").unwrap();
        fs::create_dir(root.join(".cut.1")).unwrap();
        let (_new, placed) = write_out(&root, "new").unwrap();
        assert!(placed);
        // A collection waits for what is being written: no copy is taken for a dead one's.
        let writing = open_lock(&root.join(LOCK)).unwrap();
        writing.lock_shared().unwrap();
        let collecting = std::thread::spawn({
            let root = root.clone();
            move || collect(&root, "new")
        });
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(root.join(".cut.1").exists(), "collected while written");
        drop(writing);
        collecting.join().unwrap();
        // Unix moves a directory with a file open in it; Windows moves none, and the waiting
        // process holds it as it was.
        if cfg!(windows) {
            assert_eq!(names(&root), ["held", "lock", "new", "waiting"]);
        } else {
            assert_eq!(names(&root), ["held", "lock", "new"]);
        }
        // Placed again meanwhile, by another process of its build: the process that waited
        // finds its lock marked, not a directory of which it holds no lock.
        place(&root, "waiting").unwrap();
        let waited = take(waiting, &root.join("waiting")).unwrap();
        assert_eq!(waited.is_some(), cfg!(windows));
        assert!(complete(&root.join("held")));
        // Let go, they go with the next build's collection.
        drop((held, waited));
        collect(&root, "new");
        assert_eq!(names(&root), ["lock", "new"]);
    }
}
