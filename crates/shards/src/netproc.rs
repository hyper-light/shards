//! A VM's network process, started beside it (docs/design/architecture.md D31): the
//! frame ring and doorbells the two share, `shards-net` given its side of them, and the
//! VM's side ready for `shards-vm --net`. On Windows (D142) the network process starts
//! confined, in an AppContainer that holds the network's capabilities, once its VM's
//! process exists, whose end it watches for.

#[cfg(unix)]
use std::os::fd::{AsFd, OwnedFd};

/// A socket handed to a network process (a published port's host socket, a peer's link):
/// a descriptor on Unix; on Windows a socket, which Winsock closes (closesocket).
#[cfg(unix)]
pub type Sock = OwnedFd;
#[cfg(windows)]
pub type Sock = std::os::windows::io::OwnedSocket;

/// A [`Sock`] lent to a message that hands it on.
#[cfg(unix)]
pub type Lent<'a> = std::os::fd::BorrowedFd<'a>;
#[cfg(windows)]
pub type Lent<'a> = std::os::windows::io::BorrowedSocket<'a>;

/// [`Sock`]'s loan.
pub fn lend(sock: &Sock) -> Lent<'_> {
    #[cfg(unix)]
    return sock.as_fd();
    #[cfg(windows)]
    return std::os::windows::io::AsSocket::as_socket(sock);
}

/// A VM's network process's control socket (D31), on which its run's published ports, its
/// peers and its policy go: a Unix socket on Unix.
#[cfg(unix)]
pub type Control = std::os::unix::net::UnixStream;

/// On Windows an AF_UNIX socket, which the network process waits on with its own sockets
/// (PM M213), with the process's ID, which a socket handed to it is shared with
/// (WSADuplicateSocketW, PM M203).
#[cfg(windows)]
#[derive(Debug)]
pub struct Control {
    pub socket: shards_vmm::platform::UnixStream,
    pub pid: u32,
}

#[cfg(windows)]
impl Control {
    pub fn try_clone(&self) -> std::io::Result<Control> {
        Ok(Control {
            socket: self.socket.try_clone()?,
            pid: self.pid,
        })
    }

    pub fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> std::io::Result<()> {
        self.socket.set_read_timeout(timeout)
    }

    pub fn set_write_timeout(&self, timeout: Option<std::time::Duration>) -> std::io::Result<()> {
        self.socket.set_write_timeout(timeout)
    }
}

#[cfg(windows)]
impl std::io::Read for &Control {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::Read::read(&mut &self.socket, buf)
    }
}

/// Sends `kind`, with `payload` and the sockets `socks`, to the network process on its
/// control `net`, as that process takes them (shards_net `control`): beside the message on
/// Unix; on Windows each shared with the process, before its payload, in a message of a
/// kind that hands sockets (`shards_ipc::kind::hands_sockets`).
pub fn send(net: &Control, kind: u8, payload: &[u8], socks: &[Lent<'_>]) -> std::io::Result<()> {
    #[cfg(unix)]
    return shards_ipc::send(net, kind, payload, socks);
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket as _;
        if !shards_ipc::kind::hands_sockets(kind) {
            if !socks.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "sockets with a message of a kind that hands none",
                ));
            }
            return shards_ipc::write_message(&net.socket, kind, payload);
        }
        let raw: Vec<u64> = socks.iter().map(|s| s.as_raw_socket()).collect();
        shards_ipc::write_message(
            &net.socket,
            kind,
            &shards_ipc::with_sockets(payload, &raw, net.pid)?,
        )
    }
}

/// The next message the network process sends on its control `net`; none once it has
/// hung up.
pub fn recv(net: &Control) -> std::io::Result<Option<shards_ipc::Message>> {
    #[cfg(unix)]
    return shards_ipc::recv(net);
    #[cfg(windows)]
    return shards_ipc::read_message(&net.socket);
}

/// What a VM or network process is given of its spawner's environment: the variables it
/// reads, and nothing else. A daemon's environment is that of the client that started
/// it, which may hold secrets (an API key, an agent's socket), and these are the
/// processes a guest could take over.
/// The resolvers a VM's network process asks names past the microVM of (D59): `SHARDS_DNS`,
/// each `ADDR[:PORT]`, comma-separated, as `dockerd --dns` names them; else the host's own
/// IPv4 nameservers (`/etc/resolv.conf`; on Windows each up adapter's), at port 53. The
/// host asks, so a loopback one (a local cache's) serves as it is, which Docker must
/// replace for a container.
pub fn resolvers() -> Vec<String> {
    let with_port = |s: &str| -> Option<String> {
        if s.parse::<std::net::SocketAddrV4>().is_ok() {
            Some(s.to_string())
        } else {
            s.parse::<std::net::Ipv4Addr>().ok().map(|a| format!("{a}:53"))
        }
    };
    if let Some(v) = std::env::var_os("SHARDS_DNS") {
        return v
            .to_string_lossy()
            .split(',')
            .map(str::trim)
            .filter_map(with_port)
            .collect();
    }
    #[cfg(unix)]
    let conf = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    #[cfg(windows)]
    let conf = String::from_utf8_lossy(&shards_net::bridge::host_resolv()).into_owned();
    conf.lines()
        .filter_map(|l| l.trim().strip_prefix("nameserver"))
        .filter_map(|a| with_port(a.trim()))
        .collect()
}

pub fn child_env() -> Vec<(&'static str, std::ffi::OsString)> {
    ["SHARDS_LOG", "SHARDS_TIMING"]
        .into_iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (name, value)))
        .collect()
}

/// [`child_env`], as `shards_ipc::spawn_in` (and on Windows `shards_ipc::Spawn`) takes it.
pub fn env_pairs<'e>(
    env: &'e [(&'static str, std::ffi::OsString)],
) -> Vec<(&'static str, &'e std::ffi::OsStr)> {
    env.iter().map(|(k, v)| (*k, v.as_os_str())).collect()
}

/// The descriptor numbers a VM process takes its side of the ring at.
#[cfg(unix)]
pub const VM_FDS: [i32; 3] = [5, 6, 7];

/// The VM's side of its network: the ring and the two doorbells, for the VM process to
/// be given at [`VM_FDS`].
#[cfg(unix)]
#[derive(Debug)]
pub struct VmSide {
    pub region: OwnedFd,
    /// What the VM sleeps on, which the network process rings, and the reverse.
    pub sleeps: OwnedFd,
    pub rings: OwnedFd,
    /// The spawner's socket to the network process, on which published ports go
    /// (`shards_ipc::kind::PUBLISH`); dropped by a spawner that publishes none.
    pub control: std::os::unix::net::UnixStream,
    /// The VM's, at [`VM_RELEASE_FD`], on which they close as its run ends
    /// (`shards_ipc::kind::UNPUBLISH`).
    pub release: std::os::unix::net::UnixStream,
}

/// Where a VM process is given its side's [`VmSide::release`], named by `--net-release`.
#[cfg(unix)]
pub const VM_RELEASE_FD: i32 = 8;

#[cfg(unix)]
impl VmSide {
    /// `--net`'s value for a VM given this side at [`VM_FDS`] with `mac`.
    pub fn arg(mac: &[u8; 6]) -> String {
        let [r, s, w] = VM_FDS;
        format!("{r},{s},{w},{}", shards_net::Mac(*mac))
    }
}

/// A build's proxy (D110), as a network process is told of it: the gateway's port, and
/// the proxy's socket.
type ProxyArgs = Option<(String, std::path::PathBuf)>;

/// What a VM's network process is told of its policy: the `--policy` it takes, and a
/// build's proxy's port and socket (D110).
fn policy_args(policy: shards_net::Policy) -> Result<(&'static str, ProxyArgs), String> {
    match policy {
        shards_net::Policy::AllowAll => Ok(("allow", None)),
        shards_net::Policy::DenyAll => Ok(("deny", None)),
        // A build's proxy alone (D110): the gateway's port, and the proxy's socket.
        shards_net::Policy::Proxy { port, socket } => Ok(("proxy", Some((port.to_string(), socket)))),
        // A run's, given as it starts (NET_POLICY): a VM starts before its run is known.
        shards_net::Policy::Ports(_) | shards_net::Policy::Host => {
            Err("a VM's network starts allowing all or nothing".into())
        }
    }
}

/// Starts the network process of one VM whose guest has `mac` on `bridge`, its binary
/// (helpers.rs), under `policy`, made by `group`'s spawner where given: under its VM's
/// parent (D183).
/// Its side of the ring goes to it; the VM's comes back. Once the VM process holds its
/// side and the caller drops this one, the VM's end alone keeps the network process's
/// doorbell open, so the network process goes with the VM.
#[cfg(unix)]
pub fn start(
    policy: shards_net::Policy,
    mac: &[u8; 6],
    bridge: &shards_net::bridge::Bridge,
    group: Option<shards_ipc::Group>,
) -> Result<(shards_ipc::Child, VmSide), String> {
    let at = |e: std::io::Error| format!("a VM's network: {e}");
    let region = shards_netring::memory().map_err(at)?;
    let (vm_sleeps, net_rings) = shards_netring::doorbell().map_err(at)?;
    let (net_sleeps, vm_rings) = shards_netring::doorbell().map_err(at)?;
    let binary = crate::helpers::net()?;
    let (policy, proxy) = policy_args(policy)?;
    let pair = || std::os::unix::net::UnixStream::pair().map_err(|e| format!("a VM's network control: {e}"));
    let (control, theirs) = pair()?;
    let (release, released) = pair()?;
    let err = std::io::stderr();
    let mac = shards_net::Mac(*mac).to_string();
    let bridge = bridge.to_string();
    let env = child_env();
    let resolvers = resolvers();
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "--ring".as_ref(),
        "3,4,5".as_ref(),
        "--policy".as_ref(),
        policy.as_ref(),
        "--mac".as_ref(),
        mac.as_ref(),
        "--bridge".as_ref(),
        bridge.as_ref(),
        "--control".as_ref(),
        "6".as_ref(),
        "--release".as_ref(),
        "7".as_ref(),
    ];
    for r in &resolvers {
        args.push("--resolver".as_ref());
        args.push(r.as_ref());
    }
    if let Some((port, socket)) = &proxy {
        args.extend::<[&std::ffi::OsStr; 4]>([
            "--proxy-port".as_ref(),
            port.as_ref(),
            "--proxy-socket".as_ref(),
            socket.as_os_str(),
        ]);
    }
    // The dialer's end joins them on macOS alone.
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut fds = vec![
        (err.as_fd(), 2),
        (region.as_fd(), 3),
        (net_sleeps.as_fd(), 4),
        (net_rings.as_fd(), 5),
        (theirs.as_fd(), 6),
        (released.as_fd(), 7),
    ];
    // In App Sandbox, the network process reaches no socket by its path (D31): this
    // process dials the build's proxy for it, over a link of the two's own.
    #[cfg(target_os = "macos")]
    let dialer = match &proxy {
        Some((_, socket)) => {
            let (ours, theirs) = pair()?;
            let socket = socket.clone();
            std::thread::Builder::new()
                .name("proxy dialer".into())
                .spawn(move || serve_dials(&ours, &socket))
                .map_err(|e| format!("the build proxy's dialer: {e}"))?;
            Some(theirs)
        }
        None => None,
    };
    #[cfg(target_os = "macos")]
    if let Some(theirs) = &dialer {
        args.extend::<[&std::ffi::OsStr; 2]>(["--dialer".as_ref(), "8".as_ref()]);
        fds.push((theirs.as_fd(), 8));
    }
    let child = shards_ipc::spawn_in_group(group, &binary, &args, &fds, false, &env_pairs(&env))
        .map_err(|e| format!("starting {}: {e}", binary.display()))?;
    Ok((
        child,
        VmSide {
            region,
            sleeps: vm_sleeps,
            rings: vm_rings,
            control,
            release,
        },
    ))
}

/// Dials the build's proxy at `socket` for a network process in App Sandbox (D31), each
/// time it asks over `link` (`kind::DIAL`): the connection given as a descriptor, kept
/// until the network process says it has it (`kind::TAKEN`), as XNU drops a socket in flight
/// that no process holds (M24); or `kind::ERR` with why not. Ends as the network process
/// goes.
#[cfg(target_os = "macos")]
fn serve_dials(link: &std::os::unix::net::UnixStream, socket: &std::path::Path) {
    use shards_ipc::kind;
    while let Ok(Some(asked)) = shards_ipc::recv(link) {
        if asked.kind != kind::DIAL {
            return;
        }
        match std::os::unix::net::UnixStream::connect(socket) {
            Ok(conn) => {
                if shards_ipc::send(link, kind::GRANTED, &[], &[conn.as_fd()]).is_err() {
                    return;
                }
                match shards_ipc::recv(link) {
                    Ok(Some(m)) if m.kind == kind::TAKEN => {}
                    _ => return,
                }
            }
            Err(e) => {
                if shards_ipc::send(link, kind::ERR, e.to_string().as_bytes(), &[]).is_err() {
                    return;
                }
            }
        }
    }
}

/// How long a network process whose VM is gone has to go before it is ended: it goes as
/// its doorbell hangs up.
pub const GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// Waits for a network process whose VM is gone, [`GRACE`] at most, then ends it: woken by
/// its end (`Poller::add_exit`), where one cannot be watched by a look every millisecond.
#[cfg(unix)]
pub fn reap(net: &shards_ipc::Child) {
    let deadline = std::time::Instant::now() + GRACE;
    let watched = shards_vmm::platform::Poller::new()
        .and_then(|poller| poller.add_exit(net.id(), 0).map(|watch| (poller, watch)));
    match watched {
        Ok((poller, _watch)) => {
            let mut ready = Vec::new();
            // A signal's interruption is an empty wait: until its end, or the deadline.
            while ready.is_empty() {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() || poller.wait(&mut ready, Some(left)).is_err() {
                    break;
                }
            }
        }
        // Ended already (macOS refuses to watch it then).
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
        Err(_) => {
            while net.try_wait().is_none() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }
    if net.try_wait().is_none() {
        let _ = net.kill(libc::SIGKILL);
        let _ = net.wait();
    }
}

/// The VM's side of its network (Windows, D142): the ring and its two doorbells, handed
/// to the VM process by name ([`VmSide::given`], [`VmSide::shared`]): the event it sleeps
/// on, and the AF_UNIX socket it rings its network process by, which that process waits on
/// with its sockets (PM M213). And as on Unix, the spawner's control, on which published
/// ports go, and the VM's release, on which they close as its run ends
/// ([`VmSide::release`]): AF_UNIX sockets that process waits on too. A builder's spawner
/// publishes nothing, and drops both.
#[cfg(windows)]
#[derive(Debug)]
pub struct VmSide {
    pub region: std::os::windows::io::OwnedHandle,
    pub sleeps: std::os::windows::io::OwnedHandle,
    pub rings: shards_vmm::platform::UnixStream,
    pub control: shards_vmm::platform::UnixStream,
    pub release: shards_vmm::platform::UnixStream,
}

#[cfg(windows)]
impl VmSide {
    /// `--net`'s value for a VM handed [`VmSide::given`] with `mac`.
    pub fn arg(mac: &[u8; 6]) -> String {
        shards_net::Mac(*mac).to_string()
    }

    /// The ring and the doorbell it sleeps on, by the names a VM process takes them by.
    pub fn given(&self) -> [shards_ipc::Given<'_>; 2] {
        use std::os::windows::io::AsHandle as _;
        [
            shards_ipc::Given {
                name: "net-region",
                handle: self.region.as_handle(),
            },
            shards_ipc::Given {
                name: "net-wake-me",
                handle: self.sleeps.as_handle(),
            },
        ]
    }

    /// The doorbell it rings, a socket, by the name a VM process takes it by.
    pub fn shared(&self) -> shards_ipc::Shared<'_> {
        shards_ipc::Shared {
            name: "net-wake-peer",
            socket: self.rings.raw() as u64,
        }
    }

    /// The socket its run's published ports close on as the run ends, by the name a VM
    /// process takes it by (`--net`, vm_run.rs).
    pub fn release(&self) -> shards_ipc::Shared<'_> {
        shards_ipc::Shared {
            name: "net-release",
            socket: self.release.raw() as u64,
        }
    }
}

/// A VM's network process, made ready to start once its VM's process exists, whose end it
/// watches for (Windows, D142): a doorbell's peer cannot hang up there.
#[cfg(windows)]
#[derive(Debug)]
pub struct Ready {
    region: std::os::windows::io::OwnedHandle,
    sleeps: shards_vmm::platform::UnixStream,
    rings: std::os::windows::io::OwnedHandle,
    /// Its ends of the spawner's control and of the VM's release ([`VmSide`]).
    control: shards_vmm::platform::UnixStream,
    release: shards_vmm::platform::UnixStream,
    args: Vec<std::ffi::OsString>,
    /// A build's proxy's socket, which its spawner dials for it (D110).
    proxy: Option<std::path::PathBuf>,
}

/// The ring and doorbells of one VM's network, whose guest has `mac` on `bridge`, under
/// `policy`: the network process's side, ready to start ([`start`]), and the VM's.
#[cfg(windows)]
pub fn prepare(
    policy: shards_net::Policy,
    mac: &[u8; 6],
    bridge: &shards_net::bridge::Bridge,
) -> Result<(Ready, VmSide), String> {
    let at = |e: std::io::Error| format!("a VM's network: {e}");
    let region = shards_netring::memory().map_err(at)?;
    // The VM sleeps on an event its network process rings; the network process on a
    // socket its port names with its sockets (PM M213), which its VM rings.
    let (shards_netring::Bell::Event(vm_sleeps), shards_netring::Bell::Event(net_rings)) =
        shards_netring::doorbell().map_err(at)?
    else {
        return Err("a VM's network: a doorbell of events".into());
    };
    let (shards_netring::Bell::Socket(net_sleeps), shards_netring::Bell::Socket(vm_rings)) =
        shards_netring::socket_doorbell().map_err(at)?
    else {
        return Err("a VM's network: a doorbell of sockets".into());
    };
    let (policy, proxy) = policy_args(policy)?;
    let pair =
        || shards_vmm::platform::UnixStream::pair().map_err(|e| format!("a VM's network control: {e}"));
    let (control, net_control) = pair()?;
    let (release, net_release) = pair()?;
    let mut args: Vec<std::ffi::OsString> = vec![
        "--policy".into(),
        policy.into(),
        "--mac".into(),
        shards_net::Mac(*mac).to_string().into(),
        "--bridge".into(),
        bridge.to_string().into(),
        "--control".into(),
        "net-control".into(),
        "--release".into(),
        "net-release".into(),
    ];
    for r in resolvers() {
        args.extend(["--resolver".into(), r.into()]);
    }
    let proxy = match proxy {
        Some((port, socket)) => {
            args.extend([
                "--proxy-port".into(),
                port.into(),
                "--proxy-socket".into(),
                socket.clone().into(),
            ]);
            Some(socket)
        }
        None => None,
    };
    // Its spawner's dialer, of what its AppContainer keeps it from: a build's proxy's
    // socket, and the host's own addresses and its loopback (PM M204), which it is told.
    args.extend(["--dialer".into(), "dialer".into()]);
    for address in shards_net::bridge::host_addresses() {
        args.extend(["--host-address".into(), address.to_string().into()]);
    }
    Ok((
        Ready {
            region: region.try_clone().map_err(at)?,
            sleeps: net_sleeps,
            rings: net_rings,
            control: net_control,
            release: net_release,
            args,
            proxy,
        },
        VmSide {
            region,
            sleeps: vm_sleeps,
            rings: vm_rings,
            control,
            release,
        },
    ))
}

/// A started network process: on Windows holding the AppContainer it runs in, whose profile
/// goes once it has (`shards_ipc::Child::hold`).
pub type NetProcess = shards_ipc::Child;

/// Starts `ready`, the network process of the VM `vm`, made and not yet running: given a
/// handle to it that may only wait for its end, in an AppContainer holding the network's
/// capabilities (PM M204).
#[cfg(windows)]
pub fn start(ready: Ready, vm: &shards_ipc::Suspended) -> Result<NetProcess, String> {
    use std::os::windows::io::AsHandle as _;
    let binary = crate::helpers::net()?;
    let container = crate::helpers::containers().and_then(|records| {
        shards_ipc::AppContainer::new("net", &records)
            .map_err(|e| format!("a VM network's AppContainer: {e}"))
    })?;
    let vm_end = vm
        .waitable()
        .map_err(|e| format!("a VM's network: its VM's process: {e}"))?;
    let (ours, theirs) =
        shards_vmm::platform::UnixStream::pair().map_err(|e| format!("a network process's dialer: {e}"))?;
    let sockets = [
        shards_ipc::Shared {
            name: "dialer",
            socket: theirs.raw() as u64,
        },
        shards_ipc::Shared {
            name: "net-wake-me",
            socket: ready.sleeps.raw() as u64,
        },
        shards_ipc::Shared {
            name: "net-control",
            socket: ready.control.raw() as u64,
        },
        shards_ipc::Shared {
            name: "net-release",
            socket: ready.release.raw() as u64,
        },
    ];
    let args: Vec<&std::ffi::OsStr> = ready.args.iter().map(std::ffi::OsString::as_os_str).collect();
    let err = std::io::stderr();
    let env = child_env();
    let mut child = shards_ipc::spawn_confined(&shards_ipc::Spawn {
        program: &binary,
        args: &args,
        grants_at: 0,
        given: &[
            shards_ipc::Given {
                name: "net-region",
                handle: ready.region.as_handle(),
            },
            shards_ipc::Given {
                name: "net-wake-peer",
                handle: ready.rings.as_handle(),
            },
            shards_ipc::Given {
                name: "vm",
                handle: vm_end.as_handle(),
            },
        ],
        stdin: None,
        stdout: err.as_handle(),
        stderr: err.as_handle(),
        sockets: &sockets,
        env: &env_pairs(&env),
        container: Some(&container),
        holds: shards_ipc::Holds::Network,
        group: None,
    })
    .map_err(|e| format!("starting {}: {e}", binary.display()))?;
    drop(theirs);
    // An AppContainer reaches no socket by its path, nor the host's own addresses: this
    // process dials them for it, over a link of the two's own, as on macOS (D31).
    let pid = child.id();
    let proxy = ready.proxy;
    std::thread::Builder::new()
        .name("network dialer".into())
        .spawn(move || serve_dials(&ours, proxy.as_deref(), pid))
        .map_err(|e| format!("a network process's dialer: {e}"))?;
    child.hold(container);
    Ok(child)
}

/// Dials for the network process `pid`, in an AppContainer, as it asks over `link`
/// (`kind::DIAL`, each ask numbered): with nothing named, the build's proxy at `proxy`; with
/// an address and a port, a TCP connection to the host's own address or its loopback (PM
/// M204), refused to any other address. Each is answered once connected, as Windows
/// documents a shared socket's use, one process making the connection and another
/// exchanging data on it (WSADuplicateSocketW, PM M203): shared with it, and kept until it
/// says it has its own (`kind::TAKEN`); or `kind::ERR` says why not. The connects under way
/// are each waited on beside the link, so that none holds up another's answer. Ends as the
/// network process goes.
#[cfg(windows)]
fn serve_dials(link: &shards_vmm::platform::UnixStream, proxy: Option<&std::path::Path>, pid: u32) {
    use shards_ipc::kind;
    use std::os::windows::io::AsRawSocket as _;
    use windows_sys::Win32::Networking::WinSock::{
        POLLERR, POLLHUP, POLLNVAL, POLLRDNORM, POLLWRNORM, WSAPOLLFD, WSAPoll,
    };
    // A connection is held, and closed as it drops, until the process has its own.
    type Held = Box<dyn std::any::Any>;
    let answer = |id: u32, kind: u8, rest: &[u8]| {
        let mut payload = id.to_be_bytes().to_vec();
        payload.extend(rest);
        shards_ipc::write_message(link, kind, &payload)
    };
    let mut connecting: Vec<(u32, std::net::TcpStream)> = Vec::new();
    let mut held: Vec<(u32, Held)> = Vec::new();
    loop {
        let mut fds: Vec<WSAPOLLFD> = std::iter::once(link.raw())
            .map(|fd| (fd, POLLRDNORM))
            .chain(
                connecting
                    .iter()
                    .map(|(_, s)| (s.as_raw_socket() as usize, POLLWRNORM)),
            )
            .map(|(fd, events)| WSAPOLLFD {
                fd,
                events,
                revents: 0,
            })
            .collect();
        // SAFETY: an array of its own length, waited on until one is ready.
        if unsafe { WSAPoll(fds.as_mut_ptr(), fds.len() as u32, -1) } < 0 {
            return;
        }
        // Each connect that ended, answered: connected, or why not. A peer that took the
        // connection and closed it at once leaves it ended too, its end the network
        // process's to see.
        let ended: Vec<(usize, i16)> = fds
            .iter()
            .skip(1)
            .map(|fd| fd.revents)
            .enumerate()
            .filter(|&(_, r)| r != 0)
            .collect();
        for (i, r) in ended.into_iter().rev() {
            if i >= connecting.len() {
                continue;
            }
            let (id, sock) = connecting.swap_remove(i);
            let shared = match sock.take_error() {
                Ok(Some(e)) | Err(e) => Err(e),
                Ok(None) if sock.peer_addr().is_ok() => shards_ipc::share_socket(sock.as_raw_socket(), pid),
                Ok(None) if r & (POLLERR | POLLHUP | POLLNVAL) != 0 => {
                    Err(std::io::Error::other("the connection failed"))
                }
                Ok(None) => {
                    connecting.push((id, sock));
                    continue;
                }
            };
            let sent = match shared {
                Ok(info) => answer(id, kind::GRANTED, &info).map(|()| held.push((id, Box::new(sock)))),
                Err(e) => answer(id, kind::ERR, e.to_string().as_bytes()),
            };
            if sent.is_err() {
                return;
            }
        }
        if fds.first().map_or(0, |fd| fd.revents) == 0 {
            continue;
        }
        let Ok(Some(asked)) = shards_ipc::read_message(link) else {
            return;
        };
        let Some((id, rest)) = asked.payload.split_first_chunk::<4>() else {
            return;
        };
        let id = u32::from_be_bytes(*id);
        match asked.kind {
            kind::TAKEN => held.retain(|(i, _)| *i != id),
            kind::DIAL if rest.is_empty() => {
                let dialed = proxy
                    .ok_or_else(|| std::io::Error::other("no build proxy to dial"))
                    .and_then(shards_vmm::platform::UnixStream::connect)
                    .and_then(|conn| conn.share_with(pid).map(|info| (conn, info)));
                let sent = match dialed {
                    Ok((conn, info)) => {
                        answer(id, kind::GRANTED, &info).map(|()| held.push((id, Box::new(conn))))
                    }
                    Err(e) => answer(id, kind::ERR, e.to_string().as_bytes()),
                };
                if sent.is_err() {
                    return;
                }
            }
            kind::DIAL => match host_tcp(rest) {
                Ok(sock) => connecting.push((id, sock)),
                Err(e) => {
                    if answer(id, kind::ERR, e.to_string().as_bytes()).is_err() {
                        return;
                    }
                }
            },
            // A UDP socket connects at once: answered as it is made.
            kind::DIAL_UDP => {
                let made = host_udp(rest).and_then(|sock| {
                    shards_ipc::share_socket(sock.as_raw_socket(), pid).map(|info| (sock, info))
                });
                let sent = match made {
                    Ok((sock, info)) => {
                        answer(id, kind::GRANTED, &info).map(|()| held.push((id, Box::new(sock))))
                    }
                    Err(e) => answer(id, kind::ERR, e.to_string().as_bytes()),
                };
                if sent.is_err() {
                    return;
                }
            }
            _ => return,
        }
    }
}

/// A TCP connection begun, without waiting, to the address (4 or 16 bytes) and port
/// (big-endian) `asked` names, from that address: only one the host holds as it binds, its
/// own or its loopback, which a network process's AppContainer keeps it from (D142); its own
/// capabilities reach the rest.
#[cfg(windows)]
fn host_tcp(asked: &[u8]) -> std::io::Result<std::net::TcpStream> {
    shards_net::sys::connect_tcp_to_itself(dialed(asked)?).map_err(not_the_hosts)
}

/// A UDP socket bound at the address (4 or 16 bytes) `asked` names and connected there, to
/// its port (big-endian), as [`host_tcp`] connects: only an address the host holds as it
/// binds, its own or its loopback, which a network process's AppContainer keeps it from
/// (D142).
#[cfg(windows)]
fn host_udp(asked: &[u8]) -> std::io::Result<std::net::UdpSocket> {
    let to = dialed(asked)?;
    let sock = std::net::UdpSocket::bind(std::net::SocketAddr::new(to.ip(), 0)).map_err(not_the_hosts)?;
    sock.connect(to)?;
    Ok(sock)
}

/// Where a dial `asked` goes: its address (4 or 16 bytes), then its port (big-endian).
#[cfg(windows)]
fn dialed(asked: &[u8]) -> std::io::Result<std::net::SocketAddr> {
    let refused = |why: &str| std::io::Error::new(std::io::ErrorKind::PermissionDenied, why.to_string());
    let (address, port) = asked
        .split_at_checked(asked.len().saturating_sub(2))
        .ok_or_else(|| refused("a dial of nothing"))?;
    let port = u16::from_be_bytes(port.try_into().map_err(|_| refused("a dial without a port"))?);
    let ip: std::net::IpAddr = match address.len() {
        4 => <[u8; 4]>::try_from(address)
            .map_err(|_| refused("an address"))?
            .into(),
        16 => <[u8; 16]>::try_from(address)
            .map_err(|_| refused("an address"))?
            .into(),
        _ => return Err(refused("a dial of no address")),
    };
    Ok(std::net::SocketAddr::new(ip, port))
}

/// A bind's refusal of an address the host does not hold, said so.
#[cfg(windows)]
fn not_the_hosts(e: std::io::Error) -> std::io::Error {
    if e.kind() == std::io::ErrorKind::AddrNotAvailable {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "not one of this host's addresses",
        )
    } else {
        e
    }
}

/// Waits for a network process whose VM is gone, [`GRACE`] at most, then ends it (Windows):
/// it goes as it sees its VM's process end.
#[cfg(windows)]
pub fn reap(net: &NetProcess) {
    let grace = u32::try_from(GRACE.as_millis()).unwrap_or(u32::MAX);
    if !net.ended_within(grace) {
        let _ = net.kill(shards_ipc::signal::SIGKILL);
        let _ = net.wait();
    }
}
