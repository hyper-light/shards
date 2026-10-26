//! Host-OS services behind one API: guest-memory reservation, copies between it and the
//! host's memory, positional I/O into guest memory, durable flushes, entropy, the console,
//! and vCPU thread policy. The rest of the VMM never calls the OS directly.

use std::fs::File;
use std::io;

mod checked;
mod copy;
pub use checked::copy_checked;
#[cfg(test)]
pub use copy::COPIED;
pub use copy::{copy_in, copy_out};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

/// AF_UNIX stream sockets, std's on Unix and shards' own on Windows (PM M199).
#[cfg(windows)]
pub use shards_afunix::{RawSocket, UnixListener, UnixStream, startup as winsock};
#[cfg(unix)]
pub use std::os::unix::net::{UnixListener, UnixStream};
/// A socket as the vsock device's poller names it.
#[cfg(unix)]
pub type RawSocket = std::os::fd::RawFd;

/// The stream of a share another server serves (D38): a Unix stream socket on Unix
/// (`--slates-fs-fd`), a duplex named pipe on Windows (`--slates-fs-handle`, PM M340).
#[cfg(unix)]
pub type ShareStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub type ShareStream = windows::PipeStream;

/// A socket's [`RawSocket`], on every host.
pub trait AsRawSocket {
    fn raw_socket(&self) -> RawSocket;
}

#[cfg(unix)]
impl AsRawSocket for UnixStream {
    fn raw_socket(&self) -> RawSocket {
        std::os::fd::AsRawFd::as_raw_fd(self)
    }
}

#[cfg(unix)]
impl AsRawSocket for UnixListener {
    fn raw_socket(&self) -> RawSocket {
        std::os::fd::AsRawFd::as_raw_fd(self)
    }
}

#[cfg(windows)]
impl AsRawSocket for UnixStream {
    fn raw_socket(&self) -> RawSocket {
        self.raw()
    }
}

#[cfg(windows)]
impl AsRawSocket for UnixListener {
    fn raw_socket(&self) -> RawSocket {
        self.raw()
    }
}

#[cfg(target_os = "macos")]
mod macos_thread;

#[cfg(unix)]
mod pressure;
#[cfg(unix)]
pub use pressure::{MemoryPressure, PRESSURE_WINDOW, PressureWatches};

mod granted;
#[cfg(unix)]
pub use granted::open_beneath_input;
pub use granted::{
    dial_unix, grant_input, grant_listener, input_metadata, input_path, listen_unix, open_input, set_dialer,
};

#[cfg(target_os = "linux")]
pub use shards_syscall_filter as seccomp;

/// What keeps a thread at the policy a `prioritize_` call gave it: the thread holds it
/// while it runs and drops it before it ends. On macOS the real-time policy's governor
/// watches the thread while it is held (macos_thread.rs): one that runs long goes to
/// timeshare before XNU's fail-safe would depress it (PM M317).
#[derive(Debug)]
pub struct Prioritized {
    #[cfg(target_os = "macos")]
    _watch: macos_thread::Watch,
}

/// Applies the scheduling policy measured to give vCPUs precise timer wake-ups. Only
/// macOS has a measured policy (docs/research/platform-measurements.md M8, M10); on other
/// hosts this is a no-op until one is measured.
pub fn prioritize_vcpu_thread() -> Result<Prioritized, String> {
    #[cfg(target_os = "macos")]
    return macos_thread::make_current_realtime().map(|watch| Prioritized { _watch: watch });
    #[cfg(not(target_os = "macos"))]
    Ok(Prioritized {})
}

/// A device worker's thread, which answers its guest's requests: on macOS at the vCPUs'
/// policy, where with every CPU of the host busy a block device's readers had 238k reads
/// verified in 3 s at p50, 210k at QoS user-interactive and 156k at the default, and all
/// three alike on a quiet host (PM M317). Elsewhere as it is.
pub fn prioritize_device_thread() -> Result<Prioritized, String> {
    prioritize_vcpu_thread()
}

/// Has the calling thread, a machine's vCPU hold's governor (D191), run as it wakes,
/// however busy its vCPU threads keep the CPUs they may use: on macOS at their real-time
/// policy ([`prioritize_vcpu_thread`]); on Windows above them, at
/// THREAD_PRIORITY_TIME_CRITICAL, where at their priority three spinning vCPU threads of a
/// process held to two CPUs kept it from running and used up to 1.58 s of CPU in a second
/// held to one (PM M294). On Linux, where the hold's test has held on every runner, as it
/// is.
pub fn prioritize_governor_thread() -> Result<Prioritized, String> {
    #[cfg(target_os = "macos")]
    return prioritize_vcpu_thread();
    #[cfg(windows)]
    return windows::run_time_critical().map(|()| Prioritized {});
    #[cfg(not(any(target_os = "macos", windows)))]
    Ok(Prioritized {})
}

/// The bytes of `name` in the directory `dir` holds open, or `None` if there is no such
/// file. A file longer than `max` bytes is an error, found before it is read.
pub fn read_in(dir: &File, name: &str, max: u64) -> io::Result<Option<Vec<u8>>> {
    read_bounded(open_in(dir, name), name, max)
}

/// The bytes of the regular file `rel` names beneath `root` ([`open_beneath_input`]), or
/// `None` if there is none. A file longer than `max` bytes is an error, found before it
/// is read.
pub fn read_beneath(root: &std::path::Path, rel: &[&str], max: u64) -> io::Result<Option<Vec<u8>>> {
    read_bounded(open_beneath_input(root, rel), &rel.join("/"), max)
}

/// [`read_in`] for a VM's input at `path` ([`open_input`]).
pub fn read_input(path: &std::path::Path, max: u64) -> io::Result<Option<Vec<u8>>> {
    read_bounded(open_input(path, false), &path.display().to_string(), max)
}

fn read_bounded(opened: io::Result<File>, name: &str, max: u64) -> io::Result<Option<Vec<u8>>> {
    let file = match opened {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{name}: {len} bytes, past the limit of {max}"),
        ));
    }
    let len = usize::try_from(len).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e.to_string()))?;
    bytes.resize(len, 0);
    read_exact_at(&file, &mut bytes, 0)?;
    Ok(Some(bytes))
}

/// Makes the entries of the directory at `path` durable: the renames into it survive a
/// crash once this returns. Linux takes an fsync of the directory itself (fsync(2)); macOS
/// takes `sync_durable`'s flush, whose `F_FULLFSYNC` directories accept (PM M46).
pub fn sync_dir(path: &std::path::Path) -> io::Result<()> {
    #[cfg(unix)]
    return sync_entries(&File::open(path)?);
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Whether letting go of a file written through a shared mapping, its last writable
/// descriptor or mapping, writes its pages back before it returns, as whoever reads the
/// file meanwhile waits: macOS (APFS, PM M278). Linux writes back in its own time.
pub const LET_GO_WRITES_BACK: bool = cfg!(target_os = "macos");

/// Has what was written to `file` written back now, on a thread of its own, where its OS
/// would make whoever lets go of the file last wait for it: macOS writes a file's dirty
/// pages back as its last writable descriptor or mapping goes, and whoever reads the file
/// meanwhile waits too (APFS, PM M278). fsync(2) alone, which writes them to the drive, not
/// through its cache as `F_FULLFSYNC` does: nothing here asks to be durable. Its own
/// descriptor of the file, which it closes once written back; the caller goes on at once,
/// or waits for it where no thread starts. Elsewhere nothing: Linux writes back in its own
/// time, and a close never waits for it.
pub fn write_back_ahead(file: &File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd as _;
        let fsync = |file: &File| {
            // SAFETY: fsync(2) of the descriptor `file` holds.
            if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        };
        let own = file.try_clone()?;
        let spawned = std::thread::Builder::new()
            .name("write-back".into())
            .spawn(move || {
                if let Err(e) = fsync(&own) {
                    crate::debug!("writing back: {e}");
                }
            });
        if spawned.is_err() {
            return fsync(file);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = file;
    Ok(())
}

/// Fills `buf` from `file` at `offset`, failing on a short file.
pub fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    let mut done = 0;
    while let Some(rest) = buf.get_mut(done..).filter(|r| !r.is_empty()) {
        let at = offset
            .checked_add(done as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file offset overflows"))?;
        // SAFETY: `rest` is a live, exclusively borrowed buffer of `rest.len()` bytes.
        let n = unsafe { read_at(file, rest.as_mut_ptr(), rest.len(), at)? };
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
    }
    Ok(())
}

/// Writes all of `buf` to `file` at `offset`, whatever the file's own offset.
pub fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    let mut done = 0;
    while let Some(rest) = buf.get(done..).filter(|r| !r.is_empty()) {
        let at = offset
            .checked_add(done as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file offset overflows"))?;
        // SAFETY: `rest` is a live buffer of `rest.len()` bytes, only read.
        let n = unsafe { write_at(file, rest.as_ptr(), rest.len(), at)? };
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        done += n;
    }
    Ok(())
}

/// Copies `from`'s first `len` bytes into `to`, which is `len` bytes long after: the ranges
/// its OS says hold data, and of them only the blocks not all zeros, so that `to` takes no
/// more of a disk than `from` does.
pub fn copy_used(from: &File, to: &File, len: u64) -> io::Result<()> {
    copy_ranges(from, to, len, &data_ranges(from, len)?)
}

/// Copies `ranges` of `from`'s first `len` bytes, `(offset, length)` as [`data_ranges`]
/// gives them, into `to`, which is `len` bytes long after: of them only the blocks not all
/// zeros.
pub fn copy_ranges(from: &File, to: &File, len: u64, ranges: &[(u64, u64)]) -> io::Result<()> {
    const CHUNK: usize = 1 << 20;
    const BLOCK: usize = 4 << 10;
    to.set_len(0)?;
    let mut buf = vec![0u8; CHUNK];
    for &(start, n) in ranges {
        let end = start.saturating_add(n).min(len);
        let mut at = start;
        while at < end {
            let want = usize::try_from(end - at).map_or(CHUNK, |rest| rest.min(CHUNK));
            let chunk = buf.get_mut(..want).ok_or(io::ErrorKind::InvalidInput)?;
            read_exact_at(from, chunk, at)?;
            // Each run of blocks not all zeros, in one write.
            let mut run = None;
            for (i, block) in chunk.chunks(BLOCK).enumerate() {
                let used = block.iter().any(|&b| b != 0);
                match (run, used) {
                    (None, true) => run = Some(i * BLOCK),
                    (Some(first), false) => {
                        write_all_at(
                            to,
                            chunk.get(first..i * BLOCK).unwrap_or_default(),
                            at + first as u64,
                        )?;
                        run = None;
                    }
                    _ => {}
                }
            }
            if let Some(first) = run {
                write_all_at(to, chunk.get(first..).unwrap_or_default(), at + first as u64)?;
            }
            at += want as u64;
        }
    }
    to.set_len(len)
}

/// An unbuffered handle to this process's standard output, independent of `std::io::stdout`'s
/// lock and line buffering.
pub fn stdout_file() -> io::Result<File> {
    #[cfg(unix)]
    let owned = std::os::fd::AsFd::as_fd(&io::stdout()).try_clone_to_owned()?;
    #[cfg(windows)]
    let owned = std::os::windows::io::AsHandle::as_handle(&io::stdout()).try_clone_to_owned()?;
    Ok(File::from(owned))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::io::Write;

    /// What a filesystem without F_FULLFSYNC holds is made durable with fsync instead:
    /// /dev/null refuses F_FULLFSYNC and takes fsync, as an SMB share does, where std's
    /// sync_all fails (review 1.13).
    #[cfg(target_os = "macos")]
    #[test]
    fn a_file_without_full_fsync_is_synced_still() {
        let null = std::fs::OpenOptions::new().write(true).open("/dev/null").unwrap();
        assert!(null.sync_all().is_err(), "std's sync_all took F_FULLFSYNC here");
        sync_durable(&null).unwrap();
        let held = shards_testdir::TempDir::new("sync").unwrap();
        let dir = std::fs::File::open(&held).unwrap();
        sync_entries(&dir).unwrap();
    }

    fn temp_file(tag: &str, contents: &[u8]) -> (shards_testdir::TempDir, File) {
        let dir = shards_testdir::TempDir::new(&format!("platform-{tag}")).unwrap();
        let path = dir.join("it");
        std::fs::File::create(&path).unwrap().write_all(contents).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        (dir, file)
    }

    /// Whether `fd` is readable now.
    #[cfg(unix)]
    fn readable(fd: std::os::fd::BorrowedFd<'_>) -> bool {
        use std::os::fd::AsRawFd as _;
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll(2) of one descriptor, without waiting.
        let ready = unsafe { libc::poll(&mut poll, 1, 0) };
        ready == 1
    }

    /// Whether `fd` becomes readable within a second: a watch's wake on Linux is counted by
    /// its instance's reader, a moment after the change (FileWatch).
    #[cfg(unix)]
    fn woken(fd: std::os::fd::BorrowedFd<'_>) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while !readable(fd) {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        true
    }

    /// A poller names what has something to read, or has ended, by its token; waits out
    /// its timeout when nothing is; names one again while anything is left to read
    /// (level); forgets what it no longer watches; and gives what one wait cannot hold
    /// in the next.
    #[cfg(unix)]
    #[test]
    fn a_poller_names_what_is_ready() {
        use std::io::Read as _;
        use std::os::fd::AsFd as _;
        use std::os::unix::net::UnixStream;
        use std::time::{Duration, Instant};
        // Its 150 pairs past the 256 descriptors a macOS terminal starts a process with.
        raise_descriptor_limit().unwrap();
        let poller = Poller::new().unwrap();
        let (mut a, a_peer) = UnixStream::pair().unwrap();
        let (b, b_peer) = UnixStream::pair().unwrap();
        poller.add(a_peer.as_fd(), 7).unwrap();
        poller.add(b_peer.as_fd(), 9).unwrap();
        let mut ready = Vec::new();
        let t0 = Instant::now();
        poller.wait(&mut ready, Some(Duration::from_millis(50))).unwrap();
        assert!(ready.is_empty());
        assert!(t0.elapsed() >= Duration::from_millis(50), "{:?}", t0.elapsed());
        a.write_all(b"x").unwrap();
        poller.wait(&mut ready, None).unwrap();
        assert_eq!(ready, [7]);
        poller.wait(&mut ready, Some(Duration::ZERO)).unwrap();
        assert_eq!(ready, [7], "ready while unread");
        let mut byte = [0u8; 1];
        (&a_peer).read_exact(&mut byte).unwrap();
        poller.wait(&mut ready, Some(Duration::ZERO)).unwrap();
        assert!(ready.is_empty());
        // An end is ready: shut, not closed, so no copy a child took keeps it open.
        b.shutdown(std::net::Shutdown::Write).unwrap();
        poller.wait(&mut ready, None).unwrap();
        assert_eq!(ready, [9]);
        poller.remove(b_peer.as_fd()).unwrap();
        poller.wait(&mut ready, Some(Duration::ZERO)).unwrap();
        assert!(ready.is_empty());
        // More ready than one wait holds: each comes, as what is read stops being ready.
        let pairs: Vec<(UnixStream, UnixStream)> = (0..150).map(|_| UnixStream::pair().unwrap()).collect();
        for (i, (_, theirs)) in pairs.iter().enumerate() {
            poller.add(theirs.as_fd(), 100 + i as u64).unwrap();
        }
        for (mut ours, _) in pairs.iter().map(|(o, t)| (o, t)) {
            ours.write_all(b"y").unwrap();
        }
        let mut seen = std::collections::BTreeSet::new();
        while seen.len() < pairs.len() {
            poller.wait(&mut ready, Some(Duration::from_secs(5))).unwrap();
            assert!(!ready.is_empty(), "{} of {} seen", seen.len(), pairs.len());
            for &token in &ready {
                let (_, theirs) = &pairs[(token - 100) as usize];
                (&*theirs).read_exact(&mut byte).unwrap();
                assert!(seen.insert(token), "{token} twice");
            }
        }
    }

    /// The threads a process may have are read where the system says: on macOS always,
    /// `kern.num_taskthreads`; in a Linux container with a cgroup namespace of its own, no
    /// more than its own group's limit.
    #[cfg(unix)]
    #[test]
    fn a_thread_limit_is_read_where_the_system_says_one() {
        let limit = thread_limit();
        if cfg!(target_os = "macos") {
            assert!(limit.is_some_and(|n| n >= 64), "{limit:?}");
        }
        assert!(limit.is_none_or(|n| n > 0), "{limit:?}");
        #[cfg(target_os = "linux")]
        if std::fs::read_to_string("/proc/self/cgroup").is_ok_and(|g| g == "0::/\n")
            && let Some(n) = std::fs::read_to_string("/sys/fs/cgroup/pids.max")
                .ok()
                .and_then(|t| t.trim().parse::<u64>().ok())
        {
            assert!(limit.is_some_and(|l| l <= n), "{limit:?}, the group's {n}");
        }
    }

    /// A process's control groups' task limit, as cgroupfs holds it: the least of its
    /// group's and those above it, the root of those it sees included; cgroup v2's, v1's
    /// pids controller's, or both; none where none is set, or where its group is out of
    /// the namespace's sight.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_control_groups_task_limit_is_the_least_up_to_its_root() {
        let root_dir = shards_testdir::TempDir::new("pids-max").unwrap();
        let root = root_dir.join("pids-max");
        let write = |at: &str, text: &str| {
            let path = root.join(at);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        let pids_max = |groups: &str| super::unix::pids_max(&root, groups);
        assert_eq!(pids_max("0::/\n"), None);
        // A container's, with a namespace of its own: at the root it sees.
        write("pids.max", "100\n");
        assert_eq!(pids_max("0::/\n"), Some(100));
        write("a/b/pids.max", "max\n");
        write("a/pids.max", "50\n");
        assert_eq!(pids_max("0::/a/b\n"), Some(50));
        write("pids/x/pids.max", "70\n");
        write("pids/pids.max", "max\n");
        assert_eq!(pids_max("12:pids:/x\n11:cpu,cpuacct:/x\n"), Some(70));
        assert_eq!(pids_max("12:cpu,pids:/x\n0::/\n"), Some(70));
        assert_eq!(pids_max("12:cpu,pids:/x\n0::/a/b\n"), Some(50));
        assert_eq!(pids_max("11:cpu:/x\n"), None);
        assert_eq!(pids_max("0::/../c\n"), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A child's end is told once it has exited, not before, and once; its status is
    /// then taken without waiting. A child that has ended already is ready at once on
    /// Linux, and refused on macOS (`ESRCH`, measured): its watcher looks at it instead.
    #[cfg(unix)]
    #[test]
    fn a_poller_tells_of_a_childs_end() {
        use std::time::Duration;
        let poller = Poller::new().unwrap();
        let mut ready = Vec::new();
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("0.3")
            .spawn()
            .unwrap();
        {
            let _watch = poller.add_exit(child.id(), 9).unwrap();
            poller.wait(&mut ready, Some(Duration::from_millis(50))).unwrap();
            assert!(ready.is_empty(), "told of an end to come");
            poller.wait(&mut ready, Some(Duration::from_secs(10))).unwrap();
            assert_eq!(ready, [9]);
            assert!(child.try_wait().unwrap().is_some(), "not to be taken at once");
        }
        poller.wait(&mut ready, Some(Duration::from_millis(50))).unwrap();
        assert!(ready.is_empty(), "told twice");
        let mut ended = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 3"])
            .spawn()
            .unwrap();
        // SAFETY: an all-zero siginfo_t is valid; waitid(2) fills it for our child, which it
        // leaves unreaped (WNOWAIT).
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        let waited =
            unsafe { libc::waitid(libc::P_PID, ended.id(), &mut info, libc::WEXITED | libc::WNOWAIT) };
        assert_eq!(waited, 0);
        let watched = poller.add_exit(ended.id(), 10);
        if cfg!(target_os = "macos") {
            assert_eq!(
                watched.map(|_| ()).map_err(|e| e.raw_os_error()),
                Err(Some(libc::ESRCH))
            );
        } else {
            let _watch = watched.unwrap();
            poller.wait(&mut ready, Some(Duration::from_secs(1))).unwrap();
            assert_eq!(ready, [10]);
        }
        assert_eq!(ended.wait().unwrap().code(), Some(3));
    }

    /// More watches than a user may have inotify instances (128 by default, shared with the
    /// rest of the session) each see their own directory change and no other's: the
    /// process's one instance serves them all (PM M257).
    #[cfg(target_os = "linux")]
    #[test]
    fn more_watches_than_a_user_has_instances_each_see_their_own() {
        let root = shards_testdir::TempDir::new("platform-watches").unwrap();
        let dirs: Vec<std::path::PathBuf> = (0..300)
            .map(|i| {
                let d = root.join(i.to_string());
                std::fs::create_dir(&d).unwrap();
                d
            })
            .collect();
        let watches: Vec<FileWatch> = dirs
            .iter()
            .map(|d| FileWatch::new(&open_dir(d).unwrap()).unwrap())
            .collect();
        std::fs::write(dirs[7].join("x"), b"").unwrap();
        assert!(woken(watches[7].fd()), "watch 7, of the directory changed");
        std::thread::sleep(std::time::Duration::from_millis(50));
        for (i, w) in watches.iter().enumerate().filter(|(i, _)| *i != 7) {
            assert!(!readable(w.fd()), "watch {i} woke for another's change");
        }
        for d in &dirs {
            std::fs::write(d.join("y"), b"").unwrap();
        }
        for (i, w) in watches.iter().enumerate() {
            assert!(woken(w.fd()), "watch {i}");
        }
    }

    /// A watch on a directory wakes for a name's coming and going there, and for writes
    /// to the file it watches in it, only that file's, and waits again once cleared;
    /// all wherever the directory goes. A write to a file it does not watch wakes it not.
    #[cfg(unix)]
    #[test]
    fn a_watch_sees_a_directory_and_its_file_change() {
        let dir = shards_testdir::TempDir::new("platform-watch").unwrap();
        let append = |name: &str| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(name))
                .unwrap()
        };
        let (mut a, mut b) = (append("a"), append("b"));
        let opened = open_dir(&dir).unwrap();
        let mut watch = FileWatch::new(&opened).unwrap();
        assert!(!readable(watch.fd()));
        a.write_all(b"w").unwrap();
        assert!(!readable(watch.fd()), "a write, with no file watched");
        watch.file(&File::open(dir.join("a")).unwrap()).unwrap();
        b.write_all(b"w").unwrap();
        assert!(!readable(watch.fd()), "a write to a file not watched");
        a.write_all(b"x").unwrap();
        assert!(woken(watch.fd()), "a write to the file watched");
        watch.clear();
        assert!(!readable(watch.fd()));
        append("c");
        assert!(woken(watch.fd()), "a name come");
        watch.clear();
        watch.file(&File::open(dir.join("b")).unwrap()).unwrap();
        watch.clear();
        a.write_all(b"w").unwrap();
        assert!(!readable(watch.fd()), "a write to the file watched before");
        b.write_all(b"y").unwrap();
        assert!(woken(watch.fd()), "a write to the file watched now");
        watch.clear();
        let moved = dir.with_extension("moved");
        std::fs::rename(&dir, &moved).unwrap();
        watch.clear();
        std::fs::remove_file(moved.join("c")).unwrap();
        assert!(woken(watch.fd()), "a name gone, the directory moved");
        std::fs::remove_dir_all(&moved).unwrap();
    }

    /// A file watched for writes calls its function with the watch's number as another
    /// handle cuts it short, and as it writes it; two watches of it are one, which calls
    /// each; and let go of, the last of them, it calls nothing.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_watched_for_writes_calls_as_it_is_cut_short() {
        use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
        static WATCHED: AtomicI32 = AtomicI32::new(-1);
        static CALLS: AtomicU32 = AtomicU32::new(0);
        fn called(watched: libc::c_int) {
            WATCHED.store(watched, Ordering::SeqCst);
            CALLS.fetch_add(1, Ordering::SeqCst);
        }
        let calls_within_a_second = |at_least: u32| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
            while CALLS.load(Ordering::SeqCst) < at_least {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            true
        };
        let dir = shards_testdir::TempDir::new("platform-writes").unwrap();
        std::fs::write(dir.join("f"), vec![7u8; 1 << 16]).unwrap();
        let file = File::open(dir.join("f")).unwrap();
        let watched = watch_writes(&file, called).unwrap();
        assert_eq!(watch_writes(&file, called).unwrap(), watched, "one watch");
        std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join("f"))
            .unwrap()
            .set_len(1 << 12)
            .unwrap();
        assert!(
            calls_within_a_second(2),
            "cut short: {}",
            CALLS.load(Ordering::SeqCst)
        );
        assert_eq!(WATCHED.load(Ordering::SeqCst), watched);
        unwatch_writes(watched, called);
        unwatch_writes(watched, called);
        std::thread::sleep(std::time::Duration::from_millis(50));
        let before = CALLS.load(Ordering::SeqCst);
        std::fs::write(dir.join("f"), b"x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(CALLS.load(Ordering::SeqCst), before, "let go of");
    }

    #[test]
    fn reservations_are_zeroed_writable_and_page_aligned() {
        let page = page_size().unwrap();
        let len = 64 * page;
        let p = reserve(len).unwrap();
        assert!((p.as_ptr() as usize).is_multiple_of(page));
        // SAFETY: `p` is a fresh reservation of `len` bytes, released below.
        unsafe {
            let s = std::slice::from_raw_parts_mut(p.as_ptr(), len);
            assert!(s.iter().all(|&b| b == 0));
            s[0] = 1;
            s[len - 1] = 2;
            assert_eq!((s[0], s[len - 1]), (1, 2));
            release(p, len);
        }
    }

    #[test]
    fn positional_io_round_trips_and_reports_end_of_file() {
        let (_dir, file) = temp_file("io", b"0123456789");
        let mut buf = [0u8; 4];
        // SAFETY: `buf` is a live 4-byte buffer.
        assert_eq!(unsafe { read_at(&file, buf.as_mut_ptr(), 4, 3) }.unwrap(), 4);
        assert_eq!(&buf, b"3456");
        // SAFETY: as above; reading at end of file returns 0 bytes.
        assert_eq!(unsafe { read_at(&file, buf.as_mut_ptr(), 4, 10) }.unwrap(), 0);
        // SAFETY: the source is a live 3-byte buffer; writing past the end extends the file.
        assert_eq!(unsafe { write_at(&file, b"abc".as_ptr(), 3, 12) }.unwrap(), 3);
        sync_durable(&file).unwrap();
        let mut whole = [0u8; 15];
        read_exact_at(&file, &mut whole, 0).unwrap();
        assert_eq!(&whole, b"0123456789\0\0abc");
        let short = read_exact_at(&file, &mut [0u8; 4], 13).unwrap_err();
        assert_eq!(short.kind(), io::ErrorKind::UnexpectedEof);
        drop(file);
    }

    #[test]
    fn random_bytes_differ_between_calls() {
        // 600 bytes spans several getentropy(2) chunks.
        let (mut a, mut b) = ([0u8; 600], [0u8; 600]);
        fill_random(&mut a).unwrap();
        fill_random(&mut b).unwrap();
        assert_ne!(a, b);
        assert!(a[256..].iter().any(|&x| x != 0));
    }

    #[test]
    fn stdout_handle_is_writable() {
        let mut out = stdout_file().unwrap();
        out.write_all(b"").unwrap();
    }
}
