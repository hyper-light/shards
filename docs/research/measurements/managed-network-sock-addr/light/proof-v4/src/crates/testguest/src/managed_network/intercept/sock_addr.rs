//! One finite, coordinator-run cgroup socket-address comparison. Not a relay.
use super::*;
use std::fs::{self, OpenOptions};
use std::net::SocketAddrV4;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use udp::{DatagramWorker, Exchange, Mode};

mod bpf;

#[derive(Debug)]
pub(in super::super) struct Inputs(Role);

#[derive(Debug)]
enum Role {
    Controller {
        mode: Mode,
        within: Duration,
    },
    Client {
        mode: Mode,
        within: Duration,
        destination: SocketAddrV4,
        expected: Reply,
    },
}
impl Inputs {
    pub(in super::super) fn parse(words: &[&str]) -> io::Result<Self> {
        let mode = |word| match word {
            "connected" => Ok(Mode::Connected),
            "unconnected" => Ok(Mode::Unconnected),
            _ => Err(invalid("socket-address mode must be connected or unconnected")),
        };
        let within = |word: &str| -> io::Result<Duration> {
            let duration =
                Duration::from_millis(word.parse().map_err(|_| invalid("deadline is milliseconds"))?);
            Deadline::new(duration)?;
            Ok(duration)
        };
        let role: io::Result<Role> = match words {
            ["sock-addr-udp4", how, millis] => Ok(Role::Controller {
                mode: mode(how)?,
                within: within(millis)?,
            }),
            ["sock-addr-client4", how, millis, target, role] => {
                let destination: SocketAddrV4 =
                    target.parse().map_err(|_| invalid("expected IPv4 endpoint"))?;
                if *destination.ip() != Ipv4Addr::LOCALHOST || destination.port() == 0 {
                    return Err(invalid(
                        "socket-address fixture selects an assigned loopback port",
                    ));
                }
                Ok(Role::Client {
                    mode: mode(how)?,
                    within: within(millis)?,
                    destination,
                    expected: match *role {
                        "Backend" => Reply::Backend,
                        "Relay" => Reply::Relay,
                        _ => return Err(invalid("expected Backend or Relay")),
                    },
                })
            }
            _ => Err(invalid("use sock-addr-udp4 {connected|unconnected} DEADLINE_MS")),
        };
        role.map(Self)
    }
}

#[derive(Clone, Copy, Debug)]
enum Stage {
    Namespace,
    Cgroup,
    BindBackend,
    BindResponder,
    Baseline,
    Emit,
    LoadLink,
    ExecClient,
    WaitChild,
    ReapChild,
    BindClient,
    Connect,
    GetPeer,
    Send,
    Receive,
    Record,
    Compatibility,
    JoinWorkers,
    Detach,
    RemoveCgroup,
}
fn error(stage: Stage, error: &io::Error) {
    let mut out = io::stdout().lock();
    let _ = write!(
        out,
        "managed-sock-addr {{\"type\":\"Error\",\"stage\":\"{stage:?}\",\"kind\":\"{:?}\",\"errno\":",
        error.kind()
    );
    match error.raw_os_error() {
        Some(errno) => {
            let _ = write!(out, "{errno}");
        }
        None => {
            let _ = write!(out, "null");
        }
    }
    let _ = writeln!(out, "}}");
}
fn at<T>(stage: Stage, result: io::Result<T>) -> io::Result<T> {
    result.inspect_err(|e| error(stage, e))
}
fn record(args: std::fmt::Arguments<'_>) -> io::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "managed-sock-addr {args}")?;
    out.flush()
}

pub(in super::super) fn run(input: Inputs) -> io::Result<()> {
    match input.0 {
        Role::Controller { mode, within } => controller(mode, within),
        Role::Client {
            mode,
            within,
            destination,
            expected,
        } => {
            if std::process::id() == 1 {
                return Err(invalid("client must be a fresh exec child"));
            }
            client(mode, destination.into(), expected, &Deadline::new(within)?)
        }
    }
}

// A mismatch is accumulated, not returned early: both declared datagrams remain
// observable. Errors stop only unavailable I/O; no success is inferred from them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Compatibility {
    Exact,
    Mismatch,
}
impl Compatibility {
    fn observe(&mut self, exact: bool) {
        if !exact {
            *self = Self::Mismatch;
        }
    }
}
fn peer_exact(mode: Mode, actual: &io::Result<SocketAddr>, requested: SocketAddr) -> bool {
    match (mode, actual) {
        (Mode::Connected, Ok(peer)) => *peer == requested,
        (Mode::Unconnected, Err(error)) => error.kind() == io::ErrorKind::NotConnected,
        _ => false,
    }
}
fn arrival_exact(
    arrival: &UdpArrival,
    expected: &[u8],
    local: SocketAddr,
    original_peer: SocketAddr,
) -> bool {
    arrival.payload_matches(expected)
        && arrival
            .metadata
            .as_ref()
            .is_ok_and(|metadata| metadata.peer == original_peer && metadata.original == Some(local))
}
fn client(mode: Mode, destination: SocketAddr, expected: Reply, deadline: &Deadline) -> io::Result<()> {
    let mut stage = Stage::BindClient;
    let mut completed = 0usize;
    let mut compatibility = Compatibility::Exact;
    let result = (|| {
        let socket = udp_socket((Ipv4Addr::LOCALHOST, 0).into(), SocketMode::Native)?;
        let local = socket.local_addr()?;
        if mode == Mode::Connected {
            stage = Stage::Connect;
            socket.connect(destination)?;
        }
        stage = Stage::GetPeer;
        let peer = socket.peer_addr();
        // Actual getpeername precedes any compatibility verdict; no repair hook.
        let mut out = io::stdout().lock();
        write!(
            out,
            "managed-sock-addr {{\"type\":\"ClientIdentity\",\"pid\":{},\"namespace\":{},\"requested\":\"{destination}\",\"local\":\"{local}\",\"mode\":\"{mode:?}\",\"peer\":",
            std::process::id(),
            File::open("/proc/self/ns/net")?.metadata()?.ino()
        )?;
        match &peer {
            Ok(peer) => write!(out, "\"{peer}\",\"peer_error\":null")?,
            Err(e) => write!(out, "null,\"peer_error\":\"{:?}\"", e.kind())?,
        }
        write!(out, ",\"cgroup_hex\":\"")?;
        udp::hex(&mut out, &fs::read("/proc/self/cgroup")?)?;
        writeln!(out, "\"}}")?;
        out.flush()?;
        drop(out);
        compatibility.observe(peer_exact(mode, &peer, destination));
        for (index, step) in Exchange::SCRIPT.into_iter().enumerate() {
            stage = Stage::Send;
            let request = step.request();
            let sent = deadline.io(socket.as_raw_fd(), libc::POLLOUT, || match mode {
                Mode::Connected => socket.send(request),
                Mode::Unconnected => socket.send_to(request, destination),
            })?;
            if sent != request.len() {
                return Err(invalid("partial UDP send"));
            }
            stage = Stage::Receive;
            let arrival = udp_arrival(
                &socket,
                Reply::Backend.bytes().len().max(Reply::Relay.bytes().len()),
                deadline,
            )?;
            stage = Stage::Record;
            udp::arrival_record(
                if expected == Reply::Backend {
                    "SockAddrBaselineClient"
                } else {
                    "SockAddrHookClient"
                },
                step,
                index + 1,
                local,
                &arrival,
            )?;
            compatibility.observe(arrival_exact(
                &arrival,
                step.response(expected),
                local,
                destination,
            ));
            completed = completed
                .checked_add(1)
                .ok_or_else(|| invalid("exchange count overflow"))?;
        }
        stage = Stage::Compatibility;
        if compatibility != Compatibility::Exact {
            return Err(invalid(
                "original endpoint/port or datagram compatibility differs",
            ));
        }
        Ok(())
    })();
    if let Err(e) = &result {
        error(stage, e);
    }
    record(format_args!(
        "{{\"type\":\"ClientCompletion\",\"expected\":\"{expected:?}\",\"stage\":\"{stage:?}\",\"exchanges\":{completed},\"compatibility\":\"{compatibility:?}\",\"script\":\"{}\",\"success\":{}}}",
        if completed == Exchange::SCRIPT.len() {
            "Complete"
        } else {
            "Incomplete"
        },
        result.is_ok()
    ))?;
    result
}

enum ChildState {
    Running,
    Reaped,
}
struct OwnedClient {
    child: Child,
    state: ChildState,
}
impl OwnedClient {
    fn reap(&mut self) -> ExitStatus {
        loop {
            match self.child.wait() {
                Ok(status) => {
                    self.state = ChildState::Reaped;
                    return status;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    error(Stage::ReapChild, &e);
                    let _ = record(format_args!(
                        "{{\"type\":\"ChildCleanup\",\"state\":\"Unknown\",\"reaped\":false}}"
                    ));
                    // No release of links/cgroup/namespaces with unknown ownership.
                    // The coordinator's outer VM deadline bounds this pending state.
                    loop {
                        std::thread::park();
                    }
                }
            }
        }
    }
}
impl Drop for OwnedClient {
    fn drop(&mut self) {
        if matches!(self.state, ChildState::Running) {
            let killed = self.child.kill();
            if let Err(e) = &killed {
                error(Stage::ReapChild, e);
            }
            let _ = record(format_args!(
                "{{\"type\":\"ChildCleanup\",\"state\":\"Pending\",\"kill_ok\":{},\"reaped\":false}}",
                killed.is_ok()
            ));
            let status = self.reap();
            let _ = record(format_args!(
                "{{\"type\":\"ChildCleanup\",\"state\":\"Reaped\",\"success\":{}}}",
                status.success()
            ));
        }
    }
}

fn exec_client(
    namespace: &Namespace,
    cgroup: &Cgroup,
    mode: Mode,
    destination: SocketAddrV4,
    expected: Reply,
    deadline: &Deadline,
) -> io::Result<bool> {
    let path = PathBuf::from(match expected {
        Reply::Backend => "/tmp/sock-addr-baseline.report",
        Reply::Relay => "/tmp/sock-addr-hook.report",
    });
    let output = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let how = match mode {
        Mode::Connected => "connected",
        Mode::Unconnected => "unconnected",
    };
    let mut command = Command::new(std::env::current_exe()?);
    // The parent pidfd wait enforces the original absolute work deadline. This
    // subordinate child budget cannot extend it across exec/scheduling delay.
    let millis = deadline.left()?.as_millis().max(1).to_string();
    command
        .args([
            "managed-network",
            "sock-addr-client4",
            how,
            &millis,
            &destination.to_string(),
            match expected {
                Reply::Backend => "Backend",
                Reply::Relay => "Relay",
            },
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output.try_clone()?));
    let netfd = namespace.handle.as_raw_fd();
    let procs = cgroup.procs.as_raw_fd();
    // SAFETY: only async-signal-safe raw syscalls after fork. Both held FDs are
    // CLOEXEC; no socket exists in this child before cgroup placement and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::setns(netfd, libc::CLONE_NEWNET) != 0 {
                return Err(io::Error::last_os_error());
            }
            let n = libc::write(procs, c"0".as_ptr().cast(), 1);
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n != 1 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            Ok(())
        });
    }
    let mut owned = OwnedClient {
        child: at(Stage::ExecClient, command.spawn())?,
        state: ChildState::Running,
    };
    record(format_args!(
        "{{\"type\":\"Child\",\"pid\":{},\"role\":\"{expected:?}\",\"state\":\"ExecReturned\"}}",
        owned.child.id()
    ))?;
    // SAFETY: child is owned and unreaped, so its PID cannot be reused here.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, owned.child.id(), 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pidfd_open returned a new CLOEXEC descriptor.
    let pidfd = unsafe { OwnedFd::from_raw_fd(i32::try_from(fd).map_err(io::Error::other)?) };
    let wait = at(
        Stage::WaitChild,
        deadline.io(pidfd.as_raw_fd(), libc::POLLIN, || {
            let mut poll = libc::pollfd {
                fd: pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one initialized pollfd, zero-time readiness check before wait.
            if unsafe { libc::poll(&raw mut poll, 1, 0) } < 0 {
                return Err(io::Error::last_os_error());
            }
            if poll.revents != 0 {
                Ok(())
            } else {
                Err(io::ErrorKind::WouldBlock.into())
            }
        }),
    );
    if wait.is_err() {
        let killed = owned.child.kill();
        if let Err(e) = &killed {
            error(Stage::ReapChild, e);
        }
        record(format_args!(
            "{{\"type\":\"ChildCleanup\",\"state\":\"Pending\",\"kill_ok\":{},\"reaped\":false}}",
            killed.is_ok()
        ))?;
    }
    let status = owned.reap();
    // Copy the finite child report only after reap: no detached pipe reader and
    // no cross-process console writes erase actual tuple/datagram observations.
    let mut reader = output;
    use std::io::{Seek, SeekFrom};
    reader.seek(SeekFrom::Start(0))?;
    io::copy(&mut reader, &mut io::stdout().lock())?;
    io::stdout().flush()?;
    record(format_args!(
        "{{\"type\":\"ChildReaped\",\"role\":\"{expected:?}\",\"reaped\":true,\"success\":{},\"code\":{}}}",
        status.success(),
        status.code().map_or_else(|| "null".to_owned(), |c| c.to_string())
    ))?;
    fs::remove_file(path)?;
    wait?;
    Ok(status.success())
}

// The current mount namespace/PID1 remains untouched. Only this new mount
// namespace administers its cgroup2 mount and one owned, initially empty leaf.
fn in_mount<T: Send>(namespace: Option<&File>, f: impl FnOnce() -> io::Result<T> + Send) -> io::Result<T> {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .spawn_scoped(scope, move || {
                // SAFETY: dedicated new thread; never changes the caller's namespace.
                let rc = unsafe {
                    match namespace {
                        Some(ns) => libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNS),
                        None => libc::unshare(libc::CLONE_NEWNS),
                    }
                };
                if rc != 0 {
                    return Err(io::Error::last_os_error());
                }
                f()
            })?
            .join()
            .map_err(|_| io::Error::other("mount namespace worker panicked"))?
    })
}
#[derive(Clone, Copy, Debug)]
enum MountState {
    Directory,
    Mounted,
    Leaf,
    Removed,
}
struct MountScope {
    namespace: File,
    state: MountState,
}
const MOUNT: &std::ffi::CStr = c"/tmp/shards-sock-addr-cgroup";
const LEAF: &str = "/tmp/shards-sock-addr-cgroup/sock-addr-client";
impl MountScope {
    fn cleanup(&mut self) -> io::Result<()> {
        let namespace = self.namespace.try_clone()?;
        in_mount(Some(&namespace), || {
            if matches!(self.state, MountState::Leaf) {
                fs::remove_dir(LEAF)?;
                self.state = MountState::Mounted;
            }
            if matches!(self.state, MountState::Mounted) {
                // SAFETY: only the fixture's fresh mount in its held namespace.
                if unsafe { libc::umount2(MOUNT.as_ptr(), 0) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                self.state = MountState::Directory;
            }
            if matches!(self.state, MountState::Directory) {
                fs::remove_dir(MOUNT.to_str().map_err(io::Error::other)?)?;
                self.state = MountState::Removed;
            }
            Ok(())
        })
    }
}
impl Drop for MountScope {
    fn drop(&mut self) {
        if !matches!(self.state, MountState::Removed)
            && let Err(e) = self.cleanup()
        {
            error(Stage::RemoveCgroup, &e);
        }
    }
}
struct Cgroup {
    handle: File,
    procs: File,
    scope: MountScope,
}
impl Cgroup {
    fn cleanup(self) -> io::Result<()> {
        let Self {
            handle,
            procs,
            mut scope,
        } = self;
        drop(procs);
        drop(handle);
        scope.cleanup()
    }
    fn new() -> io::Result<Self> {
        in_mount(None, || {
            let namespace = File::open("/proc/thread-self/ns/mnt")?;
            fs::create_dir(MOUNT.to_str().map_err(io::Error::other)?)?;
            let mut scope = MountScope {
                namespace,
                state: MountState::Directory,
            };
            // SAFETY: private propagation and cgroup2 mount only in this NEW ns.
            if unsafe {
                libc::mount(
                    std::ptr::null(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            if unsafe {
                libc::mount(
                    c"none".as_ptr(),
                    MOUNT.as_ptr(),
                    c"cgroup2".as_ptr(),
                    0,
                    std::ptr::null(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            scope.state = MountState::Mounted;
            fs::create_dir(LEAF)?;
            scope.state = MountState::Leaf;
            Ok(Self {
                scope,
                handle: File::open(LEAF)?,
                procs: OpenOptions::new()
                    .write(true)
                    .open(PathBuf::from(LEAF).join("cgroup.procs"))?,
            })
        })
    }
}

fn controller(mode: Mode, within: Duration) -> io::Result<()> {
    if std::process::id() != 1 {
        return Err(invalid("controller requires direct guest PID1"));
    }
    let deadline = Deadline::new(within)?;
    let namespace = at(Stage::Namespace, Namespace::new(&deadline))?;
    let cgroup = at(Stage::Cgroup, Cgroup::new())?;
    let socket = |stage| {
        at(
            stage,
            in_namespace(Some(&namespace.handle), || {
                udp_socket((Ipv4Addr::LOCALHOST, 0).into(), SocketMode::Native)
            }),
        )
    };
    let backend = socket(Stage::BindBackend)?;
    let responder = socket(Stage::BindResponder)?;
    let destination = match backend.local_addr()? {
        SocketAddr::V4(at) => at,
        _ => return Err(invalid("IPv4 backend required")),
    };
    let rewritten = match responder.local_addr()? {
        SocketAddr::V4(at) => at,
        _ => return Err(invalid("IPv4 responder required")),
    };
    record(format_args!(
        "{{\"type\":\"Target\",\"mode\":\"{mode:?}\",\"namespace\":{},\"cgroup_inode\":{},\"backend\":\"{destination}\",\"responder\":\"{rewritten}\",\"address_hooks\":\"DestinationOnly\",\"presentation_hooks\":\"None\"}}",
        namespace.inode()?,
        cgroup.handle.metadata()?.ino()
    ))?;
    let mut backend = DatagramWorker::start(
        backend.try_clone()?,
        backend,
        destination.into(),
        Reply::Backend,
        &deadline,
    )?;
    let mut relay = None;
    let mut link = None;
    let result = (|| {
        if !at(
            Stage::Baseline,
            exec_client(&namespace, &cgroup, mode, destination, Reply::Backend, &deadline),
        )? {
            return Err(invalid("baseline native script failed"));
        }
        let bytes = at(Stage::Emit, bpf::program(destination, rewritten))?;
        let mut out = io::stdout().lock();
        write!(
            out,
            "managed-sock-addr {{\"type\":\"Program\",\"bytecode_hex\":\""
        )?;
        udp::hex(&mut out, &bytes)?;
        writeln!(out, "\",\"verifier_log\":\"NotRequested\"}}")?;
        out.flush()?;
        drop(out);
        link = Some(at(
            Stage::LoadLink,
            bpf::Link::attach(&bytes, &cgroup.handle, mode),
        )?);
        relay = Some(DatagramWorker::start(
            responder.try_clone()?,
            responder,
            rewritten.into(),
            Reply::Relay,
            &deadline,
        )?);
        if !at(
            Stage::ExecClient,
            exec_client(&namespace, &cgroup, mode, destination, Reply::Relay, &deadline),
        )? {
            return Err(invalid("hooked native compatibility failed"));
        }
        Ok(())
    })();
    // Child reap is completed inside exec_client before any worker/link release.
    let pending = record(format_args!(
        "{{\"type\":\"Cleanup\",\"state\":\"Pending\",\"stage\":\"JoinWorkers\"}}"
    ));
    let backend = at(Stage::JoinWorkers, backend.finish());
    let relay = relay.as_mut().map(DatagramWorker::finish);
    let reports: io::Result<()> = (|| {
        udp::worker_record(Reply::Backend, &backend)?;
        match &relay {
            Some(result) => udp::worker_record(Reply::Relay, result)?,
            None => record(format_args!(
                "{{\"type\":\"Responder\",\"state\":\"NotStarted\"}}"
            ))?,
        }
        Ok(())
    })();
    let detached = match link {
        Some(link) => at(Stage::Detach, link.detach()),
        None => Ok(()),
    };
    let removed = at(Stage::RemoveCgroup, cgroup.cleanup());
    record(format_args!(
        "{{\"type\":\"Cleanup\",\"state\":\"Observed\",\"backend_joined\":{},\"relay_joined\":{},\"link_detached\":{},\"cgroup_removed\":{},\"after_window\":\"Unverified\"}}",
        backend.is_ok(),
        relay.as_ref().is_none_or(Result::is_ok),
        detached.is_ok(),
        removed.is_ok()
    ))?;
    result?;
    pending?;
    reports?;
    let backend = backend?;
    if backend.report.arrivals != Exchange::SCRIPT.len()
        || backend.report.matched != Exchange::SCRIPT.len()
        || backend.report.empty != 1
        || !matches!(backend.result, Ok(udp::Terminal::Cancelled))
    {
        return Err(invalid("backend script/terminal differs"));
    }
    let relay = relay.ok_or_else(|| invalid("responder not started"))??;
    if relay.report.arrivals != Exchange::SCRIPT.len()
        || relay.report.matched != Exchange::SCRIPT.len()
        || relay.report.empty != 1
        || !matches!(relay.result, Ok(udp::Terminal::ScriptComplete))
    {
        return Err(invalid("responder script/terminal differs"));
    }
    detached?;
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finite_parser_and_strict_original_port() {
        for mode in ["connected", "unconnected"] {
            assert!(Inputs::parse(&["sock-addr-udp4", mode, "1000"]).is_ok());
        }
        for words in [
            vec!["sock-addr-udp4", "other", "1000"],
            vec!["sock-addr-udp4", "connected", "0"],
            vec!["sock-addr-client4", "connected", "10", "127.0.0.1:0", "Relay"],
            vec!["sock-addr-client4", "connected", "10", "192.0.2.1:42", "Relay"],
        ] {
            assert!(Inputs::parse(&words).is_err());
        }
        let target: SocketAddr = "127.0.0.1:42".parse().unwrap();
        assert!(peer_exact(Mode::Connected, &Ok(target), target));
        assert!(!peer_exact(
            Mode::Connected,
            &Ok("127.0.0.1:43".parse().unwrap()),
            target
        ));
        assert!(peer_exact(
            Mode::Unconnected,
            &Err(io::ErrorKind::NotConnected.into()),
            target
        ));
        assert!(!peer_exact(
            Mode::Unconnected,
            &Err(io::ErrorKind::Other.into()),
            target
        ));
        let mut compatibility = Compatibility::Exact;
        compatibility.observe(false);
        compatibility.observe(true);
        assert_eq!(compatibility, Compatibility::Mismatch);
    }
}
