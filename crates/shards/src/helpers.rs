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
use std::sync::{Mutex, OnceLock};

use shards_vmm::platform::FileIdentity;

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
    ours("shards-vm", carried::SHARDS_VM, carried::SHARDS_VM_SHA256)
}

/// A helper about to start ([`program`]): this build's, its bytes checked to be those it
/// carries, held open until it has started; or another's, as it is.
#[derive(Debug)]
pub struct Program {
    path: PathBuf,
    /// The file checked, open until it has started: on Linux what it starts through, on
    /// Windows what keeps others from writing, renaming or deleting it ([`Program::path`]).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    held: Option<fs::File>,
}

impl Program {
    /// What to start. On Linux a helper checked is started through its own descriptor,
    /// `/proc/PID/fd/N` of this process, which the process that starts it (this one, or its
    /// spawner, PM M158: of the same user, which proc(5) lets open it) opens as the very file
    /// checked, whatever has been renamed over its name since; its name where /proc is not
    /// there. macOS has no fexecve(2), and Windows' CreateProcess takes a name: there it is
    /// its name, the file held open meanwhile, which on Windows no other handle may write,
    /// rename or delete ([`open_checked`]); on macOS nothing holds its name.
    pub fn path(&self) -> PathBuf {
        #[cfg(target_os = "linux")]
        if let Some(file) = &self.held {
            use std::os::fd::AsRawFd as _;
            let own = PathBuf::from(format!("/proc/{}/fd/{}", std::process::id(), file.as_raw_fd()));
            if fs::symlink_metadata(&own).is_ok() {
                return own;
            }
        }
        self.path.clone()
    }

    /// Its name, as what is said of it says it.
    pub fn shown(&self) -> std::path::Display<'_> {
        self.path.display()
    }
}

/// The helper at `path`, which [`vm`], [`net`] or [`vm_gpu`] gave, as a process is about to
/// start it: this build's own checked to hold the bytes it carries ([`checked`]), and held
/// open; any other (`SHARDS_VM_BINARY`'s, a carrier's beside it, D237) as it is, its bytes
/// its giver's to answer for.
pub fn program(path: &Path) -> Result<Program, String> {
    let ours = match DIR.get() {
        Some(Ok((dir, _))) if !carried::BESIDE => all_carried()
            .into_iter()
            .find(|(name, _, _)| *path == dir.join(exe(name)))
            .map(|carried| (dir, carried)),
        _ => None,
    };
    match ours {
        Some((dir, (name, bytes, sha))) => checked(dir, &exe(name), bytes, sha),
        None => Ok(Program {
            path: path.to_path_buf(),
            held: None,
        }),
    }
}

/// `name`, a helper's, as this platform names its program.
fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// The helper `name` of this build's directory, its bytes `bytes`, whose SHA-256 is `sha`:
/// checked ([`checked`]); or, for a carried `shards` (D237), the one beside it.
fn ours(name: &str, bytes: &[u8], sha: &str) -> Result<PathBuf, String> {
    let dir = dir()?;
    if carried::BESIDE {
        return Ok(dir.join(exe(name)));
    }
    checked(&dir, &exe(name), bytes, sha).map(|program| program.path)
}

/// Each helper this process checked, by path, and which version of its file it found to
/// hold this build's bytes: one a helper, so at most as many as this build carries. A
/// version is the file's identity and times (`platform::file_identity`), which any write to
/// it or any file renamed over it changes, so that its bytes are read again then and only
/// then.
static CHECKED: Mutex<Vec<(PathBuf, FileIdentity)>> = Mutex::new(Vec::new());

/// `name` in `dir`, checked as a process is about to start it, and held open: its bytes'
/// SHA-256 is `sha`, this build's digest of `bytes` (D36). Its length says nothing of its
/// bytes, and they can change after they were written whole (by hand, by a program of this
/// user's, on the disk), so they are read whole as this process first starts it and again
/// whenever its file's version has changed since (runs-10); else its version alone is read.
/// Where they differ, or it is not there, it is written again whole from `bytes`
/// ([`publish`]) and checked again; one that cannot be written, or written is still not
/// this build's, is refused with why.
fn checked(dir: &Path, name: &str, bytes: &[u8], sha: &str) -> Result<Program, String> {
    let path = dir.join(name);
    let refused = |why: String| {
        format!(
            "{}: not this build's bytes (SHA-256 {sha}), and not written again: {why}",
            path.display()
        )
    };
    if let Some(held) = open_checked(&path, sha).map_err(|e| at(&path, e))? {
        return Ok(Program {
            path,
            held: Some(held),
        });
    }
    publish(dir, name, bytes).map_err(refused)?;
    match open_checked(&path, sha).map_err(|e| at(&path, e))? {
        Some(held) => Ok(Program {
            path,
            held: Some(held),
        }),
        None => Err(refused("written again, it reads as other bytes".into())),
    }
}

/// The file at `path`, open, where it holds the bytes whose SHA-256 is `sha`: read whole
/// unless [`CHECKED`] has its version, and kept there where they are and its version did not
/// change as they were read. None where it is not there or holds others. On Windows it is
/// opened sharing only reads, so that while it is held no other handle writes, renames or
/// deletes it (CreateFileW's dwShareMode), which its start's own open, for reading and
/// executing, shares.
fn open_checked(path: &Path, sha: &str) -> io::Result<Option<fs::File>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
        options.share_mode(FILE_SHARE_READ);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let version = shards_vmm::platform::file_identity(&file)?;
    let known = |checked: &[(PathBuf, FileIdentity)]| checked.iter().any(|(p, v)| p == path && *v == version);
    if known(&lock_checked()) {
        return Ok(Some(file));
    }
    if digest(&file)? != sha || shards_vmm::platform::file_identity(&file)? != version {
        return Ok(None);
    }
    let mut checked = lock_checked();
    checked.retain(|(p, _)| p != path);
    checked.push((path.to_path_buf(), version));
    Ok(Some(file))
}

fn lock_checked() -> std::sync::MutexGuard<'static, Vec<(PathBuf, FileIdentity)>> {
    CHECKED.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The SHA-256 of the bytes `file` reads, from its start, in lowercase hex as build.rs writes
/// the carried digests.
fn digest(mut file: &fs::File) -> io::Result<String> {
    use sha2::Digest as _;
    use std::io::{BufRead as _, Seek as _};
    file.rewind()?;
    // Read through std's own buffer, its size std's choice.
    let mut file = io::BufReader::new(file);
    let mut hasher = sha2::Sha256::new();
    loop {
        let n = match file.fill_buf() {
            Ok([]) => break,
            Ok(buf) => {
                hasher.update(buf);
                buf.len()
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        file.consume(n);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
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
    ours("shards-net", carried::SHARDS_NET, carried::SHARDS_NET_SHA256)
}

/// The GPU's VM process's binary (D144), written out the first time a GPU VM needs it,
/// beside the others: none of its megabytes is written for a `shards` that runs no GPU.
/// `SHARDS_VM_BINARY` names one instead, as it does the VM process.
pub fn vm_gpu() -> Result<PathBuf, String> {
    if let Some(given) = given() {
        return given;
    }
    if carried::BESIDE {
        let path = dir()?.join(format!("shards-vm-gpu{}", std::env::consts::EXE_SUFFIX));
        return if path.is_file() {
            Ok(path)
        } else {
            Err(format!(
                "this shards starts the VM processes beside it, and the program carrying it wrote none for a GPU there: {} is not there",
                path.display()
            ))
        };
    }
    if carried::SHARDS_VM_GPU.is_empty() {
        return Err(
            "this shards was built without its GPU's VM process: scripts/gpu-libs builds the GPU's renderer, and SHARDS_GPU_LIBS names it to the build (D144)"
                .into(),
        );
    }
    // musl's GPU's VM process links the host's musl dynamically, to open its Vulkan driver
    // (D201): a host of glibc's has no musl loader to start it, and exec(2) would say only
    // that a file it has is not there.
    if cfg!(all(target_os = "linux", target_env = "musl")) {
        let loader = format!("/lib/ld-musl-{}.so.1", std::env::consts::ARCH);
        if !std::path::Path::new(&loader).exists() {
            return Err(format!(
                "this shards is built for musl, and its GPU's VM process opens the host's GPU driver through musl's loader, {loader}, which this host has not: on a host of glibc's, shards' glibc build gives a run its GPU (D201)"
            ));
        }
    }
    let dir = dir()?;
    checked(
        &dir,
        &exe("shards-vm-gpu"),
        carried::SHARDS_VM_GPU,
        carried::SHARDS_VM_GPU_SHA256,
    )
    .map(|program| program.path)
}

/// Writes `bytes` to `name` in `dir` whole: to a staging file of this writer's own, synced
/// and closed, then renamed over `name`, so that a process starting it finds the old file or
/// the new, never a part, and no other writer's staging is touched.
fn publish(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let path = dir.join(name);
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    // As write_in's staging names: identity fields discriminate writers; create_new
    // proves ownership. Even counter wrap or a collision never unlinks another file.
    let temp = dir.join(format!(
        ".{name}.{}.{nanos:x}.{:x}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = executable(&temp).map_err(|e| at(&temp, e))?;
    #[cfg(test)]
    pause_gpu(GpuStage::Created);
    let written = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| at(&temp, e));
    // A published executable has no writable handle left (exec can otherwise fail
    // ETXTBSY). Another caller may start it as soon as rename makes it visible.
    drop(file);
    let placed = written.and_then(|()| {
        #[cfg(test)]
        pause_gpu(GpuStage::Publish);
        fs::rename(&temp, &path).map_err(|e| at(&path, e))
    });
    if placed.is_err() {
        let _ = fs::remove_file(&temp);
    }
    placed?;
    Ok(path)
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum GpuStage {
    Created,
    Publish,
}

#[cfg(test)]
type GpuPause = (
    GpuStage,
    std::sync::mpsc::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

#[cfg(test)]
std::thread_local! {
    static GPU_PAUSE: std::cell::RefCell<Option<GpuPause>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
fn pause_gpu(stage: GpuStage) {
    GPU_PAUSE.with(|pause| {
        if pause.borrow().as_ref().is_some_and(|p| p.0 == stage) {
            let (_, ready, resume) = pause.borrow_mut().take().unwrap();
            ready.send(()).unwrap();
            resume.recv().unwrap();
        }
    });
}

/// What this build carries: each helper's name, bytes and SHA-256.
fn carried() -> [(&'static str, &'static [u8], &'static str); 2] {
    [
        ("shards-vm", carried::SHARDS_VM, carried::SHARDS_VM_SHA256),
        ("shards-net", carried::SHARDS_NET, carried::SHARDS_NET_SHA256),
    ]
}

/// [`carried`], and the GPU's VM process, written out only as a GPU VM needs it.
fn all_carried() -> [(&'static str, &'static [u8], &'static str); 3] {
    let [vm, net] = carried();
    let gpu = (
        "shards-vm-gpu",
        carried::SHARDS_VM_GPU,
        carried::SHARDS_VM_GPU_SHA256,
    );
    [vm, net, gpu]
}

/// The directory of `exe`, a `shards` built to be carried (`SHARDS_HELPERS=beside`, D237),
/// where the program carrying it wrote the VM and network processes out beside it: that
/// program owns the directory's life and the helpers' signatures, so nothing here writes,
/// holds or collects it.
fn beside(exe: &Path) -> Result<PathBuf, String> {
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{}: no directory holds it", exe.display()))?;
    for (name, _, _) in carried() {
        let helper = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if !helper.is_file() {
            return Err(format!(
                "this shards starts the VM and network processes beside it, which the program carrying it writes out there (SHARDS_HELPERS=beside): {} is not there",
                helper.display()
            ));
        }
    }
    Ok(dir.to_path_buf())
}

/// This process's [`dir`], found or written out the first time it is needed.
static DIR: OnceLock<Result<(PathBuf, Option<Locked>), String>> = OnceLock::new();

/// This build's helpers' directory: written out by the first process that needs it, and
/// held by each that uses it for as long as it lives ([`hold`]); or, for a `shards` built to
/// be carried (`SHARDS_HELPERS=beside`), the directory it was started from, where the program
/// carrying it wrote them out ([`beside`]).
fn dir() -> Result<PathBuf, String> {
    DIR.get_or_init(|| {
        if carried::BESIDE {
            let exe = std::env::current_exe().map_err(|e| format!("this shards' own path: {e}"))?;
            return beside(&exe).map(|dir| (dir, None));
        }
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
            return Ok((dir, Some(held)));
        }
        let (held, placed) = write_out(&root, &id)?;
        if placed {
            // Off this process's way: what a collection cut short leaves, the next removes.
            let _ = std::thread::Builder::new()
                .name("helpers".into())
                .spawn(move || collect(&root, &id));
        }
        Ok((dir, Some(held)))
    })
    .as_ref()
    .map(|(dir, _)| dir.clone())
    .map_err(Clone::clone)
}

/// Whether `dir` holds every helper at the size it was written: what the first process
/// of a build wrote, all of it, since a directory appears only once its files are synced.
/// Whether their bytes are still this build's is checked as each is started ([`checked`]).
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

    #[test]
    fn concurrent_gpu_writers_publish_their_own_complete_files() {
        use std::sync::mpsc::channel;
        use std::time::Duration;
        let dir = shards_testdir::TempDir::new("gpupub").unwrap();
        let dir = dir.path();
        let bytes = b"the complete helper bytes";
        let bound = Duration::from_secs(10);
        let (a_ready, a_seen) = channel();
        let (a_resume, a_wait) = channel();
        let (a_done, a_result) = channel();
        let (b_ready, b_seen) = channel();
        let (b_resume, b_wait) = channel();
        let (b_done, b_result) = channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                GPU_PAUSE.with(|p| *p.borrow_mut() = Some((GpuStage::Publish, a_ready, a_wait)));
                a_done.send(publish(dir, "gpu", bytes)).unwrap();
            });
            a_seen.recv_timeout(bound).unwrap();
            scope.spawn(|| {
                GPU_PAUSE.with(|p| *p.borrow_mut() = Some((GpuStage::Created, b_ready, b_wait)));
                b_done.send(publish(dir, "gpu", bytes)).unwrap();
            });
            let created = b_seen.recv_timeout(bound);
            a_resume.send(()).unwrap();
            let first = a_result.recv_timeout(bound);
            let observed = first.as_ref().ok().and_then(|r| r.as_ref().ok()).map(fs::read);
            // Release every paused actor before asserting, including the red's failures.
            let _ = b_resume.send(());
            let second = b_result.recv_timeout(bound);
            assert!(
                created.is_ok(),
                "the second writer never created its staging file"
            );
            assert_eq!(observed.unwrap().unwrap(), bytes);
            assert!(first.unwrap().is_ok());
            assert!(second.unwrap().is_ok());
        });
    }

    /// The SHA-256 build.rs gives `bytes`.
    fn sha(bytes: &[u8]) -> String {
        use sha2::Digest as _;
        sha2::Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// A helper whose bytes changed after they were written, its length kept, is not
    /// started as it is: it is written again whole from this build's (runs-10).
    #[test]
    fn a_helper_whose_bytes_changed_is_written_again_before_it_starts() {
        let dir = shards_testdir::TempDir::new("helper-sha").unwrap();
        let bytes = b"this build's helper";
        let path = dir.join("helper");
        assert_eq!(checked(&dir, "helper", bytes, &sha(bytes)).unwrap().path, path);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        let mut flipped = bytes.to_vec();
        flipped[0] ^= 1;
        fs::write(&path, &flipped).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), bytes.len() as u64);
        assert_eq!(checked(&dir, "helper", bytes, &sha(bytes)).unwrap().path, path);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    /// One that cannot be written again is refused, saying so, never started as it is.
    #[cfg(unix)]
    #[test]
    fn a_changed_helper_that_cannot_be_written_again_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = shards_testdir::TempDir::new("helper-ro").unwrap();
        let bytes = b"this build's helper";
        fs::write(dir.join("helper"), b"this build's helpeR").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let refused = checked(&dir, "helper", bytes, &sha(bytes));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        let refused = refused.unwrap_err();
        assert!(refused.contains("not this build's bytes"), "{refused}");
        assert_eq!(fs::read(dir.join("helper")).unwrap(), b"this build's helpeR");
    }

    /// A helper's bytes are read once for each version of its file: a start that finds the
    /// version it checked reads none of them, and one whose file was written since, its
    /// length kept, reads them again and writes this build's back (runs-10).
    #[test]
    fn a_helper_is_read_again_only_once_its_file_changed() {
        let dir = shards_testdir::TempDir::new("helper-version").unwrap();
        let bytes = b"this build's helper";
        let path = dir.join("helper");
        let first = checked(&dir, "helper", bytes, &sha(bytes)).unwrap();
        let version = shards_vmm::platform::file_identity(first.held.as_ref().unwrap()).unwrap();
        drop(first);
        assert!(lock_checked().contains(&(path.clone(), version)));
        // Its version known: started with no read of its bytes, which a digest that is not
        // its bytes' shows.
        assert!(open_checked(&path, "not its digest").unwrap().is_some());
        let mut flipped = bytes.to_vec();
        flipped[0] ^= 1;
        fs::write(&path, &flipped).unwrap();
        assert!(open_checked(&path, &sha(bytes)).unwrap().is_none());
        drop(checked(&dir, "helper", bytes, &sha(bytes)).unwrap());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    /// A helper checked starts as the file checked: on Linux through its descriptor, even
    /// once another file is renamed over its name; on Windows its name is held, and nothing
    /// is renamed over it; on macOS, which has neither, its name (D36's gap).
    #[test]
    fn a_checked_helper_starts_as_the_file_checked() {
        let dir = shards_testdir::TempDir::new("helper-held").unwrap();
        let (good, other): (&str, &str) = if cfg!(windows) {
            (
                r"C:\Windows\System32\hostname.exe",
                r"C:\Windows\System32\whoami.exe",
            )
        } else if Path::new("/usr/bin/true").exists() {
            ("/usr/bin/true", "/usr/bin/false")
        } else {
            ("/bin/true", "/bin/false")
        };
        let bytes = fs::read(good).unwrap();
        let program = checked(&dir, "tool", &bytes, &sha(&bytes)).unwrap();
        let swapped = dir.join("swapped");
        fs::copy(other, &swapped).unwrap();
        let renamed = fs::rename(&swapped, dir.join("tool"));
        let status = std::process::Command::new(program.path())
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        if cfg!(windows) {
            assert!(renamed.is_err(), "a file was renamed over a held helper");
            assert!(status.success());
        } else if cfg!(target_os = "linux") {
            renamed.unwrap();
            assert!(status.success(), "started the file renamed over it");
        } else {
            renamed.unwrap();
            assert!(!status.success());
        }
    }

    /// What a start's check of a helper costs, a version known and its bytes read, as
    /// `SHARDS_HELPER_COST_BYTES` bytes (a helper's size: shards-vm's, as its cache holds it)
    /// say: n, p50, p90, p99 and max of each. A measurement, run alone:
    /// `cargo test --profile test-release -p shards --bin shards a_helper_checks_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn a_helper_checks_cost() {
        let size: usize = std::env::var("SHARDS_HELPER_COST_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| fs::metadata(vm().unwrap()).unwrap().len() as usize);
        let dir = shards_testdir::TempDir::new("helper-cost").unwrap();
        let bytes: Vec<u8> = (0..size).map(|i| (i as u8).wrapping_mul(31)).collect();
        let want = sha(&bytes);
        let path = dir.join("helper");
        drop(checked(&dir, "helper", &bytes, &want).unwrap());
        let n = 200;
        let time = |f: &dyn Fn()| {
            let mut took: Vec<std::time::Duration> = (0..n)
                .map(|_| {
                    let t0 = std::time::Instant::now();
                    f();
                    t0.elapsed()
                })
                .collect();
            took.sort();
            let at = |q: f64| took[((n as f64 * q) as usize).min(n - 1)];
            format!(
                "n {n} p50 {:?} p90 {:?} p99 {:?} max {:?}",
                at(0.5),
                at(0.9),
                at(0.99),
                took[n - 1]
            )
        };
        let known = time(&|| assert!(open_checked(&path, &want).unwrap().is_some()));
        let read = time(&|| {
            let file = fs::File::open(&path).unwrap();
            assert_eq!(digest(&file).unwrap(), want);
        });
        println!("helper check, {size} bytes: version known: {known}; bytes read: {read}");
    }

    fn names(root: &Path) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// A carried `shards` (SHARDS_HELPERS=beside, D237) starts the helpers in the directory
    /// it was started from, both there, and refuses naming the one missing; it writes
    /// nothing there.
    #[test]
    fn a_carried_shards_starts_the_helpers_beside_it() {
        let root = shards_testdir::TempDir::new("beside").unwrap();
        let exe = root.join(format!("shards{}", std::env::consts::EXE_SUFFIX));
        fs::write(&exe, b"shards").unwrap();
        let vm = root.join(format!("shards-vm{}", std::env::consts::EXE_SUFFIX));
        let net = root.join(format!("shards-net{}", std::env::consts::EXE_SUFFIX));
        fs::write(&vm, b"vm").unwrap();
        let missing = beside(&exe).unwrap_err();
        assert!(missing.contains(&net.display().to_string()), "{missing}");
        fs::write(&net, b"net").unwrap();
        assert_eq!(beside(&exe).unwrap(), root.to_path_buf());
        let mut names = names(&root);
        names.retain(|n| !n.starts_with('.'));
        // The three alone, in the listing's order: on Windows `shards-net.exe` sorts before
        // `shards.exe`, as `-` comes before `.`.
        let exe_suffix = std::env::consts::EXE_SUFFIX;
        let mut alone = [
            format!("shards{exe_suffix}"),
            format!("shards-net{exe_suffix}"),
            format!("shards-vm{exe_suffix}"),
        ];
        alone.sort();
        assert_eq!(names, alone);
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
