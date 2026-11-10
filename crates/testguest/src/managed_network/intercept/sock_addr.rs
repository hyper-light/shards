//! Finite coordinator-run socket-address comparisons, never product routing.
use super::*;
use std::fs::{self, OpenOptions};
use std::net::SocketAddrV4;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use udp::{DatagramWorker, Exchange, Mode, Work};

mod bpf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presentation {
    DestinationOnly,
    FixedOriginalPeer,
}
impl Presentation {
    fn parse(word: &str) -> io::Result<Self> {
        match word {
            "destination-only" => Ok(Self::DestinationOnly),
            "fixed-original-peer" => Ok(Self::FixedOriginalPeer),
            _ => Err(invalid("profile must be destination-only or fixed-original-peer")),
        }
    }
    fn hooks(self, mode: Mode) -> &'static [bpf::Hook] {
        use bpf::Hook::*;
        match (self, mode) {
            (Self::DestinationOnly, Mode::Connected) => &[Connect4],
            (Self::DestinationOnly, Mode::Unconnected) => &[Sendmsg4],
            (Self::FixedOriginalPeer, Mode::Connected) => &[Connect4, Recvmsg4, Getpeername4],
            (Self::FixedOriginalPeer, Mode::Unconnected) => &[Sendmsg4, Recvmsg4],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HookSetup {
    NotStarted,
    Partial,
    Ready,
}

#[derive(Clone, Copy, Debug)]
enum ControllerKind {
    ScriptedResponder,
    BufferedRelay { buffer_bytes: usize },
    CancelUpstreamReceive { buffer_bytes: usize },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientRole {
    BaselineBackend,
    ScriptedResponder,
    ForwardedBackend,
}
impl ClientRole {
    fn reply(self) -> Reply {
        match self {
            Self::BaselineBackend | Self::ForwardedBackend => Reply::Backend,
            Self::ScriptedResponder => Reply::Relay,
        }
    }
    fn argument(self) -> &'static str {
        match self {
            Self::BaselineBackend => "Backend",
            Self::ScriptedResponder => "Relay",
            Self::ForwardedBackend => "ForwardedBackend",
        }
    }
    fn arrival_role(self) -> &'static str {
        match self {
            Self::BaselineBackend => "SockAddrBaselineClient",
            Self::ScriptedResponder => "SockAddrHookClient",
            Self::ForwardedBackend => "SockAddrForwardedClient",
        }
    }
}

#[derive(Debug)]
pub(in super::super) struct Inputs(Role);

#[derive(Debug)]
enum Role {
    Controller {
        kind: ControllerKind,
        presentation: Presentation,
        mode: Mode,
        within: Duration,
    },
    Client {
        mode: Mode,
        within: Duration,
        destination: SocketAddrV4,
        role: ClientRole,
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
            ["sock-addr-udp4", how, millis, profile] => Ok(Role::Controller {
                kind: ControllerKind::ScriptedResponder,
                presentation: Presentation::parse(profile)?,
                mode: mode(how)?,
                within: within(millis)?,
            }),
            ["sock-addr-relay4", how, millis, profile, budget] => Ok(Role::Controller {
                kind: ControllerKind::BufferedRelay {
                    buffer_bytes: budget
                        .parse()
                        .map_err(|_| invalid("buffer envelope must be usize decimal bytes"))?,
                },
                presentation: Presentation::parse(profile)?,
                mode: mode(how)?,
                within: within(millis)?,
            }),
            ["sock-addr-cancel4", how, millis, budget] => Ok(Role::Controller {
                kind: ControllerKind::CancelUpstreamReceive {
                    buffer_bytes: budget
                        .parse()
                        .map_err(|_| invalid("buffer envelope must be usize decimal bytes"))?,
                },
                presentation: Presentation::FixedOriginalPeer,
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
                    role: match *role {
                        "Backend" => ClientRole::BaselineBackend,
                        "Relay" => ClientRole::ScriptedResponder,
                        "ForwardedBackend" => ClientRole::ForwardedBackend,
                        _ => return Err(invalid("expected Backend, Relay or ForwardedBackend")),
                    },
                })
            }
            _ => Err(invalid(
                "use sock-addr-udp4 MODE DEADLINE_MS PROFILE or sock-addr-relay4 MODE DEADLINE_MS PROFILE BUFFER_BYTES",
            )),
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
    ReserveHooks,
    ReservePayload,
    BindUpstream,
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
    RemoveCgroup,
    AwaitCancellationBoundary,
    CancelRelay,
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
        Role::Controller {
            kind,
            presentation,
            mode,
            within,
        } => controller(kind, presentation, mode, within),
        Role::Client {
            mode,
            within,
            destination,
            role,
        } => {
            if std::process::id() == 1 {
                return Err(invalid("client must be a fresh exec child"));
            }
            client(mode, destination.into(), role, &Deadline::new(within)?)
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
fn client(mode: Mode, destination: SocketAddr, role: ClientRole, deadline: &Deadline) -> io::Result<()> {
    let expected = role.reply();
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
            "managed-sock-addr {{\"type\":\"ClientIdentity\",\"pid\":{},\"namespace\":{},\"requested\":\"{destination}\",\"local\":\"{local}\",\"mode\":\"{mode:?}\",\"role\":\"{role:?}\",\"peer\":",
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
            udp::arrival_record(role.arrival_role(), step, index + 1, local, arrival.view())?;
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
        "{{\"type\":\"ClientCompletion\",\"role\":\"{role:?}\",\"expected\":\"{expected:?}\",\"stage\":\"{stage:?}\",\"exchanges\":{completed},\"compatibility\":\"{compatibility:?}\",\"script\":\"{}\",\"success\":{}}}",
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

fn start_client(
    namespace: &Namespace,
    cgroup: &Cgroup,
    mode: Mode,
    destination: SocketAddrV4,
    role: ClientRole,
    deadline: &Deadline,
) -> io::Result<RunningClient> {
    let path = PathBuf::from(match role {
        ClientRole::BaselineBackend => "/tmp/sock-addr-baseline.report",
        ClientRole::ScriptedResponder | ClientRole::ForwardedBackend => "/tmp/sock-addr-hook.report",
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
            role.argument(),
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
    let role = role.argument();
    let owned = OwnedClient {
        child: at(Stage::ExecClient, command.spawn())?,
        state: ChildState::Running,
    };
    record(format_args!(
        "{{\"type\":\"Child\",\"pid\":{},\"role\":\"{role}\",\"state\":\"ExecReturned\"}}",
        owned.child.id()
    ))?;
    // SAFETY: child is owned and unreaped, so its PID cannot be reused here.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, owned.child.id(), 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pidfd_open returned a new CLOEXEC descriptor.
    let pidfd = unsafe { OwnedFd::from_raw_fd(i32::try_from(fd).map_err(io::Error::other)?) };
    Ok(RunningClient {
        owned,
        pidfd,
        output,
        path,
        role,
    })
}
struct RunningClient {
    owned: OwnedClient,
    pidfd: OwnedFd,
    output: File,
    path: PathBuf,
    role: &'static str,
}
#[derive(Clone, Copy, Debug)]
enum ChildEnd {
    Natural,
    Cancelled,
}
impl RunningClient {
    fn finish(mut self, deadline: &Deadline, end: ChildEnd) -> io::Result<ExitStatus> {
        let role = self.role;
        if matches!(end, ChildEnd::Cancelled) {
            self.owned.child.kill()?;
            record(format_args!(
                "{{\"type\":\"ChildCancellation\",\"role\":\"{role}\",\"requested\":true}}"
            ))?;
        }
        let wait = at(
            Stage::WaitChild,
            deadline.io(self.pidfd.as_raw_fd(), libc::POLLIN, || {
                let mut poll = libc::pollfd {
                    fd: self.pidfd.as_raw_fd(),
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
            let killed = self.owned.child.kill();
            if let Err(e) = &killed {
                error(Stage::ReapChild, e);
            }
            record(format_args!(
                "{{\"type\":\"ChildCleanup\",\"state\":\"Pending\",\"kill_ok\":{},\"reaped\":false}}",
                killed.is_ok()
            ))?;
        }
        let status = self.owned.reap();
        // Copy the finite child report only after reap: no detached pipe reader and
        // no cross-process console writes erase actual tuple/datagram observations.
        let mut reader = self.output;
        use std::io::{Seek, SeekFrom};
        reader.seek(SeekFrom::Start(0))?;
        io::copy(&mut reader, &mut io::stdout().lock())?;
        io::stdout().flush()?;
        record(format_args!(
            "{{\"type\":\"ChildReaped\",\"role\":\"{role}\",\"reaped\":true,\"success\":{},\"code\":{},\"signal\":{}}}",
            status.success(),
            status.code().map_or_else(|| "null".to_owned(), |c| c.to_string()),
            status
                .signal()
                .map_or_else(|| "null".to_owned(), |signal| signal.to_string())
        ))?;
        fs::remove_file(self.path)?;
        wait?;
        Ok(status)
    }
}
fn exec_client(
    namespace: &Namespace,
    cgroup: &Cgroup,
    mode: Mode,
    destination: SocketAddrV4,
    role: ClientRole,
    deadline: &Deadline,
) -> io::Result<bool> {
    start_client(namespace, cgroup, mode, destination, role, deadline)?
        .finish(deadline, ChildEnd::Natural)
        .map(|status| status.success())
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
                        Some(ns) => {
                            // setns(2), Linux v6.18 mntns_install require unshared fs_struct.
                            if libc::unshare(libc::CLONE_FS) != 0 {
                                return Err(io::Error::last_os_error());
                            }
                            libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNS)
                        }
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
            // Direct initramfs boot need not provide the scratch parent; MOUNT stays exclusive.
            fs::create_dir_all("/tmp")?;
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
            // SAFETY: cgroup2 target is the fresh fixture path in this NEW mount namespace.
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

fn hook_error(failure: bpf::HookFailure) -> io::Error {
    let _ = failure.write_record(&mut io::stdout().lock());
    failure.cause
}

fn controller(
    kind: ControllerKind,
    presentation: Presentation,
    mode: Mode,
    within: Duration,
) -> io::Result<()> {
    if std::process::id() != 1 {
        return Err(invalid("controller requires direct guest PID1"));
    }
    let deadline = Deadline::new(within)?;
    enum Payload {
        Scripted,
        Buffered(udp::forward::Slots),
    }
    let payload = match kind {
        ControllerKind::ScriptedResponder => Payload::Scripted,
        ControllerKind::BufferedRelay { buffer_bytes }
        | ControllerKind::CancelUpstreamReceive { buffer_bytes } => {
            let slots = at(
                Stage::ReservePayload,
                udp::forward::Slots::reserve(buffer_bytes).map_err(udp::forward::ReserveError::into_io),
            )?;
            slots.record()?;
            Payload::Buffered(slots)
        }
    };
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
    let operation = match kind {
        ControllerKind::ScriptedResponder => "ScriptedResponder",
        ControllerKind::BufferedRelay { .. } => "BufferedRelay",
        ControllerKind::CancelUpstreamReceive { .. } => "CancelUpstreamReceive",
    };
    record(format_args!(
        "{{\"type\":\"Target\",\"mode\":\"{mode:?}\",\"namespace\":{},\"cgroup_inode\":{},\"backend\":\"{destination}\",\"responder\":\"{rewritten}\",\"profile\":\"{presentation:?}\",\"operation\":\"{operation}\"}}",
        namespace.inode()?,
        cgroup.handle.metadata()?.ino()
    ))?;
    let plan = presentation.hooks(mode);
    let mut links = Vec::new();
    at(
        Stage::ReserveHooks,
        links.try_reserve_exact(plan.len()).map_err(io::Error::other),
    )?;
    let mut hook_setup = HookSetup::NotStarted;
    let comparison_role = match kind {
        ControllerKind::ScriptedResponder => ClientRole::ScriptedResponder,
        ControllerKind::BufferedRelay { .. } | ControllerKind::CancelUpstreamReceive { .. } => {
            ClientRole::ForwardedBackend
        }
    };
    // Only this explicit coordinator fixture has the two finite boundary signals.
    let cancellation = if matches!(kind, ControllerKind::CancelUpstreamReceive { .. }) {
        Some((udp::notification()?, udp::notification()?))
    } else {
        None
    };
    let work = match payload {
        Payload::Scripted => Work::Scripted {
            receiver: responder.try_clone()?,
            sender: responder,
            destination: rewritten.into(),
            role: Reply::Relay,
        },
        Payload::Buffered(slots) => {
            let upstream = socket(Stage::BindUpstream)?;
            at(Stage::BindUpstream, upstream.connect(destination))?;
            record(format_args!(
                "{{\"type\":\"ForwardTarget\",\"ingress\":\"{rewritten}\",\"upstream\":\"{}\",\"backend\":\"{destination}\",\"upstream_peer\":\"{}\",\"source_preservation\":\"MustObserveBackend\"}}",
                upstream.local_addr()?,
                upstream.peer_addr()?
            ))?;
            Work::Forwarded(udp::forward::Inputs {
                ingress: responder,
                upstream,
                backend: destination.into(),
                slots,
                receive_entered: cancellation
                    .as_ref()
                    .map(|(entered, _)| entered.try_clone())
                    .transpose()?,
            })
        }
    };
    let scripts = [ClientRole::BaselineBackend, comparison_role];
    let backend_scripts = scripts
        .into_iter()
        .filter(|role| role.reply() == Reply::Backend)
        .count();
    let expected_backend = Exchange::SCRIPT
        .len()
        .checked_mul(backend_scripts)
        .ok_or_else(|| invalid("expected backend count overflow"))?;
    let expected_empty = Exchange::SCRIPT
        .into_iter()
        .filter(|step| matches!(step, Exchange::Empty))
        .count()
        .checked_mul(backend_scripts)
        .ok_or_else(|| invalid("expected empty count overflow"))?;
    let backend_work = match &cancellation {
        Some((_, receipt)) => Work::HeldReply {
            receiver: backend.try_clone()?,
            sender: backend,
            destination: destination.into(),
            receipt: receipt.try_clone()?,
        },
        None => Work::Scripted {
            receiver: backend.try_clone()?,
            sender: backend,
            destination: destination.into(),
            role: Reply::Backend,
        },
    };
    let mut backend = DatagramWorker::start(backend_work, &deadline)?;
    let mut relay = None;
    let result = (|| {
        if !at(
            Stage::Baseline,
            exec_client(
                &namespace,
                &cgroup,
                mode,
                destination,
                ClientRole::BaselineBackend,
                &deadline,
            ),
        )? {
            return Err(invalid("baseline native script failed"));
        }
        for &hook in plan {
            let (from, to) = hook.endpoints(destination, rewritten);
            let bytes = bpf::program(from, to).map_err(|cause| {
                hook_error(bpf::HookFailure {
                    hook,
                    stage: bpf::HookStage::Emit,
                    cause,
                })
            })?;
            let mut out = io::stdout().lock();
            write!(
                out,
                "managed-sock-addr {{\"type\":\"Program\",\"hook\":\"{hook:?}\",\"from\":\"{from}\",\"to\":\"{to}\",\"bytecode_hex\":\""
            )?;
            udp::hex(&mut out, &bytes)?;
            writeln!(out, "\",\"verifier_log\":\"NotRequested\"}}")?;
            out.flush()?;
            drop(out);
            let link = bpf::Link::attach(&bytes, &cgroup.handle, hook).map_err(hook_error)?;
            // Reservation precedes loading; every acquired link enters this owner
            // before any reporting failure can leave the setup closure.
            links.push(link);
            hook_setup = HookSetup::Partial;
            record(format_args!(
                "{{\"type\":\"LinkAttached\",\"hook\":\"{hook:?}\"}}"
            ))?;
        }
        hook_setup = HookSetup::Ready;
        relay = Some(DatagramWorker::start(work, &deadline)?);
        if let Some((entered, receipt)) = &cancellation {
            let client = start_client(&namespace, &cgroup, mode, destination, comparison_role, &deadline)?;
            // Both independent receipt and explicit stage entry must precede the
            // cancel request. Neither notification claims an observed kernel sleep.
            let boundary = (|| {
                at(
                    Stage::AwaitCancellationBoundary,
                    udp::notified(receipt, &deadline),
                )?;
                at(
                    Stage::AwaitCancellationBoundary,
                    udp::notified(entered, &deadline),
                )?;
                let left = deadline.left()?;
                let worker = relay
                    .as_ref()
                    .ok_or_else(|| invalid("relay worker not started"))?;
                at(Stage::CancelRelay, worker.request_cancel(&deadline))?;
                record(format_args!(
                    "{{\"type\":\"CancelBoundary\",\"state\":\"Requested\",\"stage\":\"UpstreamReceive\",\"receipt\":\"ValidatedBackendDatagram\",\"work_remaining_ns\":{},\"kernel_sleep\":\"Unobserved\"}}",
                    left.as_nanos()
                ))
            })();
            // Even a failed boundary/cancel report ends and reaps this owned child
            // before the existing worker/link/cgroup cleanup path proceeds.
            let status = client.finish(&deadline, ChildEnd::Cancelled);
            boundary?;
            if status?.signal() != Some(libc::SIGKILL) {
                return Err(invalid("waiting client was not reaped after owned cancellation"));
            }
            return Ok(());
        }
        if !at(
            Stage::ExecClient,
            exec_client(&namespace, &cgroup, mode, destination, comparison_role, &deadline),
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
    let cancellation_deadline = if cancellation.is_some() && result.is_ok() {
        deadline.left().and_then(|left| record(format_args!("{{\"type\":\"CancelBoundary\",\"state\":\"JoinedWithinWorkDeadline\",\"work_remaining_ns\":{}}}", left.as_nanos())))
    } else {
        Ok(())
    };
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
    let acquired = links.len();
    let mut detach_failure = None;
    let mut detach_report_failure = None;
    for link in links {
        let hook = link.hook;
        let detached = link.detach();
        let reported = match &detached {
            Ok(()) => record(format_args!(
                "{{\"type\":\"LinkDetached\",\"hook\":\"{hook:?}\",\"detached\":true}}"
            )),
            Err(failure) => failure.write_record(&mut io::stdout().lock()),
        };
        // Neither a failed detach nor a failed record can skip another owned link.
        if let Err(error) = reported
            && detach_report_failure.is_none()
        {
            detach_report_failure = Some(error);
        }
        if let Err(failure) = detached
            && detach_failure.is_none()
        {
            detach_failure = Some(failure);
        }
    }
    let removed = at(Stage::RemoveCgroup, cgroup.cleanup());
    record(format_args!(
        "{{\"type\":\"Cleanup\",\"state\":\"Observed\",\"backend_joined\":{},\"relay_joined\":{},\"hook_setup\":\"{hook_setup:?}\",\"links_required\":{},\"links_acquired\":{acquired},\"link_detached\":{},\"cgroup_removed\":{},\"after_window\":\"Unverified\"}}",
        backend.is_ok(),
        relay.as_ref().is_none_or(Result::is_ok),
        plan.len(),
        detach_failure.is_none(),
        removed.is_ok()
    ))?;
    result?;
    pending?;
    reports?;
    cancellation_deadline?;
    let backend = backend?.scripted()?;
    let (expected_backend, expected_empty) = if cancellation.is_some() {
        (
            Exchange::SCRIPT
                .len()
                .checked_add(1)
                .ok_or_else(|| invalid("held backend count overflow"))?,
            Exchange::SCRIPT
                .into_iter()
                .filter(|step| matches!(step, Exchange::Empty))
                .count(),
        )
    } else {
        (expected_backend, expected_empty)
    };
    if backend.report.arrivals != expected_backend
        || backend.report.matched != expected_backend
        || backend.report.empty != expected_empty
        || (cancellation.is_some() && backend.report.replies_sent != Exchange::SCRIPT.len())
        || !matches!(backend.result, Ok(udp::Terminal::Cancelled))
    {
        return Err(invalid("backend script/terminal differs"));
    }
    let relay = relay.ok_or_else(|| invalid("comparison worker not started"))??;
    match (kind, relay) {
        (ControllerKind::ScriptedResponder, udp::Completion::Scripted(relay)) => {
            if relay.report.arrivals != Exchange::SCRIPT.len()
                || relay.report.matched != Exchange::SCRIPT.len()
                || relay.report.empty != 1
                || !matches!(relay.result, Ok(udp::Terminal::ScriptComplete))
            {
                return Err(invalid("responder script/terminal differs"));
            }
        }
        (ControllerKind::BufferedRelay { .. }, udp::Completion::Forwarded(relay)) => {
            let expected = Exchange::SCRIPT.len();
            let empty = Exchange::SCRIPT
                .into_iter()
                .filter(|step| matches!(step, Exchange::Empty))
                .count();
            if relay.report.ingress != expected
                || relay.report.upstream_sent != expected
                || relay.report.upstream_received != expected
                || relay.report.client_sent != expected
                || relay.report.empty_ingress != empty
                || relay.report.empty_reply != empty
                || !matches!(relay.result, Ok(udp::Terminal::ScriptComplete))
            {
                return Err(invalid("forwarded legs/terminal differ"));
            }
        }
        (ControllerKind::CancelUpstreamReceive { .. }, udp::Completion::Forwarded(relay)) => {
            if !relay.cancelled_upstream_receive() {
                return Err(invalid("relay did not cancel at the upstream receive boundary"));
            }
        }
        _ => return Err(invalid("comparison completion type differs")),
    }
    if let Some(error) = detach_report_failure {
        return Err(error);
    }
    if let Some(failure) = detach_failure {
        return Err(failure.cause);
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_or_unknown_profile_is_typed_invalid_input() {
        for words in [
            vec!["sock-addr-udp4", "connected", "1000"],
            vec!["sock-addr-udp4", "connected", "1000", "unknown"],
        ] {
            assert_eq!(
                Inputs::parse(&words).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
    #[test]
    fn buffered_comparison_requires_explicit_envelope_and_preserves_client_roles() {
        for mode in ["connected", "unconnected"] {
            for profile in ["destination-only", "fixed-original-peer"] {
                let parsed = Inputs::parse(&["sock-addr-relay4", mode, "1000", profile, "18"]).unwrap();
                assert!(matches!(
                    parsed.0,
                    Role::Controller {
                        kind: ControllerKind::BufferedRelay { buffer_bytes: 18 },
                        ..
                    }
                ));
            }
        }
        for words in [
            vec!["sock-addr-relay4", "connected", "1000", "fixed-original-peer"],
            vec![
                "sock-addr-relay4",
                "connected",
                "1000",
                "fixed-original-peer",
                "-1",
            ],
            vec![
                "sock-addr-relay4",
                "connected",
                "1000",
                "fixed-original-peer",
                "bytes",
            ],
            vec!["sock-addr-relay4", "connected", "1000", "unknown", "18"],
        ] {
            assert_eq!(
                Inputs::parse(&words).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert_eq!(
            Inputs::parse(&["sock-addr-relay4", "connected", "0", "fixed-original-peer", "18",])
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        for (word, role) in [
            ("Backend", ClientRole::BaselineBackend),
            ("Relay", ClientRole::ScriptedResponder),
            ("ForwardedBackend", ClientRole::ForwardedBackend),
        ] {
            let parsed =
                Inputs::parse(&["sock-addr-client4", "connected", "1000", "127.0.0.1:41", word]).unwrap();
            assert!(matches!(parsed.0, Role::Client {role: actual, ..} if actual == role));
            assert_eq!(role.argument(), word);
        }
    }
    #[test]
    fn cancel_command_is_explicit_and_fixes_only_the_existing_presentation() {
        for mode in ["connected", "unconnected"] {
            let parsed = Inputs::parse(&["sock-addr-cancel4", mode, "1000", "18"]).unwrap();
            assert!(matches!(
                parsed.0,
                Role::Controller {
                    kind: ControllerKind::CancelUpstreamReceive { buffer_bytes: 18 },
                    presentation: Presentation::FixedOriginalPeer,
                    ..
                }
            ));
        }
        for words in [
            vec!["sock-addr-cancel4", "connected", "1000"],
            vec!["sock-addr-cancel4", "connected", "1000", "bad"],
            vec!["sock-addr-cancel4", "other", "1000", "18"],
            vec!["sock-addr-cancel4", "connected", "1000", "18", "extra"],
        ] {
            assert_eq!(
                Inputs::parse(&words).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
    #[test]
    fn finite_hook_plans_are_explicit_and_mode_specific() {
        use bpf::Hook::*;
        assert_eq!(Presentation::DestinationOnly.hooks(Mode::Connected), [Connect4]);
        assert_eq!(Presentation::DestinationOnly.hooks(Mode::Unconnected), [Sendmsg4]);
        assert_eq!(
            Presentation::FixedOriginalPeer.hooks(Mode::Connected),
            [Connect4, Recvmsg4, Getpeername4]
        );
        assert_eq!(
            Presentation::FixedOriginalPeer.hooks(Mode::Unconnected),
            [Sendmsg4, Recvmsg4]
        );
    }
    #[test]
    fn finite_parser_and_strict_original_port() {
        for mode in ["connected", "unconnected"] {
            for profile in ["destination-only", "fixed-original-peer"] {
                assert!(Inputs::parse(&["sock-addr-udp4", mode, "1000", profile]).is_ok());
            }
        }
        for words in [
            vec!["sock-addr-udp4", "other", "1000", "destination-only"],
            vec!["sock-addr-udp4", "connected", "1000"],
            vec!["sock-addr-udp4", "connected", "1000", "unknown"],
            vec!["sock-addr-udp4", "connected", "0", "destination-only"],
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
