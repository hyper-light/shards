use shards_testguest::limits::{Budget, Cause, Command, Phase, Report};
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

#[derive(Debug)]
enum Failure {
    Io(io::Error),
    Fork(io::Error),
    Input,
    Protocol,
    UnexpectedAdmission,
    WrongRefusal(i32),
    ChildStatus,
}
impl From<io::Error> for Failure {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl Failure {
    fn code(&self) -> Cause {
        match self {
            Self::Io(_) => Cause::Io,
            Self::Fork(_) => Cause::WrongRefusal,
            Self::Input => Cause::Input,
            Self::Protocol => Cause::Protocol,
            Self::UnexpectedAdmission => Cause::UnexpectedAdmission,
            Self::WrongRefusal(_) => Cause::WrongRefusal,
            Self::ChildStatus => Cause::ChildStatus,
        }
    }
    fn errno(&self) -> i32 {
        match self {
            Self::Io(e) | Self::Fork(e) => e.raw_os_error().unwrap_or(0),
            Self::WrongRefusal(n) => *n,
            _ => 0,
        }
    }
}

fn deadline(milliseconds: &str) -> Result<Instant, Failure> {
    let milliseconds = milliseconds.parse::<u64>().map_err(|_| Failure::Input)?;
    Instant::now()
        .checked_add(Duration::from_millis(milliseconds))
        .filter(|_| milliseconds != 0)
        .ok_or(Failure::Input)
}

fn remaining(until: Instant) -> io::Result<Duration> {
    let left = until.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(io::ErrorKind::TimedOut.into())
    } else {
        Ok(left)
    }
}

fn wait(fd: i32, until: Instant) -> io::Result<()> {
    loop {
        let left = remaining(until)?;
        let ms = i32::try_from(left.as_millis()).unwrap_or(i32::MAX).max(1);
        let mut event = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll borrows one held descriptor and initializes revents.
        let n = unsafe { libc::poll(&raw mut event, 1, ms) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if event.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if event.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            return Ok(());
        }
    }
}

fn listen(port: &str) -> Result<std::net::TcpListener, Failure> {
    let port = port.parse::<u16>().map_err(|_| Failure::Input)?;
    if port == 0 {
        return Err(Failure::Input);
    }
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn accept(listener: &std::net::TcpListener, until: Instant) -> Result<std::net::TcpStream, Failure> {
    wait(listener.as_raw_fd(), until)?;
    let (stream, _) = listener.accept()?;
    Ok(stream)
}

fn receive<const N: usize>(stream: &mut std::net::TcpStream, until: Instant) -> Result<[u8; N], Failure> {
    let mut bytes = [0; N];
    let mut offset = 0;
    while offset < N {
        stream.set_read_timeout(Some(remaining(until)?))?;
        match stream.read(bytes.get_mut(offset..).ok_or(Failure::Protocol)?) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Ok(n) => offset = offset.checked_add(n).ok_or(Failure::Protocol)?,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(bytes)
}

fn send(stream: &mut std::net::TcpStream, mut bytes: &[u8], until: Instant) -> Result<(), Failure> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(remaining(until)?))?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero).into()),
            Ok(n) => bytes = bytes.get(n..).ok_or(Failure::Protocol)?,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn publish(root: &std::path::Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let pending = root.join(format!("{name}.pending"));
    let mut file = std::fs::File::create(&pending)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(pending, root.join(name))
}

struct Child {
    pid: libc::pid_t,
    pidfd: OwnedFd,
    gate: UnixStream,
    reaped: bool,
}
impl Child {
    fn start(until: Instant) -> Result<Self, Failure> {
        let (gate, child) = UnixStream::pair()?;
        let child_fd = child.as_raw_fd();
        // SAFETY: this fixture is single threaded. Child calls only raw descriptor
        // syscalls and _exit, never Rust's allocator/runtime after fork.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(Failure::Fork(io::Error::last_os_error()));
        }
        if pid == 0 {
            // fd3 is the only surviving endpoint. Inherited gates cannot hold siblings.
            // SAFETY: descriptors and stack buffers were made before fork.
            unsafe {
                if libc::dup2(child_fd, 3) < 0 {
                    libc::_exit(120);
                }
                libc::close(0);
                libc::close(1);
                libc::close(2);
                if libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 0u32) != 0 {
                    libc::_exit(121);
                }
                let ready = [1u8];
                if libc::write(3, ready.as_ptr().cast(), 1) != 1 {
                    libc::_exit(122);
                }
                let mut release = [0u8];
                loop {
                    let n = libc::read(3, release.as_mut_ptr().cast(), 1);
                    if n == 1 {
                        libc::_exit(if release == [1] { 0 } else { 123 });
                    }
                    if n == 0 {
                        libc::_exit(124);
                    }
                    // EINTR is the only retry: the child otherwise blocks on its gate.
                    if *libc::__errno_location() != libc::EINTR {
                        libc::_exit(125);
                    }
                }
            }
        }
        drop(child);
        // Child cannot exit before its held gate is released, so this birth PID cannot recycle.
        // SAFETY: pidfd_open opens the known child, with flags zero.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            let e = io::Error::last_os_error();
            // SAFETY: this is still our held unreaped child, not a discovered PID.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
            return Err(e.into());
        }
        let raw = i32::try_from(raw).map_err(|_| Failure::Protocol)?;
        // SAFETY: this is a fresh descriptor, owned here alone.
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw) };
        let mut owned = Self {
            pid,
            pidfd,
            gate,
            reaped: false,
        };
        wait(owned.gate.as_raw_fd(), until)?;
        owned.gate.set_read_timeout(Some(remaining(until)?))?;
        let mut ready = [0];
        owned.gate.read_exact(&mut ready)?;
        if ready != [1] {
            return Err(Failure::Protocol);
        }
        Ok(owned)
    }

    fn release(mut self, until: Instant) -> Result<u32, Failure> {
        self.gate.set_write_timeout(Some(remaining(until)?))?;
        self.gate.write_all(&[1])?;
        wait(self.pidfd.as_raw_fd(), until)?;
        let mut status = 0;
        // SAFETY: waitpid only reaps this exact owned child after its exit event.
        if unsafe { libc::waitpid(self.pid, &raw mut status, 0) } != self.pid {
            return Err(io::Error::last_os_error().into());
        }
        self.reaped = true;
        if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
            return Err(Failure::ChildStatus);
        }
        // SAFETY: this child has been waited for; no new child is created between these calls.
        if unsafe { libc::waitpid(self.pid, &raw mut status, libc::WNOHANG) } != -1
            || io::Error::last_os_error().raw_os_error() != Some(libc::ECHILD)
        {
            return Err(Failure::ChildStatus);
        }
        u32::try_from(self.pid).map_err(|_| Failure::Protocol)
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if !self.reaped {
            // SAFETY: signal through the held pidfd, then reap only its owned child.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
            loop {
                // SAFETY: the exact unreaped child is owned by this object.
                let n = unsafe { libc::waitpid(self.pid, std::ptr::null_mut(), 0) };
                if n >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
        }
    }
}

fn descriptors() -> io::Result<u32> {
    std::fs::read_dir("/proc/self/fd")?.try_fold(0u32, |n, entry| {
        entry?;
        n.checked_add(1).ok_or_else(|| io::ErrorKind::InvalidData.into())
    })
}

pub fn actor(root: &str, tasks: &str, reserved: &str, children: &str, patience: &str, port: &str) -> i32 {
    let result = (|| -> Result<(), Failure> {
        use std::os::unix::fs::MetadataExt as _;
        let parse = |s: &str| s.parse::<u32>().map_err(|_| Failure::Input);
        let budget =
            Budget::new(parse(tasks)?, parse(reserved)?, parse(children)?).map_err(|_| Failure::Input)?;
        let until = deadline(patience)?;
        let root = std::path::Path::new(root);
        let listener = listen(port)?;
        let status = std::fs::read_to_string("/proc/self/status")?;
        let threads = status
            .lines()
            .find_map(|l| l.strip_prefix("Threads:"))
            .ok_or(Failure::Protocol)?
            .trim()
            .parse()
            .map_err(|_| Failure::Protocol)?;
        let mut report = Report {
            phase: Phase::Ready,
            budget,
            actor: std::process::id(),
            // SAFETY: these getters only observe this fixture's identity.
            parent: unsafe { libc::getppid() }
                .try_into()
                .map_err(|_| Failure::Protocol)?,
            // SAFETY: getuid only observes this fixture actor.
            uid: unsafe { libc::getuid() },
            reaper_uid: std::fs::metadata("/proc/1")?.uid(),
            threads,
            held: 0,
            reaped: 0,
            replacement: 0,
            descriptors: descriptors()?,
            cause: Cause::None,
            errno: 0,
        };
        let exercise = (|| -> Result<(), Failure> {
            publish(root, "ready", &report.encode())?;
            let mut control = accept(&listener, until)?;
            let baseline = descriptors()?;
            if Command::decode(receive(&mut control, until)?).map_err(|_| Failure::Protocol)? != Command::Fill
            {
                return Err(Failure::Protocol);
            }
            let mut held = Vec::new();
            held.try_reserve_exact(usize::try_from(budget.children).map_err(|_| Failure::Input)?)
                .map_err(io::Error::other)?;
            for _ in 0..budget.children {
                held.push(Child::start(until)?);
            }
            // Exactly one excess attempt. Success is owned and reaped before failure.
            match Child::start(until) {
                Err(Failure::Fork(e)) if e.raw_os_error() == Some(libc::EAGAIN) => {}
                Err(Failure::Fork(e)) => return Err(Failure::WrongRefusal(e.raw_os_error().unwrap_or(0))),
                Err(e) => return Err(e),
                Ok(child) => {
                    drop(child);
                    return Err(Failure::UnexpectedAdmission);
                }
            }
            report.descriptors = descriptors()?;
            report.phase = Phase::Refused;
            report.held = budget.children;
            report.cause = Cause::ForkRefused;
            report.errno = libc::EAGAIN;
            publish(root, "refused", &report.encode())?;
            send(&mut control, &report.encode(), until)?;
            if Command::decode(receive(&mut control, until)?).map_err(|_| Failure::Protocol)?
                != Command::Release
            {
                return Err(Failure::Protocol);
            }
            for child in held {
                child.release(until)?;
                report.reaped += 1;
            }
            report.held = 0;
            report.replacement = Child::start(until)?.release(until)?;
            report.reaped += 1;
            report.phase = Phase::Recovered;
            report.cause = Cause::None;
            report.errno = 0;
            let returned = descriptors()?;
            if returned != baseline {
                return Err(Failure::Protocol);
            }
            report.descriptors = returned;
            publish(root, "recovered", &report.encode())?;
            send(&mut control, &report.encode(), until)?;
            // Stay alive for host cancellation; this wait cannot consume another task.
            if Command::decode(receive(&mut control, until)?).map_err(|_| Failure::Protocol)?
                != Command::Finish
            {
                return Err(Failure::Protocol);
            }
            Ok(())
        })();
        if let Err(e) = &exercise {
            report.phase = Phase::Failed;
            report.cause = e.code();
            report.errno = e.errno();
            publish(root, "failure", &report.encode())?;
        }
        exercise
    })();
    finish(result)
}

pub fn healthy(root: &str, patience: &str, port: &str) -> i32 {
    finish((|| {
        let until = deadline(patience)?;
        let root = std::path::Path::new(root);
        let listener = listen(port)?;
        publish(root, "ready", &[1])?;
        let mut control = accept(&listener, until)?;
        for _ in 0..2 {
            let nonce = receive::<16>(&mut control, until)?;
            send(&mut control, &nonce, until)?;
        }
        if Command::decode(receive(&mut control, until)?).map_err(|_| Failure::Protocol)? != Command::Finish {
            return Err(Failure::Protocol);
        }
        Ok(())
    })())
}

pub fn nonce(hex: &str) -> i32 {
    finish((|| {
        let value = u128::from_str_radix(hex, 16).map_err(|_| Failure::Input)?;
        write!(io::stdout(), "{value:032x}")?;
        Ok(())
    })())
}

fn finish(result: Result<(), Failure>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            // Typed control flow above; presentation alone renders these fixture diagnostics.
            let _ = writeln!(io::stderr(), "managed task fixture: {e:?}");
            if let Failure::WrongRefusal(errno) = e {
                let _ = writeln!(io::stderr(), "unexpected fork errno {errno}");
            }
            1
        }
    }
}
