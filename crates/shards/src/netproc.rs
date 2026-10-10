//! A VM's network process, started beside it (docs/design/architecture.md D31): the
//! frame ring and doorbells the two share, `shards-net` given its side of them, and the
//! VM's side ready for `shards-vm --net`.

use std::os::fd::{AsFd, OwnedFd};

/// What a VM or network process is given of its spawner's environment: the variables it
/// reads, and nothing else. A daemon's environment is that of the client that started
/// it, which may hold secrets (an API key, an agent's socket), and these are the
/// processes a guest could take over.
/// The resolvers a VM's network process asks names past the microVM of (D59): `SHARDS_DNS`,
/// each `ADDR[:PORT]`, comma-separated, as `dockerd --dns` names them; else the host's own
/// IPv4 nameservers (`/etc/resolv.conf`), at port 53. The host asks, so a loopback one (a
/// local cache's) serves as it is, which Docker must replace for a container.
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
    std::fs::read_to_string("/etc/resolv.conf")
        .unwrap_or_default()
        .lines()
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

/// [`child_env`], as `shards_ipc::spawn_in` takes it.
pub fn env_pairs<'e>(
    env: &'e [(&'static str, std::ffi::OsString)],
) -> Vec<(&'static str, &'e std::ffi::OsStr)> {
    env.iter().map(|(k, v)| (*k, v.as_os_str())).collect()
}

/// The descriptor numbers a VM process takes its side of the ring at.
pub const VM_FDS: [i32; 3] = [5, 6, 7];

/// The VM's side of its network: the ring and the two doorbells, for the VM process to
/// be given at [`VM_FDS`].
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
pub const VM_RELEASE_FD: i32 = 8;

impl VmSide {
    /// `--net`'s value for a VM given this side at [`VM_FDS`] with `mac`.
    pub fn arg(mac: &[u8; 6]) -> String {
        let [r, s, w] = VM_FDS;
        format!("{r},{s},{w},{}", shards_net::Mac(*mac))
    }
}

/// Starts the network process of one VM whose guest has `mac` on `bridge`, its binary
/// (helpers.rs), under `policy`.
/// Its side of the ring goes to it; the VM's comes back. Once the VM process holds its
/// side and the caller drops this one, the VM's end alone keeps the network process's
/// doorbell open, so the network process goes with the VM.
pub fn start(
    policy: shards_net::Policy,
    mac: &[u8; 6],
    bridge: &shards_net::bridge::Bridge,
) -> Result<(shards_ipc::Child, VmSide), String> {
    let at = |e: std::io::Error| format!("a VM's network: {e}");
    let region = shards_netring::memory().map_err(at)?;
    let (vm_sleeps, net_rings) = shards_netring::doorbell().map_err(at)?;
    let (net_sleeps, vm_rings) = shards_netring::doorbell().map_err(at)?;
    let binary = crate::helpers::net()?;
    let (policy, proxy) = match policy {
        shards_net::Policy::AllowAll => ("allow", None),
        shards_net::Policy::DenyAll => ("deny", None),
        // A build's proxy alone (D110): the gateway's port, and the proxy's socket.
        shards_net::Policy::Proxy { port, socket } => ("proxy", Some((port.to_string(), socket))),
        // A run's, given as it starts (NET_POLICY): a VM starts before its run is known.
        shards_net::Policy::Ports(_) => return Err("a VM's network starts allowing all or nothing".into()),
    };
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
        args.extend::<[&std::ffi::OsStr; 2]>(["--proxy-dialer".as_ref(), "8".as_ref()]);
        fds.push((theirs.as_fd(), 8));
    }
    let child = shards_ipc::spawn_in(&binary, &args, &fds, false, &env_pairs(&env))
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
