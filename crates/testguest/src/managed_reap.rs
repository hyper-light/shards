use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;

/// Reaps `rounds` orphans, each round a line of the report at `path`. Its waits have no
/// bound of their own: its host's wait on the report judges them (tests/managed_lifecycle.rs,
/// D209), and what the report has said by then, at `path` with `.pending`, says how far it
/// came.
pub fn run(path: &str, rounds: &str) -> i32 {
    let result = (|| {
        let pending = format!("{path}.pending");
        let mut report = std::fs::File::create(&pending)?;
        let result = exercise(&mut report, path, rounds);
        if let Err(error) = &result {
            writeln!(
                report,
                "failure {:?} errno {:?}",
                error.kind(),
                error.raw_os_error()
            )?;
        }
        report.sync_all()?;
        std::fs::rename(pending, path)?;
        result
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(io::stderr(), "managed reaping fixture: {error}");
            1
        }
    }
}

fn exercise(report: &mut std::fs::File, path: &str, rounds: &str) -> io::Result<()> {
    let rounds = rounds.parse::<usize>().map_err(io::Error::other)?;
    if rounds == 0 {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let starting = format!("{path}.started.pending");
    std::fs::write(&starting, b"started")?;
    std::fs::rename(starting, format!("{path}.started"))?;
    let descriptors = || {
        std::fs::read_dir("/proc/self/fd")?.try_fold(0usize, |n, entry| {
            entry?;
            n.checked_add(1)
                .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))
        })
    };
    let before = descriptors()?;
    for round in 0..rounds {
        let pid = orphan(report)?;
        let after = descriptors()?;
        writeln!(
            report,
            "round {round} pid {pid} adopted 1 reaped true fds {before}:{after}"
        )?;
        if before != after {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
    }
    Ok(())
}

fn pidfd(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open refers only to this fixture's known, held-live child.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    let fd = i32::try_from(fd).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this is a fresh pidfd, owned here alone.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn wait_raw(fd: i32, events: i16) -> io::Result<()> {
    loop {
        let mut poll = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: poll borrows this one live descriptor and initializes revents.
        let n = unsafe { libc::poll(&raw mut poll, 1, -1) };
        if n < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if poll.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        if poll.revents & events != 0 {
            return Ok(());
        }
        if poll.revents & libc::POLLHUP != 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
    }
}

fn orphan(report: &mut std::fs::File) -> io::Result<u32> {
    let socket = format!("/tmp/reap-{}.sock", std::process::id());
    let listener = UnixListener::bind(&socket)?;
    let mut parent = Command::new(std::env::current_exe()?)
        .args(["managed-reap-child", &socket, "parent"])
        .spawn()?;
    let parent_fd = pidfd(parent.id())?;
    wait_raw(listener.as_raw_fd(), libc::POLLIN)?;
    let (mut link, _) = listener.accept()?;
    let mut bytes = [0; size_of::<u32>()];
    link.read_exact(&mut bytes)?;
    let pid = u32::from_be_bytes(bytes);
    let child_fd = pidfd(pid)?;
    wait_raw(parent_fd.as_raw_fd(), libc::POLLIN)?;
    if !parent.wait()?.success() {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    link.write_all(&[1])?;
    link.read_exact(&mut bytes)?;
    if u32::from_be_bytes(bytes) != 1 {
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    // Linux6.18 pidfd_poll distinguishes zombie POLLIN from reaped POLLHUP.
    if let Err(error) = wait_raw(child_fd.as_raw_fd(), libc::POLLHUP) {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                let state = stat
                    .rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().next());
                writeln!(report, "unreaped pid {pid} state {}", state.unwrap_or("unknown"))?;
            }
            Err(read) => writeln!(report, "unreaped pid {pid} stat errno {:?}", read.raw_os_error())?,
        }
        return Err(error);
    }
    drop(listener);
    std::fs::remove_file(socket)?;
    Ok(pid)
}

pub fn child(socket: &str, role: &str) -> i32 {
    let result = (|| {
        if role == "parent" {
            let child = Command::new(std::env::current_exe()?)
                .args(["managed-reap-child", socket, "orphan"])
                .spawn()?;
            drop(child);
            return Ok(());
        }
        if role != "orphan" {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let mut link = UnixStream::connect(socket)?;
        link.write_all(&std::process::id().to_be_bytes())?;
        let mut released = [0];
        link.read_exact(&mut released)?;
        if released != [1] {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        // SAFETY: getppid only reads this controlled child's namespace parent.
        let parent = unsafe { libc::getppid() };
        link.write_all(&parent.to_be_bytes())?;
        // SAFETY: the synthetic orphan crashes itself, never another process.
        if unsafe { libc::raise(libc::SIGKILL) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Err(io::Error::from(io::ErrorKind::InvalidData))
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(io::stderr(), "managed orphan fixture: {error}");
            1
        }
    }
}
