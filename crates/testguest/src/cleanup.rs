//! Ordinary-child lifecycle fixture; no foreign process is signalled here. Its waits take no
//! deadline of their own: its host's wait on it is judged (D209), and ends its microVM where
//! its judge refuses it.
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Command, Stdio};
use std::time::Instant;

fn wait(fds: &mut [libc::pollfd]) -> io::Result<()> {
    loop {
        // SAFETY: poll borrows only the fixture's live descriptors and fills revents; -1
        // waits until one of them is ready.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if n > 0 {
            return Ok(());
        }
        if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error());
        }
    }
}
fn readable(fd: i32) -> io::Result<()> {
    let mut fds = [libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    }];
    wait(&mut fds)?;
    let [ready] = fds;
    if ready.revents & libc::POLLIN == 0 {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
    }
    Ok(())
}
fn finish(result: io::Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(io::stderr(), "cleanup fixture: {error}");
            1
        }
    }
}
pub fn main(socket: &str, token: &str, count: &str) -> i32 {
    finish((|| {
        let n = count.parse::<usize>().map_err(io::Error::other)?;
        let listener = UnixListener::bind(socket)?;
        for _ in 0..n {
            let child = Command::new(std::env::current_exe()?)
                .args(["cleanup-control", "hold", socket, token])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            readable(listener.as_raw_fd())?;
            let (mut ready, _) = listener.accept()?;
            let mut pid = [0; 4];
            ready.read_exact(&mut pid)?;
            if u32::from_be_bytes(pid) != child.id() {
                return Err(io::Error::from(io::ErrorKind::InvalidData));
            }
            drop(child); // Deliberately leave these known children for init's real cleanup.
        }
        writeln!(io::stdout(), "ready")?;
        readable(listener.as_raw_fd())?;
        let (mut release, _) = listener.accept()?;
        let mut pid = [0; 4];
        release.read_exact(&mut pid)?;
        let controller = pidfd(u32::from_be_bytes(pid))?;
        release.write_all(&[1])?;
        // The release exec ends before main, so cleanup cannot kill the fixture's
        // release acknowledgement and manufacture a controller failure.
        let mut terminal = [libc::pollfd {
            fd: controller.as_raw_fd(),
            events: 0,
            revents: 0,
        }];
        wait(&mut terminal)?;
        let [terminal] = terminal;
        if terminal.revents & libc::POLLHUP == 0 {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        writeln!(io::stdout(), "released {n}")?;
        Ok(())
    })())
}
pub fn control(args: &[String]) -> i32 {
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or_default();
    finish(match arg(0) {
        "hold" => (|| {
            let mut stream = UnixStream::connect(arg(1))?;
            stream.write_all(&std::process::id().to_be_bytes())?;
            drop(stream);
            loop {
                // Independent of the control socket closing: only process cleanup ends it.
                // SAFETY: this controlled child simply waits for an ordinary signal.
                unsafe { libc::pause() };
            }
        })(),
        "release" => (|| {
            let mut stream = UnixStream::connect(arg(1))?;
            stream.write_all(&std::process::id().to_be_bytes())?;
            let mut accepted = [0];
            stream.read_exact(&mut accepted)?;
            if accepted != [1] {
                return Err(io::Error::from(io::ErrorKind::InvalidData));
            }
            Ok(())
        })(),
        "watch" => watch(arg(1), arg(2)),
        _ => Err(io::Error::from(io::ErrorKind::InvalidInput)),
    })
}
fn watch(token: &str, count: &str) -> io::Result<()> {
    let count = count.parse::<usize>().map_err(io::Error::other)?;
    let mut owned = Vec::<(u32, OwnedFd)>::new();
    // One discovery snapshot after all children acknowledged readiness; no /proc polling.
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(command) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let parts: Vec<_> = command.split(|byte| *byte == 0).collect();
        if parts.get(1) != Some(&b"cleanup-control".as_slice())
            || parts.get(2) != Some(&b"hold".as_slice())
            || parts.get(4) != Some(&token.as_bytes())
        {
            continue;
        }
        owned.push((pid, pidfd(pid)?));
    }
    if owned.len() != count {
        writeln!(io::stderr(), "discovery expected {count} actual {}", owned.len())?;
        return Err(io::Error::from(io::ErrorKind::InvalidData));
    }
    writeln!(io::stdout(), "ready")?;
    let began = Instant::now();
    let mut pending: Vec<_> = owned
        .iter()
        .map(|(_, fd)| libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let mut reaped = 0;
    while reaped < count {
        if let Err(error) = wait(&mut pending) {
            for (pid, _) in &owned {
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"));
                writeln!(io::stderr(), "remaining {pid} {stat:?}")?;
            }
            return Err(error);
        }
        for fd in &mut pending {
            if fd.fd >= 0 && fd.revents & libc::POLLHUP != 0 {
                fd.fd = -1;
                reaped += 1;
            }
            // After exit, wait only for the kernel's reaped POLLHUP transition.
            if fd.revents & libc::POLLIN != 0 {
                fd.events = 0;
            }
        }
    }
    writeln!(
        io::stdout(),
        "cleanup children {count} reaped {reaped} remaining 0 elapsed_us {}",
        began.elapsed().as_micros()
    )?;
    Ok(())
}

fn pidfd(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: observes only a token-identified child or live control peer; no signal.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = i32::try_from(fd).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
    // SAFETY: successful pidfd_open returns a fresh descriptor owned here alone.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
