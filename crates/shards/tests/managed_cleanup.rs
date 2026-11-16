//! Real-VM ordinary-child cleanup with an independently owned joiner observing pidfds.
// Supervision platform proof and owed native Windows coverage are recorded in
// managed-cleanup/readiness.md; a compile pass is not a real-VM cleanup result.
#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing
)]
mod common;

use common::{Patience, Run, TempDir, guest_init, kernel};
use std::io::{self, BufRead as _, BufReader, Write as _};
#[cfg(unix)]
use std::os::fd::AsRawFd as _;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[track_caller]
fn call(home: &Path, args: &[&str]) -> Run {
    common::run_shards_env(
        &[],
        args,
        &[
            ("SHARDS_HOME", home.as_os_str()),
            ("SHARDS_KERNEL", kernel().as_os_str()),
            ("SHARDS_INIT", guest_init().as_os_str()),
        ],
    )
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Readiness,
    Completion,
}

#[derive(Debug)]
enum ClientError {
    Io(io::Error),
    /// Its wait on `Phase` refused: its judge's (said on stderr), or its caller's deadline past.
    Refused(Phase),
    OutputEnded {
        ready_bytes: usize,
    },
    InvalidReadiness {
        bytes: usize,
    },
    InvalidPhase,
    ExitNotification,
    MissingStdout,
    Patience,
    #[cfg(windows)]
    PipeNames,
    #[cfg(windows)]
    ConnectEvent,
}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(out, "cleanup client I/O: {error}"),
            Self::Refused(phase) => write!(out, "cleanup client {phase:?} refused"),
            Self::OutputEnded { ready_bytes } => write!(
                out,
                "cleanup client output ended with {ready_bytes} readiness bytes"
            ),
            Self::InvalidReadiness { bytes } => {
                write!(out, "cleanup client invalid readiness ({bytes} bytes)")
            }
            other => write!(out, "cleanup client {other:?}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientState {
    Starting,
    Ready,
    Finished,
}

#[repr(u64)]
enum Wake {
    Output = 1,
    Exit = 2,
}

struct Completed {
    status: ExitStatus,
    bytes: Vec<u8>,
}

/// What a client's wait on a phase is held to.
enum Bound {
    /// A wait judged as the tests' waits are (D209): on the daemon of its home and its
    /// microVMs, or on what this test started; its kind its call site.
    Judged(Box<Patience>),
    /// One absolute deadline of its caller's: the native fixtures', whose patience is their
    /// caller's explicit synthetic input (managed-cleanup/readiness.md), partial readiness
    /// not extending it.
    Until(Instant),
}

impl Bound {
    fn until(patience: Duration) -> Result<Bound, ClientError> {
        if patience.is_zero() {
            return Err(ClientError::Patience);
        }
        let until = Instant::now()
            .checked_add(patience)
            .ok_or(ClientError::Patience)?;
        Ok(Bound::Until(until))
    }

    /// How long a wait for its client's next event may block before it is looked at again.
    fn left(&self) -> Duration {
        match self {
            Bound::Judged(patience) => patience.next_look(),
            Bound::Until(until) => until.saturating_duration_since(Instant::now()),
        }
    }

    /// Whether its wait goes on, asked once no event came within [`left`](Self::left).
    fn holds(&mut self) -> bool {
        match self {
            Bound::Judged(patience) => patience.holds(),
            Bound::Until(until) => Instant::now() < *until,
        }
    }

    /// Its phase came: a judged wait's answer, which teaches its kind.
    fn done(self) {
        if let Bound::Judged(patience) = self {
            patience.done();
        }
    }
}

#[cfg(windows)]
use shards_ipc::{ExitWatch, Poller};
#[cfg(unix)]
use shards_vmm::platform::{ExitWatch, Poller};
#[cfg(unix)]
type Output = std::process::ChildStdout;
#[cfg(windows)]
type Output = shards_ipc::Stream;

struct Client {
    child: Child,
    /// Taken before anything could reap the child.
    #[cfg(unix)]
    identity: Option<ProcessIdentity>,
    output: Option<BufReader<Output>>,
    poller: Poller,
    exit: Option<ExitWatch>,
    status: Option<ExitStatus>,
    state: ClientState,
    /// Its readiness's bound, taken by [`await_ready`](Client::await_ready).
    ready: Option<Bound>,
}

/// A child as the kernel knows it, so a check after its reap never names another process
/// that reused its PID: its pidfd on Linux (pidfd_open(2)).
#[cfg(target_os = "linux")]
struct ProcessIdentity {
    pidfd: std::os::fd::OwnedFd,
}

/// macOS has no process descriptor: the PID with the start time the kernel gave it, which a
/// process reusing the PID after the reap does not have.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
struct ProcessIdentity {
    pid: libc::pid_t,
    started: (u64, u64),
}

#[cfg(target_os = "linux")]
impl ProcessIdentity {
    fn of(pid: u32) -> io::Result<Self> {
        use std::os::fd::FromRawFd as _;
        // SAFETY: pidfd_open(2) of an unreaped child of this process, without flags.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let raw = i32::try_from(raw).map_err(io::Error::other)?;
        // SAFETY: the descriptor pidfd_open(2) just returned, owned once.
        let pidfd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        Ok(Self { pidfd })
    }

    fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            pidfd: self.pidfd.try_clone()?,
        })
    }
}

#[cfg(target_os = "macos")]
impl ProcessIdentity {
    fn of(pid: u32) -> io::Result<Self> {
        let pid = libc::pid_t::try_from(pid).map_err(io::Error::other)?;
        let started = started(pid).ok_or_else(io::Error::last_os_error)?;
        Ok(Self { pid, started })
    }

    fn try_clone(&self) -> io::Result<Self> {
        Ok(*self)
    }
}

/// When the kernel started process `pid`, running or an unreaped zombie: proc_pidinfo(2)'s
/// PROC_PIDTBSDINFO with argument 1, which finds zombies too (xnu bsd/kern/proc_info.c;
/// audit-fix-tests-v1/logs/f11-macos-identity.log).
#[cfg(target_os = "macos")]
fn started(pid: libc::pid_t) -> Option<(u64, u64)> {
    // SAFETY: proc_bsdinfo is plain data, for which all zeroes is a value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = i32::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: a buffer of exactly `size` bytes for proc_pidinfo(2) to fill.
    let filled = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 1, (&raw mut info).cast(), size) };
    (filled == size).then_some((info.pbi_start_tvsec, info.pbi_start_tvusec))
}

impl Client {
    fn spawn(command: &mut Command, ready: Bound) -> Result<Self, ClientError> {
        let poller = Poller::new()?;
        #[cfg(windows)]
        let (output, writer) = windows_stdout()?;
        #[cfg(windows)]
        command.stdout(writer);
        #[cfg(unix)]
        command.stdout(Stdio::piped());
        let child = command.spawn();
        // Command retains explicit Stdio handles for reuse. Its parent copy must
        // close even when spawn failed, rather than keeping this pipe's writer alive.
        #[cfg(windows)]
        command.stdout(Stdio::null());
        let child = child?;
        // Ownership exists before taking stdout or installing any fallible watch.
        let mut client = Self {
            child,
            #[cfg(unix)]
            identity: None,
            output: None,
            poller,
            exit: None,
            status: None,
            state: ClientState::Starting,
            ready: Some(ready),
        };
        // Before the first try_wait, which may reap the child and free its PID.
        #[cfg(unix)]
        {
            client.identity = Some(ProcessIdentity::of(client.child.id())?);
        }
        #[cfg(unix)]
        let output = client.child.stdout.take().ok_or(ClientError::MissingStdout)?;
        #[cfg(unix)]
        {
            let fd = output.as_raw_fd();
            // SAFETY: fcntl only reads/sets status flags on this owned pipe descriptor.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 {
                return Err(io::Error::last_os_error().into());
            }
            // SAFETY: the same held descriptor, preserving every existing status flag.
            if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                return Err(io::Error::last_os_error().into());
            }
        }
        #[cfg(windows)]
        output.set_nonblocking(true)?;
        client.poller.add(&output, Wake::Output as u64)?;
        client.output = Some(BufReader::new(output));
        let watched = client.poller.add_exit(client.child.id(), Wake::Exit as u64);
        // Register before checking. macOS can refuse an already-exited process;
        // a single status check then distinguishes that case from a watch failure.
        client.status = client.child.try_wait()?;
        match watched {
            Ok(watch) if client.status.is_none() => client.exit = Some(watch),
            Ok(_) => {}
            Err(_) if client.status.is_some() => {}
            Err(error) => return Err(error.into()),
        }
        Ok(client)
    }

    fn wait_change(&mut self, bound: &mut Bound, phase: Phase) -> Result<(), ClientError> {
        let mut events = Vec::new();
        self.poller.wait(&mut events, Some(bound.left()))?;
        if events.is_empty() {
            return if bound.holds() {
                Ok(())
            } else {
                Err(ClientError::Refused(phase))
            };
        }
        if events.contains(&(Wake::Exit as u64)) {
            self.status = self.child.try_wait()?;
            if self.status.is_none() {
                return Err(ClientError::ExitNotification);
            }
            // Linux pidfds remain readable after exit. Retire the watch so a
            // surviving delegated stdout writer cannot turn this into a busy loop.
            self.exit.take();
        }
        Ok(())
    }

    fn await_ready(&mut self) -> Result<(), ClientError> {
        if self.state != ClientState::Starting {
            return Err(ClientError::InvalidPhase);
        }
        let mut bound = self.ready.take().ok_or(ClientError::InvalidPhase)?;
        let expected = b"ready\n";
        let mut ready = Vec::new();
        while ready.len() < expected.len() {
            let output = self.output.as_mut().ok_or(ClientError::MissingStdout)?;
            match output.fill_buf() {
                Ok([]) => {
                    return Err(ClientError::OutputEnded {
                        ready_bytes: ready.len(),
                    });
                }
                Ok(bytes) => {
                    let n = bytes.len().min(expected.len() - ready.len());
                    ready.extend_from_slice(&bytes[..n]);
                    output.consume(n);
                    if !expected.starts_with(&ready) {
                        return Err(ClientError::InvalidReadiness { bytes: ready.len() });
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.wait_change(&mut bound, Phase::Readiness)?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        bound.done();
        self.state = ClientState::Ready;
        Ok(())
    }

    /// Whether nothing has come of it yet, at this one look: it has said nothing more and
    /// has not exited. What it said stays buffered for [`finish`](Self::finish).
    fn quiet(&mut self) -> Result<bool, ClientError> {
        if self.state != ClientState::Ready {
            return Err(ClientError::InvalidPhase);
        }
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
        }
        if self.status.is_some() {
            return Ok(false);
        }
        let output = self.output.as_mut().ok_or(ClientError::MissingStdout)?;
        match output.fill_buf() {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
            Ok(_) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn finish(&mut self, mut bound: Bound) -> Result<Completed, ClientError> {
        if self.state != ClientState::Ready {
            return Err(ClientError::InvalidPhase);
        }
        let mut bytes = Vec::new();
        loop {
            let output = self.output.as_mut().ok_or(ClientError::MissingStdout)?;
            match output.fill_buf() {
                Ok([]) => break,
                Ok(part) => {
                    let n = part.len();
                    bytes.extend_from_slice(part);
                    output.consume(n);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    self.wait_change(&mut bound, Phase::Completion)?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.poller
            .remove(self.output.as_ref().ok_or(ClientError::MissingStdout)?.get_ref())?;
        while self.status.is_none() {
            self.wait_change(&mut bound, Phase::Completion)?;
        }
        bound.done();
        self.state = ClientState::Finished;
        Ok(Completed {
            status: self.status.ok_or(ClientError::ExitNotification)?,
            bytes,
        })
    }
}

#[cfg(windows)]
fn windows_stdout() -> Result<(Output, Stdio), ClientError> {
    use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
    use std::sync::atomic::{AtomicU64, Ordering};
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    };
    use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    let next = NEXT
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
        .map_err(|_| ClientError::PipeNames)?;
    // The listener refuses an existing first instance. PID reuse/name collision is
    // an error, never permission to connect to someone else's pipe or retry a name.
    let name = format!(r"\\.\pipe\shards-cleanup-{}-{next}", std::process::id());
    let listener = shards_ipc::Listener::new(&name)?;
    let wide: Vec<_> = name.encode_utf16().chain([0]).collect();
    // SAFETY: a terminated pipe name. This separately opened child writer is
    // synchronous; the existing listener retains its overlapped server read end.
    let raw = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            std::ptr::null_mut(),
        )
    };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: the new handle is owned exactly once until Command takes its Stdio.
    let writer = unsafe { OwnedHandle::from_raw_handle(raw) };
    // No bound: the listener's connect was pending before this thread's own CreateFileW
    // opened its client end, which completes it; the wait is ordered after that open.
    // SAFETY: Listener owns its manual-reset connect event until this returns.
    match unsafe { WaitForSingleObject(listener.event(), INFINITE) } {
        WAIT_OBJECT_0 => {}
        WAIT_FAILED => return Err(io::Error::last_os_error().into()),
        _ => return Err(ClientError::ConnectEvent),
    }
    // Accept exactly once after actual completion; no WouldBlock retry/probe.
    let output = listener.accept()?;
    Ok((output, Stdio::from(writer)))
}

impl Drop for Client {
    fn drop(&mut self) {
        if self.status.is_none() {
            let _ = self.child.kill();
            if let Err(error) = self.child.wait() {
                let _ = writeln!(io::stderr(), "cleanup client reap failed: {error}");
            }
        }
    }
}

/// A client of `home`'s daemon that said it is ready, its wait on that judged ([`Bound::Judged`]),
/// its kind its caller's call site.
#[track_caller]
fn ready(home: &Path, args: &[&str]) -> Client {
    let bound = Bound::Judged(Box::new(Patience::of(home)));
    let mut client = Client::spawn(
        common::interruptible(
            common::command()
                .args(args)
                .env("SHARDS_HOME", home)
                .env("SHARDS_KERNEL", kernel())
                .env("SHARDS_INIT", guest_init())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit()),
        ),
        bound,
    )
    .unwrap();
    client
        .await_ready()
        .unwrap_or_else(|error| panic!("{args:?}: {error}"));
    client
}
/// What a client of `home`'s daemon said to its end, its wait judged, its kind its caller's
/// call site.
#[track_caller]
fn finished(home: &Path, client: &mut Client) -> String {
    let result = client
        .finish(Bound::Judged(Box::new(Patience::of(home))))
        .unwrap();
    assert!(
        result.status.success(),
        "{}: {} output bytes",
        result.status,
        result.bytes.len()
    );
    String::from_utf8(result.bytes).unwrap()
}
struct Daemon<'a>(&'a Path);
impl Drop for Daemon<'_> {
    fn drop(&mut self) {
        let _ = call(self.0, &["daemon", "stop"]);
    }
}

#[test]
#[ignore = "explicit bounded real-VM cleanup campaign"]
fn main_cleanup_reaps_owned_children_and_preserves_independent_joiner() {
    assert!(
        !common::cannot_run_vms(),
        "actual cleanup campaign requires a working hypervisor"
    );
    let home = TempDir::new("cleanup-owned");
    let _daemon = Daemon(&home);
    let (image, _) = common::served();
    println!(
        "CLEANUP_BINARIES shards={} init={} guest={} kernel={} helpers={}",
        common::shards().display(),
        guest_init().display(),
        common::test_guest().display(),
        kernel().display(),
        env!("SHARDS_HELPERS_CARRIED")
    );
    let pulled = call(&home, &["pull", "-q", &image]);
    assert_eq!(pulled.status, Some(0), "{pulled}");
    // Counts bound this synthetic fixture. 72 children exceeds the unrelated 64-pass
    // source literal, but is not a claim that process count requires that many passes.
    for (mode, n) in [("own", 8), ("init", 8), ("host", 8), ("host", 72)] {
        let token = format!("cleanup-{}-{mode}-{n}", std::process::id());
        let count = n.to_string();
        let mut args = vec!["run", "--pull", "never", "--name", "source"];
        match mode {
            "host" => args.extend(["--pid", "host"]),
            "init" => args.push("--init"),
            _ => {}
        }
        args.extend([&image, "cleanup", "/tmp/cleanup-control", &token, &count]);
        let mut main = ready(&home, &args);
        let join = call(
            &home,
            &[
                "run",
                "--pull",
                "never",
                "-d",
                "--name",
                "observer",
                "--network",
                "container:source",
                "--pid",
                "host",
                &image,
                "sleep",
            ],
        );
        assert_eq!(join.status, Some(0), "{join}");
        let pid = call(&home, &["inspect", "-f", "{{.State.Pid}}", "source"]);
        assert_eq!(pid.status, Some(0), "{pid}");
        let mut watcher = ready(
            &home,
            &[
                "exec",
                "observer",
                "/bin/testguest",
                "cleanup-control",
                "watch",
                &token,
                &count,
            ],
        );
        // The held-live control, as an order: the observer, ready while every child is held,
        // has reported nothing and not ended before the release is asked, so its report
        // comes of the release and not of main's readiness. (Its old form, a watch of 100 ms
        // that had to fail, was a claim that nothing happens within a time, which a judged
        // wait cannot refuse.)
        assert!(
            watcher.quiet().unwrap(),
            "the observer reported or ended before the release, mode={mode} children={n}"
        );
        println!("CLEANUP_CONTROL observer_quiet_before_release=true mode={mode} children={n}");
        let began = Instant::now();
        let release = call(
            &home,
            &[
                "exec",
                "source",
                "/bin/testguest",
                "cleanup-control",
                "release",
                "/tmp/cleanup-control",
            ],
        );
        assert_eq!(release.status, Some(0), "{release}");
        let ended = finished(&home, &mut main);
        assert_eq!(ended, format!("released {n}\n"));
        let observed = finished(&home, &mut watcher);
        assert!(
            observed.contains(&format!("cleanup children {n} reaped {n} remaining 0")),
            "{observed}"
        );
        let proof = format!("write:/work/cleanup-proof={token}");
        let progress = call(
            &home,
            &[
                "exec",
                "observer",
                "/bin/testguest",
                "fs",
                &proof,
                "print:/work/cleanup-proof",
            ],
        );
        assert_eq!(
            (progress.status, progress.stdout.as_str()),
            (Some(0), token.as_str()),
            "{progress}"
        );
        println!(
            "CLEANUP_RESULT mode={mode} children={n} vm_pid={} {} surviving_joiner_progress=true total_us={}",
            pid.stdout.trim(),
            observed.trim(),
            began.elapsed().as_micros()
        );
        let removed = call(&home, &["rm", "-f", "source", "observer"]);
        assert_eq!(removed.status, Some(0), "{removed}");
    }
}

mod native {
    use super::*;

    fn patience() -> Duration {
        let millis: u64 = std::env::var("CLEANUP_CLIENT_PATIENCE_MS")
            .expect("caller supplies native test patience")
            .parse()
            .unwrap();
        assert_ne!(millis, 0);
        Duration::from_millis(millis)
    }

    fn child(mode: &str) -> Client {
        let helper = std::env::var_os("CLEANUP_CLIENT_CHILD")
            .expect("caller supplies the absolute compiled readiness-child artifact");
        let helper = Path::new(&helper);
        assert!(
            helper.is_absolute() && helper.is_file(),
            "CLEANUP_CLIENT_CHILD must be an absolute existing native artifact"
        );
        Client::spawn(
            Command::new(helper)
                .arg(mode)
                .arg("stdin")
                .stdin(Stdio::piped())
                .stderr(Stdio::inherit()),
            Bound::until(patience()).unwrap(),
        )
        .unwrap()
    }

    /// One observation of the controlled child, through its identity held from spawn, never
    /// a PID it may have given up, and not a probe loop. A failing mutation is rescued before
    /// asserting so this test never leaves its own controlled child behind.
    #[cfg(unix)]
    struct Returned(ProcessIdentity);
    #[cfg(windows)]
    struct Returned(std::os::windows::io::OwnedHandle);

    impl Returned {
        fn of(client: &Client) -> Self {
            #[cfg(unix)]
            {
                let held = client.identity.as_ref().expect("identity taken at spawn");
                Self(held.try_clone().unwrap())
            }
            #[cfg(windows)]
            {
                use std::os::windows::io::AsHandle as _;
                Self(client.child.as_handle().try_clone_to_owned().unwrap())
            }
        }

        #[cfg(windows)]
        fn check(self) {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
            use windows_sys::Win32::System::Threading::WaitForSingleObject;
            // SAFETY: a duplicate of this exact child's held process handle. One
            // completion observation, never a reused PID or readiness probe loop.
            let ended = unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) };
            assert_eq!(ended, WAIT_OBJECT_0);
        }

        #[cfg(target_os = "linux")]
        fn check(self) {
            let fd = self.0.pidfd.as_raw_fd();
            let id = libc::id_t::try_from(fd).unwrap();
            // SAFETY: siginfo_t is plain data, for which all zeroes is a value.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: waitid(2) on this exact child's pidfd, without waiting.
            let waited =
                unsafe { libc::waitid(libc::P_PIDFD, id, &raw mut info, libc::WEXITED | libc::WNOHANG) };
            let error = io::Error::last_os_error();
            // SAFETY: si_pid of the siginfo waitid(2) filled, zero where nothing changed.
            if waited == 0 && unsafe { info.si_pid() } == 0 {
                // Still running: ended through its own pidfd, then reaped.
                // SAFETY: pidfd_send_signal(2) and waitid(2) on this exact child's pidfd.
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd,
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                    libc::waitid(libc::P_PIDFD, id, &raw mut info, libc::WEXITED);
                }
            }
            // A zombie was reaped by the waitid above (waited 0, si_pid set): also a rescue.
            assert_eq!(waited, -1, "controlled child needed explicit rescue: {waited}");
            assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
        }

        #[cfg(target_os = "macos")]
        fn check(self) {
            let ProcessIdentity { pid, started: at } = self.0;
            // Still ours, running or an unreaped zombie: a process that reused the PID after
            // the reap started at another time.
            let unreaped = started(pid) == Some(at);
            if unreaped {
                let mut status = 0;
                // SAFETY: an unreaped child of this process keeps its PID until reaped here.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, &raw mut status, 0);
                }
            }
            assert!(!unreaped, "controlled child needed explicit rescue");
        }
    }

    /// The identity taken at spawn still names the child once `finish` has reaped it, and the
    /// check sees it returned (Linux: waitid(P_PIDFD) answers ECHILD; macOS: no process of
    /// its PID and start time).
    #[cfg(unix)]
    #[test]
    fn a_child_reaped_through_its_client_is_returned() {
        let mut client = Client::spawn(
            Command::new("/bin/sh")
                .args(["-c", "echo ready"])
                .stdin(Stdio::null())
                .stderr(Stdio::inherit()),
            Bound::Judged(Box::new(Patience::started())),
        )
        .unwrap();
        let returned = Returned::of(&client);
        client.await_ready().unwrap();
        let completed = client
            .finish(Bound::Judged(Box::new(Patience::started())))
            .unwrap();
        assert!(completed.status.success());
        drop(client);
        returned.check();
    }

    fn rejected(mode: &str, deadline: bool) {
        let mut client = child(mode);
        let returned = Returned::of(&client);
        let result = client.await_ready();
        let expected = if deadline {
            matches!(result, Err(ClientError::Refused(Phase::Readiness)))
        } else {
            matches!(result, Err(ClientError::OutputEnded { ready_bytes: 0 }))
        };
        drop(client);
        returned.check();
        assert!(expected, "mode={mode} readiness result={result:?}");
    }

    #[test]
    #[ignore = "explicit caller-bounded native child fixture"]
    fn silent_child_deadline_reaps_the_owned_process() {
        rejected("silent", true);
    }

    #[test]
    #[ignore = "explicit caller-bounded native child fixture"]
    fn early_exit_reaps_the_owned_process() {
        rejected("early", false);
    }

    #[test]
    #[ignore = "explicit caller-bounded native child fixture"]
    fn partial_readiness_keeps_the_same_deadline_and_reaps() {
        rejected("partial", true);
    }

    #[test]
    #[ignore = "explicit caller-bounded native child fixture"]
    fn normal_release_preserves_output_and_reaps() {
        let mut client = child("normal");
        let returned = Returned::of(&client);
        client.await_ready().unwrap();
        client.child.stdin.as_mut().unwrap().write_all(b"r").unwrap();
        let completed = client.finish(Bound::until(patience()).unwrap()).unwrap();
        assert!(completed.status.success());
        assert_eq!(completed.bytes, b"released\n");
        drop(client);
        returned.check();
    }

    #[test]
    #[ignore = "explicit caller-bounded native child fixture"]
    fn dropping_ready_client_reaps_without_releasing_the_child() {
        let mut client = child("normal");
        let returned = Returned::of(&client);
        client.await_ready().unwrap();
        drop(client);
        returned.check();
    }
}
