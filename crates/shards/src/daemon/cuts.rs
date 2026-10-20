//! The files its microVMs' DAX windows map, watched for other processes cutting them short
//! (D189). A guest that has not learned a file's new size reads past its end through its
//! window, where the host has no page: x86 KVM retries that access without end, its vCPU
//! spinning in KVM_RUN (PM M273). So each VM names the files its windows map
//! (`kind::WATCH`), and the daemon watches them with its one inotify instance, as an
//! instance a VM would run the user out of them (PM M257), and tells a VM of one cut short
//! (`kind::CUT`), whose window then gives zeros where the file has no pages.

use std::collections::HashMap;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, OnceLock, PoisonError};

use shards_ipc::kind;

use super::{RunSocket, lock, log};

/// A file runs' windows map: one descriptor of it, however many map it.
struct Watched {
    file: File,
    /// Its size as last looked at: a write that grows it cuts nothing.
    size: u64,
    by: Vec<Watcher>,
}

/// A run whose window maps a watched file: its share's place among its VM's shares, and the
/// node of that share's server whose file it is, as its VM named it.
struct Watcher {
    run: String,
    socket: Arc<RunSocket>,
    share: u32,
    node: u64,
}

/// The files watched, by their watch's number, one a file.
static WATCHED: LazyLock<Mutex<HashMap<i32, Watched>>> = LazyLock::new(Default::default);

/// The watches written since [`look`] last took them, and its wake.
static FIRED: Mutex<Vec<i32>> = Mutex::new(Vec::new());
static WAKE: Condvar = Condvar::new();

/// A watched file was written, or cut short: called on the inotify instance's reader
/// thread, under its lock, so only noted.
fn fired(watched: i32) {
    lock(&FIRED).push(watched);
    WAKE.notify_one();
}

/// Whether the thread that looks at the files written runs, started the first time.
fn looking() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("cuts".into())
            .spawn(look)
            .is_ok()
    })
}

/// Tells each run whose watched file was cut short since it was last looked at: its VM's
/// window gives zeros where the file has no pages, and a vCPU that spun there runs on.
fn look() {
    loop {
        let mut fired = std::mem::take(&mut *wait_fired());
        fired.sort_unstable();
        fired.dedup();
        let mut cut = Vec::new();
        {
            let mut watched = lock(&WATCHED);
            for w in fired {
                let Some(f) = watched.get_mut(&w) else {
                    continue;
                };
                let Ok(size) = f.file.metadata().map(|m| m.len()) else {
                    continue;
                };
                if size < f.size {
                    cut.extend(
                        f.by.iter()
                            .map(|b| (b.run.clone(), b.socket.clone(), said(b.share, b.node))),
                    );
                }
                f.size = size;
            }
        }
        for (run, socket, said) in cut {
            tell(&run, &socket, &said);
        }
    }
}

fn wait_fired() -> MutexGuard<'static, Vec<i32>> {
    let mut fired = lock(&FIRED);
    while fired.is_empty() {
        fired = WAKE.wait(fired).unwrap_or_else(PoisonError::into_inner);
    }
    fired
}

/// A share's place and a node, as `kind::WATCH`, `kind::UNWATCH` and `kind::CUT` say them.
fn said(share: u32, node: u64) -> Vec<u8> {
    let mut said = share.to_be_bytes().to_vec();
    said.extend_from_slice(&node.to_be_bytes());
    said
}

/// The share's place and the node `payload` names, and what follows them.
fn named(payload: &[u8]) -> Option<(u32, u64, &[u8])> {
    let share = u32::from_be_bytes(*payload.first_chunk::<4>()?);
    let rest = payload.get(4..)?;
    let node = u64::from_be_bytes(*rest.first_chunk::<8>()?);
    Some((share, node, rest.get(8..)?))
}

fn tell(run: &str, socket: &RunSocket, said: &[u8]) {
    if let Err(e) = socket.send(kind::CUT, said, &[]) {
        log(format!(
            "container {run}: telling its microVM of a file cut short: {e}"
        ));
    }
}

/// Run `run`'s VM, on `socket`, has begun to map the file it sends with `payload`
/// (`kind::WATCH`): watched until it says it maps it no more, or it ends. A file cut short
/// between its mapping and its watch is told at once.
pub(super) fn watch(run: &str, socket: &Arc<RunSocket>, payload: &[u8], fds: Vec<OwnedFd>) {
    let (Some((share, node, rest)), Some(fd)) = (named(payload), fds.into_iter().next()) else {
        log(format!(
            "container {run}: a malformed watch of a file its microVM maps"
        ));
        return;
    };
    let Some(size) = rest.first_chunk::<8>().map(|s| u64::from_be_bytes(*s)) else {
        log(format!(
            "container {run}: a watch of a file its microVM maps names no size"
        ));
        return;
    };
    let file = File::from(fd);
    if !looking() {
        log(format!(
            "container {run}: no thread to watch the files its microVM maps"
        ));
        return;
    }
    let watch = match shards_vmm::platform::watch_writes(&file, fired) {
        Ok(watch) => watch,
        Err(e) => {
            log(format!("container {run}: watching a file its microVM maps: {e}"));
            return;
        }
    };
    let now = file.metadata().map_or(size, |m| m.len());
    let by = Watcher {
        run: run.to_string(),
        socket: socket.clone(),
        share,
        node,
    };
    // A file watched already keeps the descriptor it has; this one closes.
    lock(&WATCHED)
        .entry(watch)
        .or_insert_with(|| Watched {
            file,
            size: now,
            by: Vec::new(),
        })
        .by
        .push(by);
    if now < size {
        tell(run, socket, &said(share, node));
    }
}

/// Run `run`'s VM maps the file `payload` names no more (`kind::UNWATCH`).
pub(super) fn unwatch(run: &str, payload: &[u8]) {
    if let Some((share, node, _)) = named(payload) {
        let_go(|b| b.run == run && b.share == share && b.node == node);
    }
}

/// Run `run`'s VM has ended: none of its files are watched for it.
pub(super) fn forget(run: &str) {
    let_go(|b| b.run == run);
}

/// Watches no more for each watcher `of` picks; a file let go of with its last watcher.
fn let_go(of: impl Fn(&Watcher) -> bool) {
    let mut ended = Vec::new();
    {
        let mut watched = lock(&WATCHED);
        for (w, f) in watched.iter_mut() {
            let before = f.by.len();
            f.by.retain(|b| !of(b));
            ended.extend(std::iter::repeat_n(*w, before.saturating_sub(f.by.len())));
        }
        watched.retain(|_, f| !f.by.is_empty());
    }
    for w in ended {
        shards_vmm::platform::unwatch_writes(w, fired);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    /// A file a run's VM names is watched: grown, nothing is told; cut short by another
    /// process, the VM is told its share's place and node; named already cut short, told
    /// at once; let go of, and its run forgotten, nothing more is told.
    #[test]
    fn a_file_cut_short_is_told_to_the_vm_that_maps_it() {
        let dir = shards_testdir::TempDir::new("daemon-cuts").unwrap();
        let path = dir.join("f");
        std::fs::write(&path, vec![1u8; 1 << 20]).unwrap();
        let (ours, vm) = UnixStream::pair().unwrap();
        let socket = Arc::new(RunSocket::new(ours));
        let named = |share: u32, node: u64, size: u64| {
            let mut payload = said(share, node);
            payload.extend_from_slice(&size.to_be_bytes());
            payload
        };
        let fd = || OwnedFd::from(File::open(&path).unwrap());
        let told = |within: Duration| {
            vm.set_read_timeout(Some(within)).unwrap();
            shards_ipc::recv(&vm).ok().flatten().map(|m| (m.kind, m.payload))
        };
        let writer = || std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        watch("cuts-a", &socket, &named(2, 7, 1 << 20), vec![fd()]);
        writer().set_len(2 << 20).unwrap();
        assert_eq!(told(Duration::from_millis(200)), None, "grown");
        writer().set_len(4096).unwrap();
        assert_eq!(told(Duration::from_secs(5)), Some((kind::CUT, said(2, 7))));
        // Named as its window mapped it, longer than it is now: told at once.
        watch("cuts-a", &socket, &named(2, 8, 1 << 20), vec![fd()]);
        assert_eq!(told(Duration::from_secs(5)), Some((kind::CUT, said(2, 8))));
        assert_eq!(lock(&WATCHED).values().map(|f| f.by.len()).sum::<usize>(), 2);
        unwatch("cuts-a", &said(2, 7));
        forget("cuts-a");
        assert!(
            lock(&WATCHED)
                .values()
                .all(|f| f.by.iter().all(|b| b.run != "cuts-a"))
        );
        writer().set_len(10).unwrap();
        assert_eq!(told(Duration::from_millis(200)), None, "let go of");
    }
}
