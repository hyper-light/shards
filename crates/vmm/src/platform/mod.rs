//! Host-OS services behind one API: guest-memory reservation, copies between it and the
//! host's memory, positional I/O into guest memory, durable flushes, entropy, the console,
//! and vCPU thread policy. The rest of the VMM never calls the OS directly.

use std::fs::File;
use std::io;

mod copy;
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

#[cfg(target_os = "macos")]
mod macos_thread;

#[cfg(unix)]
mod granted;
#[cfg(unix)]
pub use granted::{
    dial_unix, grant_input, grant_listener, input_metadata, input_path, listen_unix, open_beneath_input,
    open_input, set_dialer,
};

/// `path` for reading, and with `write` for writing too (a VM's inputs; granted
/// descriptors are a Unix host's).
#[cfg(not(unix))]
pub fn open_input(path: &std::path::Path, write: bool) -> io::Result<File> {
    std::fs::OpenOptions::new().read(true).write(write).open(path)
}

/// `path` resolved, links and all.
#[cfg(not(unix))]
pub fn input_path(path: &std::path::Path) -> io::Result<std::path::PathBuf> {
    std::fs::canonicalize(path)
}

/// `path`'s metadata.
#[cfg(not(unix))]
pub fn input_metadata(path: &std::path::Path) -> io::Result<std::fs::Metadata> {
    std::fs::metadata(path)
}

#[cfg(target_os = "linux")]
pub mod seccomp;

/// Applies the scheduling policy measured to give vCPUs precise timer wake-ups. Only
/// macOS has a measured policy (docs/research/platform-measurements.md M8, M10); on other
/// hosts this is a no-op until one is measured.
pub fn prioritize_vcpu_thread() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return macos_thread::make_current_realtime();
    #[cfg(not(target_os = "macos"))]
    Ok(())
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
        assert!(readable(watch.fd()), "a write to the file watched");
        watch.clear();
        assert!(!readable(watch.fd()));
        append("c");
        assert!(readable(watch.fd()), "a name come");
        watch.clear();
        watch.file(&File::open(dir.join("b")).unwrap()).unwrap();
        watch.clear();
        a.write_all(b"w").unwrap();
        assert!(!readable(watch.fd()), "a write to the file watched before");
        b.write_all(b"y").unwrap();
        assert!(readable(watch.fd()), "a write to the file watched now");
        watch.clear();
        let moved = dir.with_extension("moved");
        std::fs::rename(&dir, &moved).unwrap();
        watch.clear();
        std::fs::remove_file(moved.join("c")).unwrap();
        assert!(readable(watch.fd()), "a name gone, the directory moved");
        std::fs::remove_dir_all(&moved).unwrap();
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
