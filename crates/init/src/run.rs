//! Runs a workload in an image (docs/design/architecture.md D16). The image on
//! virtio-pmem becomes the root filesystem under a tmpfs overlay, with the mounts Docker
//! gives a container. Then init dials the host for the workload, runs it as `docker run`
//! would, relays its stdio, and reports its exit status.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::time::{Duration, Instant};

use shards_abi::run::{self, Size, Spec, kind};
use shards_abi::{control, marker};

use crate::frames::{Outbox, each_frame};
use crate::linux::power_off;
use crate::orders::{Orders, cstrings, pointers};
use crate::user::{self, ExecUser};

/// `docker run`'s status for a command that never ran, when nothing says more
/// (docker/cli cli/command/container/run.go, toStatusError).
const NOT_RUN: u32 = 125;
/// Bytes buffered in each direction before init stops reading more, so backpressure
/// reaches the writer.
const BUFFERED: usize = 256 * 1024;
const CHUNK: usize = 64 * 1024;
/// How long a template waits for the kernel's crypto self-tests. One snapshotted while
/// they run is still correct, only slower to restore.
const SELFTESTS_WAIT: Duration = Duration::from_secs(2);

/// Why the workload did not run, and the status to report.
struct Failure {
    status: u32,
    message: String,
}

fn setup_failed(message: impl Into<String>) -> Failure {
    Failure {
        status: NOT_RUN,
        message: message.into(),
    }
}

/// Boots into the image on `device`, runs the host's workload, and powers off. As a
/// template (`template`), it asks for a snapshot once the image is mounted: every VM
/// restored from it continues from there, and dials the host for its own workload.
pub fn main(device: &str, template: bool) -> ! {
    // Before any snapshot, so that every copy of a template has one.
    let standby = mount_root(device).and_then(|()| Standby::fork());
    if template && standby.is_ok() {
        await_crypto_selftests();
        if let Err(e) = crate::linux::control_write(control::SNAPSHOT, control::SNAPSHOT_NOW) {
            let _ = writeln!(io::stderr(), "shards-init: requesting a snapshot: {e}");
            power_off()
        }
        // A restored VM continues here, with its snapshot's wall clock.
        let _ = crate::linux::control_write(control::MARKER, marker::RESUMED);
        if let Err(e) = crate::linux::sync_clock() {
            let _ = writeln!(io::stderr(), "shards-init: setting the clock: {e}");
        }
    }
    let conn = match dial(run::PORT, true) {
        Ok(conn) => conn,
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards-init: dialing the host: {e}");
            power_off()
        }
    };
    let _ = crate::linux::control_write(control::MARKER, marker::CONNECTED);
    // Before the workload exists: the host takes one connection on each of its ports, so
    // a workload that dials one finds it taken (AGENTFILE_ARCH.md §9.7). Without blocking:
    // the relay finishes the connection while the workload runs.
    let signals = dial(run::SIGNAL_PORT, false).ok();
    let started = standby.and_then(|standby| standby.start(&receive(&conn)?));
    let status = match started {
        Ok(workload) => {
            let _ = crate::linux::control_write(control::MARKER, marker::WORKLOAD_STARTED);
            let _ = send(&conn, kind::STARTED, &[]);
            workload.relay(&conn, signals)
        }
        Err(f) => {
            let _ = send(&conn, kind::SYSTEM_ERR, f.message.as_bytes());
            f.status
        }
    };
    let _ = send(&conn, kind::EXIT, &status.to_be_bytes());
    // The host closes the connection once it has the status. Powering off before then
    // could lose the frame on its way out.
    let _ = shutdown_and_wait(&conn);
    let _ = crate::linux::control_write(control::MARKER, marker::POWERING_OFF);
    power_off()
}

/// Waits for the crypto self-tests the kernel starts at boot (crypto/algapi.c,
/// `crypto_start_tests`), which run in `cryptomgr_test` threads alongside init. Every copy
/// of a template replays what its guest still had running, and this `PREEMPT_NONE` kernel
/// gives such a thread the CPU for up to a tick at a time (docs/research/
/// platform-measurements.md M21).
fn await_crypto_selftests() {
    let deadline = Instant::now() + SELFTESTS_WAIT;
    loop {
        match crypto_selftests_running() {
            Ok(false) => return,
            Ok(true) if Instant::now() >= deadline => {
                let _ = writeln!(
                    io::stderr(),
                    "shards-init: crypto self-tests still running after {SELFTESTS_WAIT:?}; saving the template anyway"
                );
                return;
            }
            Ok(true) => std::thread::sleep(Duration::from_millis(1)),
            Err(e) => {
                let _ = writeln!(io::stderr(), "shards-init: /proc/crypto: {e}");
                return;
            }
        }
    }
}

/// Whether /proc/crypto lists an algorithm under test (a larval) or not yet tested
/// (crypto/proc.c, `c_show`). A kernel without it runs no tests.
fn crypto_selftests_running() -> io::Result<bool> {
    let text = match std::fs::read_to_string("/proc/crypto") {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    Ok(text.lines().any(|line| {
        let mut field = line.splitn(2, ':').map(str::trim);
        matches!(
            (field.next(), field.next()),
            (Some("selftest"), Some("unknown")) | (Some("type"), Some("larval"))
        )
    }))
}

fn c(s: &str) -> Result<CString, Failure> {
    CString::new(s).map_err(|_| setup_failed(format!("{s:?} contains NUL")))
}

fn mount(source: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: &str) -> Result<(), Failure> {
    let (s, t, f, d) = (c(source)?, c(target)?, c(fstype)?, c(data)?);
    // SAFETY: NUL-terminated strings that outlive the call.
    if unsafe { libc::mount(s.as_ptr(), t.as_ptr(), f.as_ptr(), flags, d.as_ptr().cast()) } != 0 {
        return Err(setup_failed(format!(
            "mounting {fstype} on {target}: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Makes a directory unless it exists.
fn mkdir(path: &str) -> Result<(), Failure> {
    match std::fs::create_dir(path) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(setup_failed(format!("mkdir {path}: {e}"))),
        _ => Ok(()),
    }
}

fn chdir_chroot(path: &str, root: bool) -> Result<(), Failure> {
    let p = c(path)?;
    // SAFETY: a NUL-terminated path.
    let rc = unsafe {
        if root {
            libc::chroot(p.as_ptr())
        } else {
            libc::chdir(p.as_ptr())
        }
    };
    if rc != 0 {
        return Err(setup_failed(format!(
            "entering {path}: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn mount_root(device: &str) -> Result<(), Failure> {
    for dir in ["/lower", "/rw", "/newroot"] {
        mkdir(dir)?;
    }
    mount(device, "/lower", "erofs", libc::MS_RDONLY, "dax=always")?;
    mount("tmpfs", "/rw", "tmpfs", 0, "mode=0755")?;
    for dir in ["/rw/upper", "/rw/work"] {
        mkdir(dir)?;
    }
    mount(
        "overlay",
        "/newroot",
        "overlay",
        0,
        "lowerdir=/lower,upperdir=/rw/upper,workdir=/rw/work,volatile",
    )?;
    // The initramfs cannot be unmounted, so the new root moves over it
    // (Documentation/filesystems/ramfs-rootfs-initramfs.rst).
    chdir_chroot("/newroot", false)?;
    mount(".", "/", "", libc::MS_MOVE, "")?;
    chdir_chroot(".", true)?;
    chdir_chroot("/", false)?;
    // Docker's mounts for a container (moby daemon/pkg/oci/defaults.go, a 64 MiB /dev/shm
    // from daemon/config/config.go), except that /dev holds the VM's own devices.
    let (nosuid, noexec, nodev) = (libc::MS_NOSUID, libc::MS_NOEXEC, libc::MS_NODEV);
    for (source, target, fstype, flags, data) in [
        ("proc", "/proc", "proc", nosuid | noexec | nodev, ""),
        (
            "sysfs",
            "/sys",
            "sysfs",
            nosuid | noexec | nodev | libc::MS_RDONLY,
            "",
        ),
        ("devtmpfs", "/dev", "devtmpfs", nosuid, "mode=0755"),
        (
            "devpts",
            "/dev/pts",
            "devpts",
            nosuid | noexec,
            "newinstance,ptmxmode=0666,mode=0620,gid=5",
        ),
        (
            "shm",
            "/dev/shm",
            "tmpfs",
            nosuid | noexec | nodev,
            "mode=1777,size=65536k",
        ),
        ("mqueue", "/dev/mqueue", "mqueue", nosuid | noexec | nodev, ""),
    ] {
        mkdir(target)?;
        mount(source, target, fstype, flags, data)?;
    }
    Ok(())
}

/// Connects to a host port. Without blocking, the connection may still be in progress:
/// the socket turns writable when it completes.
fn dial(port: u32, blocking: bool) -> io::Result<File> {
    let flags = libc::SOCK_STREAM | libc::SOCK_CLOEXEC | if blocking { 0 } else { libc::SOCK_NONBLOCK };
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_VSOCK, flags, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let sock = unsafe { File::from_raw_fd(fd) };
    // SAFETY: an all-zero sockaddr_vm is a valid value.
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_cid = libc::VMADDR_CID_HOST;
    addr.svm_port = port;
    let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
    // SAFETY: `addr` is a valid sockaddr_vm of `len` bytes.
    if unsafe { libc::connect(fd, (&raw const addr).cast(), len) } != 0 {
        let e = io::Error::last_os_error();
        if blocking || e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(e);
        }
    }
    Ok(sock)
}

/// A nonblocking connect's result, once its socket is writable.
fn connect_result(fd: RawFd) -> io::Result<()> {
    let mut err: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt(2) into a c_int of `len` bytes.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&raw mut err).cast(),
            &mut len,
        )
    };
    match (rc, err) {
        (0, 0) => Ok(()),
        (0, e) => Err(io::Error::from_raw_os_error(e)),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Writes one frame, blocking.
fn send(conn: &File, kind: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("frame too long"))?;
    let mut w = conn;
    w.write_all(&run::header(kind, len))?;
    w.write_all(payload)
}

/// Half-closes the connection and waits, at most two seconds, for the host to close it.
fn shutdown_and_wait(conn: &File) -> io::Result<()> {
    // SAFETY: shutdown(2) on our own socket.
    unsafe { libc::shutdown(conn.as_raw_fd(), libc::SHUT_WR) };
    let mut pfd = libc::pollfd {
        fd: conn.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let mut buf = [0u8; 256];
    loop {
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut pfd, 1, 2000) } <= 0 {
            return Ok(());
        }
        if (&*conn).read(&mut buf)? == 0 {
            return Ok(());
        }
    }
}

fn receive(conn: &File) -> Result<Spec, Failure> {
    let mut r = conn;
    let mut h = [0u8; run::HEADER];
    r.read_exact(&mut h)
        .map_err(|e| setup_failed(format!("reading the workload: {e}")))?;
    let len = match run::parse_header(h) {
        Some((kind::SPEC, len)) => len,
        _ => return Err(setup_failed("the host sent no workload")),
    };
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)
        .map_err(|e| setup_failed(format!("reading the workload: {e}")))?;
    Spec::decode(&payload).ok_or_else(|| setup_failed("malformed workload"))
}

fn pipe() -> Result<(OwnedFd, OwnedFd), Failure> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: pipe2(2) fills two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(setup_failed(format!("pipe: {}", io::Error::last_os_error())));
    }
    let [r, w] = fds;
    // SAFETY: fresh descriptors nothing else owns.
    Ok(unsafe { (OwnedFd::from_raw_fd(r), OwnedFd::from_raw_fd(w)) })
}

/// What the child reports through its error pipe when it cannot exec: the step that
/// failed and errno.
mod step {
    pub const CHDIR: u8 = 0;
    pub const USER: u8 = 1;
    /// execve(2) itself, or setting up the stdio before it.
    pub const EXEC: u8 = 2;
    /// No candidate on PATH was an executable file.
    pub const NOT_IN_PATH: u8 = 3;
    /// The command, named by a path, is not there: its stat(2) failed.
    pub const STAT: u8 = 4;
    /// The command, named by a path, is a directory or may not be executed.
    pub const ACCESS: u8 = 5;
    /// Opening its terminal, or making it the session's.
    pub const TTY: u8 = 6;
}

/// A running workload and init's ends of its stdio. With a terminal, `stdout` is its
/// pty's master and `stdin` a duplicate of it, and there is no `stderr`.
struct Workload {
    pid: libc::pid_t,
    stdin: Option<OwnedFd>,
    stdout: Option<OwnedFd>,
    stderr: Option<OwnedFd>,
    sigchld: OwnedFd,
    tty: bool,
}

/// A pty's master, and the path of its peer, which the standby opens as the workload's
/// terminal. runc's steps (libcontainer/console_linux.go, safeAllocPty and setupConsole;
/// docs/research/tty-and-interactive-runs.md §2.2): a new master, unlocked; the size
/// given, if both dimensions are; the peer owned by the workload's user
/// (libcontainer/init_linux.go, fixStdioPermissions). The pty keeps the kernel's termios,
/// as on Docker's path, which changes none.
struct Pty {
    master: OwnedFd,
    peer: Vec<u8>,
}

impl Pty {
    fn open(size: Size, uid: u32) -> Result<Pty, Failure> {
        let failed = |what: &str| setup_failed(format!("{what}: {}", io::Error::last_os_error()));
        // SAFETY: posix_openpt(3) returns a new descriptor, or -1.
        let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(failed("opening /dev/ptmx"));
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let master = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: unlockpt(3) on our master.
        if unsafe { libc::unlockpt(fd) } != 0 {
            return Err(failed("unlocking the pty"));
        }
        let mut name = [0u8; 64];
        // SAFETY: ptsname_r(3) writes a NUL-terminated name within the buffer's length.
        if unsafe { libc::ptsname_r(fd, name.as_mut_ptr().cast(), name.len()) } != 0 {
            return Err(failed("naming the pty"));
        }
        let peer = std::ffi::CStr::from_bytes_until_nul(&name)
            .map_err(|_| setup_failed("the pty's name is not terminated"))?
            .to_bytes()
            .to_vec();
        if size.rows != 0 && size.cols != 0 {
            let ws = libc::winsize {
                ws_row: size.rows,
                ws_col: size.cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            // SAFETY: TIOCSWINSZ reads one winsize.
            if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } != 0 {
                return Err(failed("sizing the pty"));
            }
        }
        let path = CString::new(peer.clone()).map_err(|_| setup_failed("the pty's name holds a NUL"))?;
        // SAFETY: chown(2) of the peer by path; gid -1 keeps devpts's.
        if unsafe { libc::chown(path.as_ptr(), uid, u32::MAX) } != 0 {
            return Err(failed("handing the pty to the workload's user"));
        }
        Ok(Pty { master, peer })
    }
}

/// A process forked ahead of its workload's request, waiting to exec it, with the pipes
/// that become its stdio. init forks it before the template's snapshot, so that no run
/// waits for a fork (docs/research/platform-measurements.md M27).
struct Standby {
    pid: libc::pid_t,
    /// Where init writes the standby's orders: what to exec, and as whom.
    orders: OwnedFd,
    /// Closes when the standby execs: bytes on it mean it could not.
    err: OwnedFd,
    stdin: OwnedFd,
    stdout: OwnedFd,
    stderr: OwnedFd,
    sigchld: OwnedFd,
    /// The image's user database, read once: a template's image cannot change.
    passwd: Option<Vec<u8>>,
    group: Option<Vec<u8>>,
}

/// The standby's ends of its pipes, and init's, which it closes.
struct Ends {
    orders: OwnedFd,
    stdio: [OwnedFd; 3],
    err: OwnedFd,
    inits: [OwnedFd; 6],
}

impl Standby {
    fn fork() -> Result<Standby, Failure> {
        let passwd = std::fs::read("/etc/passwd").ok();
        let group = std::fs::read("/etc/group").ok();
        let (stdin_r, stdin_w) = pipe()?;
        let (stdout_r, stdout_w) = pipe()?;
        let (stderr_r, stderr_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        let (orders_r, orders_w) = pipe()?;
        // SIGCHLD arrives through a signalfd, so the relay can poll for it.
        // SAFETY: plain sigset operations on a local set.
        let sigchld = unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGCHLD);
            libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK)
        };
        if sigchld < 0 {
            return Err(setup_failed(format!("signalfd: {}", io::Error::last_os_error())));
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let sigchld = unsafe { OwnedFd::from_raw_fd(sigchld) };
        // SAFETY: init is single-threaded, so its child may run anything until it execs.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(setup_failed(format!("fork: {}", io::Error::last_os_error())));
        }
        if pid == 0 {
            standby(Ends {
                orders: orders_r,
                stdio: [stdin_r, stdout_w, stderr_w],
                err: err_w,
                inits: [stdin_w, stdout_r, stderr_r, err_r, orders_w, sigchld],
            })
        }
        drop((stdin_r, stdout_w, stderr_w, err_w, orders_r));
        Ok(Standby {
            pid,
            orders: orders_w,
            err: err_r,
            stdin: stdin_w,
            stdout: stdout_r,
            stderr: stderr_r,
            sigchld,
            passwd,
            group,
        })
    }

    /// Resolves the spec as Docker and runc do, then has the standby exec the workload. A
    /// standby that has ended is replaced first.
    fn start(self, spec: &Spec) -> Result<Workload, Failure> {
        let mut status = 0;
        // SAFETY: waitpid(2) for our own child, without blocking.
        let ended = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) } == self.pid;
        let standby = if ended { Standby::fork()? } else { self };
        let argv0 = spec
            .argv
            .first()
            .ok_or_else(|| setup_failed("no command given"))?;
        if !spec.hostname.is_empty() {
            // SAFETY: a buffer of the given length.
            if unsafe { libc::sethostname(spec.hostname.as_ptr().cast(), spec.hostname.len()) } != 0 {
                return Err(setup_failed(format!(
                    "sethostname: {}",
                    io::Error::last_os_error()
                )));
            }
        }
        let (passwd, group) = (standby.passwd.as_deref(), standby.group.as_deref());
        let ExecUser { uid, gid, groups } = user::resolve(&spec.user, passwd, group).map_err(setup_failed)?;
        let env = user::prepare_env(&spec.env, uid, passwd).map_err(setup_failed)?;
        let cwd = workdir(&spec.cwd)?;
        let path_env = env
            .iter()
            .rev()
            .find_map(|kv| kv.strip_prefix(b"PATH="))
            .unwrap_or_default();
        let explicit = argv0.contains(&b'/');
        let candidates: Vec<Vec<u8>> = if explicit {
            vec![argv0.clone()]
        } else {
            path_env
                .split(|&b| b == b':')
                .map(|dir| {
                    // An empty PATH entry is the working directory.
                    let dir: &[u8] = if dir.is_empty() { b"." } else { dir };
                    [dir, b"/", argv0].concat()
                })
                .collect()
        };
        for (list, what) in [
            (&candidates, "PATH"),
            (&spec.argv, "an argument"),
            (&env, "the environment"),
        ] {
            if list.iter().any(|b| b.contains(&0)) {
                return Err(setup_failed(format!("{what} contains a NUL byte")));
            }
        }
        if cwd.contains(&0) {
            return Err(setup_failed("the working directory contains a NUL byte"));
        }
        // The workload owns its stdio, so it can reopen it through /proc/self/fd, as runc's
        // fixStdioPermissions arranges (libcontainer/init_linux.go). A pipe's two ends are
        // one inode.
        let pty = match spec.tty {
            Some(size) => Some(Pty::open(size, uid)?),
            None => {
                for fd in [&standby.stdin, &standby.stdout, &standby.stderr] {
                    // SAFETY: fchown(2) on our own pipe; gid -1 leaves the group.
                    unsafe { libc::fchown(fd.as_raw_fd(), uid, u32::MAX) };
                }
                None
            }
        };
        let tried = candidates.clone();
        let orders = Orders {
            uid,
            gid,
            groups,
            cwd,
            explicit,
            candidates,
            argv: spec.argv.clone(),
            env,
            tty: pty.as_ref().map(|p| p.peer.clone()).unwrap_or_default(),
        }
        .encode();
        let Standby {
            pid,
            orders: to_standby,
            err,
            stdin,
            stdout,
            stderr,
            sigchld,
            ..
        } = standby;
        File::from(to_standby)
            .write_all(&orders)
            .map_err(|e| setup_failed(format!("starting the workload: {e}")))?;
        // The error pipe closes on exec: bytes on it mean the workload never started.
        let mut report = Vec::new();
        let _ = File::from(err).read_to_end(&mut report);
        if let [which, e0, e1, e2, e3, c0, c1, c2, c3] = report[..] {
            // The standby exits right after reporting.
            // SAFETY: waits for our own child.
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
            let errno = i32::from_be_bytes([e0, e1, e2, e3]);
            let tried = match (which, &pty) {
                (step::TTY, Some(p)) => &p.peer[..],
                _ => usize::try_from(u32::from_be_bytes([c0, c1, c2, c3]))
                    .ok()
                    .and_then(|i| tried.get(i))
                    .map_or(&argv0[..], Vec::as_slice),
            };
            return Err(exec_failure(which, errno, argv0, tried, &spec.cwd, &spec.user));
        }
        let Some(Pty { master, .. }) = pty else {
            return Ok(Workload {
                pid,
                stdin: Some(stdin),
                stdout: Some(stdout),
                stderr: Some(stderr),
                sigchld,
                tty: false,
            });
        };
        // The terminal carries everything; the pipes go unused.
        drop((stdin, stdout, stderr));
        let input = master
            .try_clone()
            .map_err(|e| setup_failed(format!("the pty's master: {e}")))?;
        Ok(Workload {
            pid,
            stdin: Some(input),
            stdout: Some(master),
            stderr: None,
            sigchld,
            tty: true,
        })
    }
}

/// The standby's side of the fork: it closes init's ends, waits for its orders, and runs
/// them as `child` does. The standby is single-threaded, as init was when it forked, so
/// it may allocate.
fn standby(ends: Ends) -> ! {
    let Ends {
        orders,
        stdio,
        err,
        inits,
    } = ends;
    drop(inits);
    let mut bytes = Vec::new();
    let got = File::from(orders).read_to_end(&mut bytes);
    let decoded = got.ok().and_then(|_| Orders::decode(&bytes));
    let built = decoded.and_then(|mut o| {
        let tty = std::mem::take(&mut o.tty);
        let tty = if tty.is_empty() {
            None
        } else {
            Some(CString::new(tty).ok()?)
        };
        Some((
            cstrings(std::mem::take(&mut o.candidates))?,
            cstrings(std::mem::take(&mut o.argv))?,
            cstrings(std::mem::take(&mut o.env))?,
            CString::new(std::mem::take(&mut o.cwd)).ok()?,
            tty,
            o,
        ))
    });
    let Some((candidates, argv, envp, cwd, tty, o)) = built else {
        // No orders: init has gone, or the VM is powering off. Nothing to run.
        // SAFETY: ends this process without running atexit handlers inherited from init.
        unsafe { libc::_exit(NOT_RUN as libc::c_int) }
    };
    let (argv_ptrs, envp_ptrs) = (pointers(&argv), pointers(&envp));
    let [stdin, stdout, stderr] = &stdio;
    // SAFETY: this process is the child of a fork of single-threaded init, and `child`
    // runs on data built above.
    unsafe {
        child(&Child {
            stdio: [stdin.as_raw_fd(), stdout.as_raw_fd(), stderr.as_raw_fd()],
            tty: tty.as_ref(),
            err: err.as_raw_fd(),
            cwd: &cwd,
            uid: o.uid,
            gid: o.gid,
            groups: &o.groups,
            candidates: &candidates,
            explicit: o.explicit,
            argv: argv_ptrs.as_ptr(),
            envp: envp_ptrs.as_ptr(),
        })
    }
}

impl Workload {
    /// Relays stdio until the workload has exited and its output is drained, and returns
    /// its status.
    fn relay(mut self, conn: &File, mut signals: Option<File>) -> u32 {
        let mut host = Some(conn.as_raw_fd());
        for fd in [host, self.stdin.as_ref().map(AsRawFd::as_raw_fd)]
            .into_iter()
            .chain([&self.stdout, &self.stderr].map(|f| f.as_ref().map(AsRawFd::as_raw_fd)))
            .flatten()
        {
            set_nonblocking(fd, true);
        }
        let mut from_host: Vec<u8> = Vec::new();
        let (mut signals_connected, mut from_signals) = (false, Vec::new());
        let mut to_stdin = Outbox::default();
        let mut stdin_eof = false;
        let mut to_host = Outbox::default();
        let mut status: Option<u32> = None;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let exited = status.is_some();
            if exited
                && self.stdout.is_none()
                && self.stderr.is_none()
                && (to_host.is_empty() || host.is_none())
            {
                break;
            }
            // SIGCHLD, the host, stdin, signals, stdout and stderr: six at most (audit D07).
            let mut set = [libc::pollfd {
                fd: -1,
                events: 0,
                revents: 0,
            }; 6];
            let mut used = 0;
            let mut poll = |fd: Option<RawFd>, events: libc::c_short| {
                if let (Some(fd), true, Some(slot)) = (fd, events != 0, set.get_mut(used)) {
                    *slot = libc::pollfd {
                        fd,
                        events,
                        revents: 0,
                    };
                    used += 1;
                }
            };
            poll(Some(self.sigchld.as_raw_fd()), libc::POLLIN);
            let host_events = if !exited && !stdin_eof && to_stdin.len() < BUFFERED {
                libc::POLLIN
            } else {
                0
            } | if to_host.is_empty() { 0 } else { libc::POLLOUT };
            poll(host, host_events);
            let stdin_events = if to_stdin.is_empty() { 0 } else { libc::POLLOUT };
            poll(self.stdin.as_ref().map(AsRawFd::as_raw_fd), stdin_events);
            let signal_events = if signals_connected {
                libc::POLLIN
            } else {
                libc::POLLOUT
            };
            poll(signals.as_ref().map(AsRawFd::as_raw_fd), signal_events);
            let out_events = if to_host.len() < BUFFERED { libc::POLLIN } else { 0 };
            poll(self.stdout.as_ref().map(AsRawFd::as_raw_fd), out_events);
            poll(self.stderr.as_ref().map(AsRawFd::as_raw_fd), out_events);
            let fds = set.get_mut(..used).unwrap_or_default();
            // SAFETY: valid pollfds.
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            for p in fds.iter().filter(|p| p.revents != 0) {
                let fd = p.fd;
                if fd == self.sigchld.as_raw_fd() {
                    self.reap(&mut status);
                    if status.is_some() {
                        // The workload's stdin goes with it.
                        self.stdin = None;
                        to_stdin.clear();
                    }
                } else if Some(fd) == host {
                    if p.revents & libc::POLLOUT != 0 {
                        match write(fd, to_host.pending()) {
                            Ok(n) => to_host.written(n),
                            Err(_) => host = None,
                        }
                    }
                    if p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 && host.is_some() {
                        match read(fd, &mut buf) {
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                            // The host is gone: nothing more for stdin.
                            Ok(0) | Err(_) => stdin_eof = true,
                            Ok(n) => {
                                from_host.extend_from_slice(buf.get(..n).unwrap_or_default());
                                let mut closed = false;
                                let whole = each_frame(&mut from_host, |which, payload| {
                                    if which == kind::STDIN {
                                        closed |= payload.is_empty();
                                        to_stdin.extend(&[payload]);
                                    }
                                });
                                stdin_eof |= closed || !whole;
                            }
                        }
                    }
                } else if Some(fd) == signals.as_ref().map(AsRawFd::as_raw_fd) {
                    if !signals_connected {
                        match connect_result(fd) {
                            Ok(()) => signals_connected = true,
                            Err(_) => signals = None,
                        }
                        continue;
                    }
                    match read(fd, &mut buf) {
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Ok(0) | Err(_) => signals = None,
                        Ok(n) => {
                            from_signals.extend_from_slice(buf.get(..n).unwrap_or_default());
                            let pid = self.pid;
                            let running = status.is_none();
                            let pty = self.stdout.as_ref().filter(|_| self.tty).map(AsRawFd::as_raw_fd);
                            let whole = each_frame(&mut from_signals, |which, payload| {
                                if which == kind::RESIZE {
                                    if let (Some(fd), Some(size)) = (pty, Size::decode(payload)) {
                                        resize(fd, size);
                                    }
                                    return;
                                }
                                if let (kind::SIGNAL, Ok(sig)) =
                                    (which, <[u8; 4]>::try_from(payload).map(u32::from_be_bytes))
                                    && running
                                    && (1..=64).contains(&sig)
                                {
                                    // SAFETY: kill(2) of our own child, not yet reaped.
                                    unsafe { libc::kill(pid, sig as libc::c_int) };
                                }
                            });
                            if !whole {
                                signals = None;
                            }
                        }
                    }
                } else if Some(fd) == self.stdin.as_ref().map(AsRawFd::as_raw_fd) {
                    match write(fd, to_stdin.pending()) {
                        Ok(n) => to_stdin.written(n),
                        // The workload closed its stdin.
                        Err(_) => {
                            self.stdin = None;
                            to_stdin.clear();
                        }
                    }
                } else {
                    let (slot, which) = if Some(fd) == self.stdout.as_ref().map(AsRawFd::as_raw_fd) {
                        (&mut self.stdout, kind::STDOUT)
                    } else {
                        (&mut self.stderr, kind::STDERR)
                    };
                    match read(fd, &mut buf) {
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Ok(0) | Err(_) => *slot = None,
                        Ok(n) => {
                            to_host
                                .extend(&[&run::header(which, n as u32), buf.get(..n).unwrap_or_default()]);
                        }
                    }
                }
            }
            if stdin_eof && to_stdin.is_empty() {
                self.stdin = None;
            }
        }
        if let Some(fd) = host {
            set_nonblocking(fd, false);
        }
        status.unwrap_or(NOT_RUN)
    }

    /// Reaps every exited child. When the workload is among them, its status is recorded
    /// and every process left is killed: a container ends with its main process.
    fn reap(&mut self, status: &mut Option<u32>) {
        let mut info = [0u8; std::mem::size_of::<libc::signalfd_siginfo>()];
        while read(self.sigchld.as_raw_fd(), &mut info).is_ok_and(|n| n > 0) {}
        loop {
            let mut st = 0;
            // SAFETY: waitpid(2) with a valid status pointer.
            let pid = unsafe { libc::waitpid(-1, &mut st, libc::WNOHANG) };
            if pid <= 0 {
                return;
            }
            if pid == self.pid {
                let _ = crate::linux::control_write(control::MARKER, marker::WORKLOAD_EXITED);
                *status = Some(if libc::WIFSIGNALED(st) {
                    128 + libc::WTERMSIG(st) as u32
                } else {
                    libc::WEXITSTATUS(st) as u32
                });
                // SAFETY: kill(2) of every process but init.
                unsafe { libc::kill(-1, libc::SIGKILL) };
            }
        }
    }
}

/// Sizes the pty whose master is `fd`, as the shim resizes a TTY container's
/// (containerd console, tc_unix.go): the kernel signals the terminal's foreground process
/// group only if the size changed (tty_io.c, tty_do_resize). A zero dimension leaves the
/// size alone, as the Docker CLI never sends one (cli/command/container/tty.go).
fn resize(fd: RawFd, size: Size) {
    if size.rows == 0 || size.cols == 0 {
        return;
    }
    let ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads one winsize.
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
}

/// The working directory, made as Docker makes it (moby daemon/container/container.go,
/// SetupWorkingDirectory): missing directories are created 0755, owned by root.
fn workdir(cwd: &[u8]) -> Result<Vec<u8>, Failure> {
    if cwd.is_empty() {
        return Ok(b"/".to_vec());
    }
    if cwd.first() != Some(&b'/') {
        return Err(setup_failed(format!(
            "the working directory {:?} is not absolute",
            String::from_utf8_lossy(cwd)
        )));
    }
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(cwd));
    match std::fs::create_dir_all(path) {
        Ok(()) => Ok(cwd.to_vec()),
        Err(_) if path.exists() && !path.is_dir() => Err(setup_failed(format!(
            "Cannot mkdir: {} is not a directory",
            path.display()
        ))),
        Err(e) => Err(setup_failed(format!("mkdir {}: {e}", path.display()))),
    }
}

/// Why the command did not start, in the words of runc's Go (exec.LookPath, and
/// os.PathError for the rest), which dockerd passes on: `tried` is the file it was
/// executing. The status is `docker run`'s for those words (shards_cmdline).
fn exec_failure(which: u8, errno: i32, argv0: &[u8], tried: &[u8], cwd: &[u8], user: &[u8]) -> Failure {
    use shards_cmdline::go::{linux_error, quote};
    let cmd = String::from_utf8_lossy(argv0);
    let err = linux_error(errno);
    let message = match which {
        step::NOT_IN_PATH => format!("exec: {}: executable file not found in $PATH", quote(&cmd)),
        step::STAT => format!("exec: {}: stat {cmd}: {err}", quote(&cmd)),
        step::ACCESS => format!("exec: {}: {err}", quote(&cmd)),
        step::CHDIR => format!("chdir to cwd ({}): {err}", quote(&String::from_utf8_lossy(cwd))),
        step::USER => format!("setting user {}: {err}", quote(&String::from_utf8_lossy(user))),
        step::TTY => format!("open {}: {err}", String::from_utf8_lossy(tried)),
        _ => format!("exec {}: {err}", String::from_utf8_lossy(tried)),
    };
    Failure {
        status: u32::from(shards_cmdline::commands::run_status(&message)),
        message,
    }
}

/// What the child needs, built before fork.
struct Child<'a> {
    stdio: [RawFd; 3],
    /// The terminal to open as stdio instead, with the session it controls.
    tty: Option<&'a CString>,
    err: RawFd,
    cwd: &'a CString,
    uid: u32,
    gid: u32,
    groups: &'a [u32],
    candidates: &'a [CString],
    explicit: bool,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
}

/// The workload's side of the fork, as runc's init runs it (libcontainer/init_linux.go,
/// finalizeNamespace and setupUser; standard_init_linux.go): stdio, session, working
/// directory, user, then the command, found as Go's exec.LookPath finds it.
///
/// # Safety
/// Only in the child of a fork of a single-threaded process; it never returns.
unsafe fn child(c: &Child<'_>) -> ! {
    /// Reports the failed step, errno and the candidate it was trying to the parent, and
    /// exits.
    ///
    /// # Safety
    /// As for `child`.
    unsafe fn report(err: RawFd, which: u8, candidate: usize) -> ! {
        // SAFETY: async-signal-safe calls on a local buffer.
        unsafe {
            let [a, b, c, d] = (*libc::__errno_location()).to_be_bytes();
            let [e, f, g, h] = u32::try_from(candidate).unwrap_or(u32::MAX).to_be_bytes();
            let report = [which, a, b, c, d, e, f, g, h];
            libc::write(err, report.as_ptr().cast(), report.len());
            libc::_exit(127)
        }
    }
    // SAFETY: async-signal-safe calls, on memory prepared before the fork.
    unsafe {
        let fail = |which: u8| report(c.err, which, 0);
        // Signals as a new process has them: none blocked, none ignored. Rust ignores
        // SIGPIPE in init, and execve keeps ignored signals ignored.
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
        for sig in 1..libc::SIGRTMAX() {
            libc::signal(sig, libc::SIG_DFL);
        }
        libc::setsid();
        let stdio = match c.tty {
            // The session's controlling terminal, as runc makes it (TIOCSCTTY after
            // setsid, libcontainer/init_linux.go).
            Some(path) => {
                let fd = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
                if fd < 0 || libc::ioctl(fd, libc::TIOCSCTTY, 0) != 0 {
                    fail(step::TTY);
                }
                [fd; 3]
            }
            None => c.stdio,
        };
        for (i, &fd) in stdio.iter().enumerate() {
            if libc::dup2(fd, i as libc::c_int) < 0 {
                fail(step::EXEC);
            }
        }
        if c.tty.is_some() && stdio[0] > 2 {
            libc::close(stdio[0]);
        }
        // As root first; if root may not, again as the user (runc does the same).
        let mut chdir_ok = libc::chdir(c.cwd.as_ptr()) == 0;
        if libc::setgroups(c.groups.len(), c.groups.as_ptr()) != 0
            || libc::setgid(c.gid) != 0
            || libc::setuid(c.uid) != 0
        {
            fail(step::USER);
        }
        if !chdir_ok {
            chdir_ok = libc::chdir(c.cwd.as_ptr()) == 0;
        }
        if !chdir_ok {
            fail(step::CHDIR);
        }
        for (i, path) in c.candidates.iter().enumerate() {
            // Go's findExecutable: a file that is not a directory, executable by us.
            let mut st: libc::stat = std::mem::zeroed();
            if libc::stat(path.as_ptr(), &mut st) != 0 {
                if c.explicit {
                    fail(step::STAT);
                }
                continue;
            }
            if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
                if c.explicit {
                    *libc::__errno_location() = libc::EISDIR;
                    fail(step::ACCESS);
                }
                continue;
            }
            if libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::X_OK, libc::AT_EACCESS) != 0 {
                if c.explicit {
                    fail(step::ACCESS);
                }
                continue;
            }
            libc::execve(path.as_ptr(), c.argv, c.envp);
            report(c.err, step::EXEC, i);
        }
        fail(step::NOT_IN_PATH)
    }
}

fn set_nonblocking(fd: RawFd, on: bool) {
    // SAFETY: fcntl(2) on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            let flags = if on {
                flags | libc::O_NONBLOCK
            } else {
                flags & !libc::O_NONBLOCK
            };
            libc::fcntl(fd, libc::F_SETFL, flags);
        }
    }
}

fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buf` is valid for its length.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    usize::try_from(n).map_err(|_| io::Error::last_os_error())
}

fn write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: `buf` is valid for its length.
    let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    match usize::try_from(n) {
        Ok(n) => Ok(n),
        Err(_) => {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::WouldBlock {
                Ok(0)
            } else {
                Err(e)
            }
        }
    }
}
