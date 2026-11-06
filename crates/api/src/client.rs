//! The real [`Guests`]: a client of the `shards` binary. `boot` starts
//! `shards run --kernel … --vsock <socket> --no-console`, a VM process of shards' own (on
//! macOS signed, sandboxed and granted only what its broker grants, D30; on Windows in an
//! AppContainer and a Job that ends with it, D141), with the guest's vsock device's host side
//! at a socket in a directory of this process's own, 0700. `connect` dials that socket and
//! speaks Firecracker's `CONNECT <port>` / `OK <host port>` (`devices/virtio/vsock/muxer.rs`,
//! `conn.rs`). The guest's life is the process's: it exits 0 as the guest powers off, 3 as it
//! asks for a reset (`vm_run.rs` `supervise`), and as it is signalled or killed.
//!
//! What shards writes to its stdout and stderr is diagnostics, never a protocol: its last
//! [`TAIL`] bytes are kept for a refusal to quote. The guest is ready when a `CONNECT` is
//! answered `OK`; until it listens, the handshake is refused or closed and asked again.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::stream::{self, Socket};
use crate::{End, Guest, GuestSpec, Guests, Init, Refusal, Stream, allowed, backend, check};

/// The guests the `shards` binary boots: [`GuestSpec::binary`], else `SHARDS_BIN`, else
/// `shards` on `PATH`.
#[derive(Debug, Default, Clone, Copy)]
pub struct Shards;

impl Shards {
    pub fn new() -> Shards {
        Shards
    }
}

/// How much of what shards says on its stdout and stderr is kept, its last bytes: a
/// refusal's worth, its last few lines.
const TAIL: usize = 4096;
/// How long `stop` lets shards end after SIGTERM before SIGKILL: what `shards daemon stop`
/// gives a run (`daemon.rs` `STOP_GRACE`).
const STOP_GRACE: Duration = Duration::from_secs(10);
/// How long an ended shards' stdout and stderr are read for, for its last words, once it has
/// ended: a child it left holding them (macOS's grants broker until it exits) holds no
/// refusal back longer.
const DRAIN_GRACE: Duration = Duration::from_secs(1);
/// The first wait between handshakes, doubled to [`LAST_BACKOFF`]: a guest that listens is
/// reached within this of its listening while it boots, and a dial costs the VM process a
/// connection it closes (chosen, not measured: shards' boots take milliseconds to tens of
/// them, architecture.md's start-path budget).
const FIRST_BACKOFF: Duration = Duration::from_millis(1);
const LAST_BACKOFF: Duration = Duration::from_millis(50);
/// The longest answer a handshake reads: `OK 4294967295\n`, with room.
const ANSWER: usize = 32;

impl Guests for Shards {
    fn backend(&self) -> Result<crate::Backend, Refusal> {
        backend()
    }

    fn boot(&self, spec: &GuestSpec) -> Result<Box<dyn Guest>, Refusal> {
        check(spec)?;
        backend()?;
        let dir = PrivateDir::new()?;
        let vsock = dir.0.join("v");
        let args = arguments(spec, &vsock)?;
        let binary = spec
            .binary
            .clone()
            .or_else(|| std::env::var_os("SHARDS_BIN").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("shards"));
        let mut child = Command::new(&binary)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Refusal::Boot(format!("{}: {e}", binary.display())))?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                exited: false,
                stopping: false,
                ready: false,
                end: None,
                said: String::new(),
                ended_as: String::new(),
            }),
            changed: Condvar::new(),
            tail: Mutex::new(Tail {
                bytes: VecDeque::new(),
                open: 0,
            }),
            drained: Condvar::new(),
        });
        let outputs: [Option<Box<dyn io::Read + Send>>; 2] = [
            child.stdout.take().map(|o| Box::new(o) as Box<dyn io::Read + Send>),
            child.stderr.take().map(|e| Box::new(e) as Box<dyn io::Read + Send>),
        ];
        let started = start_threads(&shared, &child, outputs);
        if let Err(e) = started {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Refusal::Boot(format!("a thread to watch shards: {e}")));
        }
        Ok(Box::new(ShardsGuest {
            ports: spec.ports.clone(),
            vsock,
            #[cfg(unix)]
            pid: child.id(),
            child: Mutex::new(child),
            shared,
            _dir: dir,
        }))
    }
}

/// `shards run --kernel …` for `spec`, its vsock at `vsock`. `--kernel` comes first, as the
/// command line knows a direct boot by it (`cli/mod.rs`).
fn arguments(spec: &GuestSpec, vsock: &Path) -> Result<Vec<OsString>, Refusal> {
    let mut args: Vec<OsString> = vec!["run".into(), "--kernel".into(), spec.kernel.clone().into()];
    match &spec.init {
        Init::Initrd(path) => args.extend(["--initrd".into(), path.clone().into()]),
        Init::Executable(path) => args.extend(["--init".into(), path.clone().into()]),
    }
    args.extend([
        "--cmdline".into(),
        spec.cmdline.clone().into(),
        "--cpus".into(),
        spec.vcpus.to_string().into(),
        "--memory".into(),
        spec.memory_mib.to_string().into(),
    ]);
    for disk in &spec.disks {
        args.extend(["--disk".into(), with_mode("disk", &disk.path, disk.read_only)?]);
    }
    for share in &spec.shares {
        args.extend(["--share".into(), with_mode("share", &share.dir, share.read_only)?]);
    }
    args.extend(["--vsock".into(), vsock.into(), "--no-console".into()]);
    Ok(args)
}

/// `path`, with `:ro` where read-only. One that ends in `:ro` itself is refused: shards
/// would read it as read-only, and the path as without it.
fn with_mode(what: &'static str, path: &Path, read_only: bool) -> Result<OsString, Refusal> {
    if path.as_os_str().as_encoded_bytes().ends_with(b":ro") {
        return Err(Refusal::Unsupported {
            what,
            why: format!("{} ends in :ro, which shards reads as read-only", path.display()),
        });
    }
    let mut arg = OsString::from(path);
    if read_only {
        arg.push(":ro");
    }
    Ok(arg)
}

/// A directory of this process's own for a guest's vsock socket, 0700, removed as it drops.
#[derive(Debug)]
struct PrivateDir(PathBuf);

/// The longest path a Unix socket may have, less its NUL: macOS's sockaddr_un holds 104
/// bytes, Linux's 108.
const SOCKET_PATH: usize = 103;

impl PrivateDir {
    fn new() -> Result<PrivateDir, Refusal> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        #[expect(
            clippy::disallowed_methods,
            reason = "a guest's socket directory, made 0700 and removed with its guest, no test's"
        )]
        let tmp = std::env::temp_dir();
        // `<tmp>/shards-api-<pid>-<n>/v`, short enough to bind, else under /tmp.
        let longest = "/shards-api-4294967295-18446744073709551615/v".len();
        #[cfg(unix)]
        let tmp = if tmp.as_os_str().len().saturating_add(longest) <= SOCKET_PATH {
            tmp
        } else {
            PathBuf::from("/tmp")
        };
        #[cfg(not(unix))]
        let _ = (longest, SOCKET_PATH);
        let mut last = None;
        // A name an earlier process of the same pid left is passed over, a bounded number
        // of times.
        for _ in 0..64 {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = tmp.join(format!("shards-api-{}-{n}", std::process::id()));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            match builder.create(&dir) {
                Ok(()) => return Ok(PrivateDir(dir)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
                Err(e) => return Err(Refusal::Boot(format!("{}: {e}", dir.display()))),
            }
        }
        Err(Refusal::Boot(format!(
            "no directory for the guest's socket in {}: {last:?}",
            tmp.display()
        )))
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What a guest's watchers and callers share.
#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    /// Signalled as the process ends.
    changed: Condvar,
    tail: Mutex<Tail>,
    /// Signalled as each of its outputs ends.
    drained: Condvar,
}

#[derive(Debug)]
struct State {
    /// The process has exited (and is reaped, or about to be under this lock).
    exited: bool,
    /// `stop` or a drop is ending it: its end is `Stopped`.
    stopping: bool,
    /// A `CONNECT` has been answered `OK`.
    ready: bool,
    end: Option<End>,
    /// What shards said last, once it has exited.
    said: String,
    /// How shards ended, with its last words, once reaped.
    ended_as: String,
}

#[derive(Debug)]
struct Tail {
    bytes: VecDeque<u8>,
    /// Outputs still being read.
    open: u8,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts the threads that read shards' outputs into the tail, and the one that waits for it
/// to exit.
fn start_threads(
    shared: &Arc<Shared>,
    child: &Child,
    outputs: [Option<Box<dyn io::Read + Send>>; 2],
) -> io::Result<()> {
    for output in outputs.into_iter().flatten() {
        {
            let mut tail = lock(&shared.tail);
            tail.open = tail.open.saturating_add(1);
        }
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("shards-api-drain".into())
            .spawn(move || drain(&shared, output))?;
    }
    let exit = Exit::of(child)?;
    let shared = shared.clone();
    std::thread::Builder::new()
        .name("shards-api-wait".into())
        .spawn(move || exit.watch(&shared))?;
    Ok(())
}

/// Reads `output` to its end, keeping its last [`TAIL`] bytes.
fn drain(shared: &Shared, mut output: Box<dyn io::Read + Send>) {
    let mut buf = [0u8; 1024];
    loop {
        match output.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let mut tail = lock(&shared.tail);
                tail.bytes.extend(buf.get(..n).unwrap_or_default());
                let over = tail.bytes.len().saturating_sub(TAIL);
                tail.bytes.drain(..over);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let mut tail = lock(&shared.tail);
    tail.open = tail.open.saturating_sub(1);
    shared.drained.notify_all();
}

/// The process's exit, awaited without reaping it, so that its pid stays its own (and a
/// signal sent to it reaches no other process) until [`State::exited`] says it has gone.
struct Exit {
    #[cfg(unix)]
    pid: libc::id_t,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
}

impl Exit {
    fn of(child: &Child) -> io::Result<Exit> {
        #[cfg(unix)]
        return Ok(Exit { pid: child.id() });
        #[cfg(windows)]
        {
            use std::os::windows::io::AsHandle as _;
            Ok(Exit {
                handle: child.as_handle().try_clone_to_owned()?,
            })
        }
    }

    /// Waits for the process to exit.
    fn awaited(&self) -> io::Result<()> {
        #[cfg(unix)]
        loop {
            // SAFETY: a siginfo_t, plain data, for waitid(2) to fill.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: waitid(2) of our own child into `info`; WNOWAIT leaves it unreaped.
            let r = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid,
                    &raw mut info,
                    libc::WEXITED | libc::WNOWAIT,
                )
            };
            if r == 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
            use windows_sys::Win32::System::Threading::{INFINITE, WaitForSingleObject};
            // SAFETY: a process handle this value owns.
            if unsafe { WaitForSingleObject(self.handle.as_raw_handle(), INFINITE) } == WAIT_OBJECT_0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }
    }

    /// Waits for the process to exit, then for its last words, and says how it ended.
    fn watch(self, shared: &Shared) {
        let awaited = self.awaited();
        let deadline = Instant::now().checked_add(DRAIN_GRACE);
        let mut tail = lock(&shared.tail);
        while tail.open > 0 {
            let left = deadline.map_or(Duration::ZERO, |d| d.saturating_duration_since(Instant::now()));
            if left.is_zero() {
                break;
            }
            tail = shared
                .drained
                .wait_timeout(tail, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let said = String::from_utf8_lossy(tail.bytes.make_contiguous()).trim().to_string();
        drop(tail);
        let mut state = lock(&shared.state);
        if let Err(e) = awaited {
            state.end = Some(End::Failed(format!("waiting for shards: {e}")));
            state.ended_as = format!("waiting for shards: {e}");
        }
        state.said = said;
        state.exited = true;
        shared.changed.notify_all();
    }
}

/// A guest shards booted.
struct ShardsGuest {
    ports: Vec<u32>,
    vsock: PathBuf,
    #[cfg(unix)]
    pid: u32,
    child: Mutex<Child>,
    shared: Arc<Shared>,
    _dir: PrivateDir,
}

impl ShardsGuest {
    /// Its end, once the process has exited: reaped here, under its state's lock, once.
    fn end(&self, state: &mut State) -> Option<End> {
        if !state.exited {
            return None;
        }
        if state.end.is_none() {
            let status = lock(&self.child).wait();
            state.ended_as = match &status {
                Ok(status) => said(&format!("shards {status}"), &state.said),
                Err(e) => format!("shards: {e}"),
            };
            state.end = Some(match status {
                _ if state.stopping => End::Stopped,
                Ok(status) if status.code() == Some(0) => End::PowerOff,
                Ok(status) if status.code() == Some(3) => End::Reset,
                _ => End::Failed(state.ended_as.clone()),
            });
        }
        state.end.clone()
    }

    /// The refusal for a guest that has ended: `Boot`, with shards' exit and last words,
    /// where no `CONNECT` was ever answered, else `Ended`.
    fn gone(&self, state: &mut State) -> Option<Refusal> {
        let end = self.end(state)?;
        if !state.ready && !state.stopping {
            return Some(Refusal::Boot(format!("before it was ready, {}", state.ended_as)));
        }
        Some(Refusal::Ended(end))
    }

    /// Signals the process, while it is unreaped.
    fn signal(&self, state: &State, kill: bool) {
        if state.exited {
            return;
        }
        #[cfg(unix)]
        if let Ok(pid) = libc::pid_t::try_from(self.pid) {
            let signal = if kill { libc::SIGKILL } else { libc::SIGTERM };
            // SAFETY: kill(2) of our own child, unreaped while `exited` is false.
            unsafe { libc::kill(pid, signal) };
        }
        // Windows sends a process no signal: shards' daemon ends a VM's process as its
        // Job's (shards_ipc `Child::kill`), and `run --kernel`'s VM, in a Job that ends with
        // it (cli/mod.rs `vm`), ends as it does.
        #[cfg(windows)]
        {
            let _ = kill;
            let _ = lock(&self.child).kill();
        }
    }
}

/// `what`, with what shards said last where it said anything.
fn said(what: &str, said: &str) -> String {
    if said.is_empty() { what.to_string() } else { format!("{what}: {said}") }
}

/// Why a handshake failed: worth asking again, or not.
enum Attempt {
    Again(String),
    Refused(String),
}

/// One `CONNECT <port>` over a new connection to `vsock`, answered by `deadline`.
fn handshake(vsock: &Path, port: u32, deadline: Instant) -> Result<Socket, Attempt> {
    let mut s = stream::connect(vsock).map_err(|e| match e.kind() {
        // Not yet bound, or bound and not yet listening: the VM is starting.
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => Attempt::Again(format!("dialing: {e}")),
        _ => Attempt::Refused(format!("{}: {e}", vsock.display())),
    })?;
    let left = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1));
    let timeouts = Stream::set_write_timeout(&s, Some(left)).and_then(|()| Stream::set_read_timeout(&s, Some(left)));
    timeouts.map_err(|e| Attempt::Refused(format!("its timeouts: {e}")))?;
    s.write_all(format!("CONNECT {port}\n").as_bytes())
        .map_err(|e| Attempt::Again(format!("asking: {e}")))?;
    let mut line = Vec::with_capacity(ANSWER);
    let mut byte = [0u8; 1];
    while line.last() != Some(&b'\n') {
        if line.len() >= ANSWER {
            return Err(Attempt::Refused(format!(
                "answered {:?}, no line",
                String::from_utf8_lossy(&line)
            )));
        }
        match s.read(&mut byte) {
            // The guest did not accept: its device closes the connection.
            Ok(0) => return Err(Attempt::Again("closed unanswered (the guest does not listen there)".into())),
            Ok(_) => line.extend_from_slice(&byte),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Attempt::Again(format!("awaiting its answer: {e}"))),
        }
    }
    let answer = String::from_utf8_lossy(&line);
    let ok = answer
        .strip_prefix("OK ")
        .is_some_and(|n| n.trim_end().parse::<u32>().is_ok());
    if !ok {
        return Err(Attempt::Refused(format!("answered {answer:?}, not OK")));
    }
    let cleared = Stream::set_write_timeout(&s, None).and_then(|()| Stream::set_read_timeout(&s, None));
    cleared.map_err(|e| Attempt::Refused(format!("its timeouts: {e}")))?;
    Ok(s)
}

impl Guest for ShardsGuest {
    fn connect(&self, port: u32, patience: Duration) -> Result<Box<dyn Stream>, Refusal> {
        allowed(&self.ports, port)?;
        let start = Instant::now();
        let deadline = start.checked_add(patience).unwrap_or(start);
        let mut backoff = FIRST_BACKOFF;
        let mut last = String::from("never asked");
        loop {
            if let Some(refusal) = self.gone(&mut lock(&self.shared.state)) {
                return Err(refusal);
            }
            match handshake(&self.vsock, port, deadline) {
                Ok(s) => {
                    lock(&self.shared.state).ready = true;
                    return Ok(Box::new(s));
                }
                Err(Attempt::Refused(why)) => return Err(Refusal::Channel { port, why }),
                Err(Attempt::Again(why)) => last = why,
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            // Waits out the backoff, woken early by the process's end.
            let state = lock(&self.shared.state);
            if !state.exited {
                let _ = self.shared.changed.wait_timeout(state, backoff.min(left));
            }
            backoff = backoff.saturating_mul(2).min(LAST_BACKOFF);
        }
        if let Some(refusal) = self.gone(&mut lock(&self.shared.state)) {
            return Err(refusal);
        }
        Err(Refusal::Channel {
            port,
            why: format!("no OK within {patience:?}: {last}"),
        })
    }

    fn stop(&self) -> Result<(), Refusal> {
        let mut state = lock(&self.shared.state);
        if state.exited {
            return Ok(());
        }
        state.stopping = true;
        self.signal(&state, false);
        let deadline = Instant::now().checked_add(STOP_GRACE);
        while !state.exited {
            let left = deadline.map_or(Duration::ZERO, |d| d.saturating_duration_since(Instant::now()));
            if left.is_zero() {
                self.signal(&state, true);
                break;
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Ok(())
    }

    fn wait_for(&self, timeout: Duration) -> Result<Option<End>, Refusal> {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = lock(&self.shared.state);
        loop {
            if let Some(end) = self.end(&mut state) {
                return Ok(Some(end));
            }
            let left = deadline.map_or(Duration::ZERO, |d| d.saturating_duration_since(Instant::now()));
            if left.is_zero() {
                return Ok(None);
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

impl Drop for ShardsGuest {
    /// Ends the guest, if it has not ended: no VM outlives its owner.
    fn drop(&mut self) {
        let mut state = lock(&self.shared.state);
        if !state.exited {
            state.stopping = true;
            self.signal(&state, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Disk, Share};

    fn spec() -> GuestSpec {
        GuestSpec {
            kernel: "/k/Image".into(),
            init: Init::Executable("/k/init".into()),
            cmdline: "console=ttyS0 quiet".into(),
            vcpus: 2,
            memory_mib: 256,
            disks: vec![
                Disk {
                    path: "/d/root".into(),
                    read_only: true,
                },
                Disk {
                    path: "/d/data".into(),
                    read_only: false,
                },
            ],
            shares: vec![Share {
                dir: "/s/ro".into(),
                read_only: true,
            }],
            ports: vec![1024],
            binary: None,
        }
    }

    /// A spec becomes `shards run --kernel` and its flags, as vm_run.rs reads them, the
    /// kernel first (Hex HEX-GUEST-9).
    #[test]
    fn a_spec_is_the_flags_shards_run_reads() {
        let args = arguments(&spec(), Path::new("/p/v")).unwrap();
        let args: Vec<&str> = args.iter().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "run",
                "--kernel",
                "/k/Image",
                "--init",
                "/k/init",
                "--cmdline",
                "console=ttyS0 quiet",
                "--cpus",
                "2",
                "--memory",
                "256",
                "--disk",
                "/d/root:ro",
                "--disk",
                "/d/data",
                "--share",
                "/s/ro:ro",
                "--vsock",
                "/p/v",
                "--no-console"
            ]
        );
        let initrd = GuestSpec {
            init: Init::Initrd("/k/initrd".into()),
            ..spec()
        };
        let args = arguments(&initrd, Path::new("/p/v")).unwrap();
        assert_eq!(args.get(3..5).unwrap(), ["--initrd", "/k/initrd"]);
    }

    /// A path ending in `:ro`, which shards would read as a mode, is refused by name (Hex
    /// HEX-GUEST-9).
    #[test]
    fn a_path_ending_in_ro_is_refused() {
        let mut s = spec();
        s.disks.push(Disk {
            path: "/d/x:ro".into(),
            read_only: false,
        });
        assert!(matches!(
            arguments(&s, Path::new("/p/v")),
            Err(Refusal::Unsupported { what: "disk", .. })
        ));
    }

    /// The socket's directory is private, short enough to bind, and goes with its value.
    #[cfg(unix)]
    #[test]
    fn the_socket_directory_is_private_and_removed() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = PrivateDir::new().unwrap();
        let path = dir.0.clone();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(path.join("v").as_os_str().len() <= SOCKET_PATH);
        drop(dir);
        assert!(!path.exists());
    }
}
