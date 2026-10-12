//! shards-net: one microVM's network process (docs/design/architecture.md D31). Its
//! spawner hands it the VM's frame ring and doorbells; it serves the guest's flows until
//! the VM goes, which its doorbell's hang-up says (Windows: its VM's process's end).
//!
//!     shards-net --ring REGION,WAKE_ME,WAKE_PEER --policy allow|deny|proxy --mac MAC
//!         --bridge SUBNET/BITS [--control FD] [--release FD]
//!         [--proxy-port PORT --proxy-socket PATH]
//!
//! On Windows (D142) `--grants HANDLE` takes `--ring`'s place, and `--control`,
//! `--release` and `--proxy-dialer` name sockets handed by name.

use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "shards-net: {e}");
            ExitCode::FAILURE
        }
    }
}

/// What the command line says, on every host; where its ring, controls and dialer are,
/// each host's own way.
struct Args {
    /// The daemon's socket, on which published ports come, and the VM's, on which they go:
    /// a descriptor on Unix, a name the spawner handed it by on Windows.
    controls: Vec<(shards_net::Control, String)>,
    ring: Option<String>,
    mac: Option<[u8; 6]>,
    /// No default: a spawner that forgot to say gets an error, not open access.
    policy: Option<Option<shards_net::Policy>>,
    /// A build's proxy (D110): the gateway's port its one flow is to, and the socket that
    /// flow goes to.
    proxy_port: Option<u16>,
    proxy_socket: Option<std::path::PathBuf>,
    proxy_dialer: Option<String>,
    /// Nor a subnet of its own: the guest's is its spawner's to elect.
    bridge: Option<shards_net::bridge::Bridge>,
    /// The host's resolvers, which a guest granted egress asks its names of (D59).
    resolvers: Vec<(std::net::Ipv4Addr, u16)>,
    /// What its spawner handed it (Windows: `--grants HANDLE`, shards_ipc's pipe).
    grants: Option<String>,
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        controls: Vec::new(),
        ring: None,
        mac: None,
        policy: None,
        proxy_port: None,
        proxy_socket: None,
        proxy_dialer: None,
        bridge: None,
        resolvers: Vec::new(),
        grants: None,
    };
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        let value = |args: &mut dyn Iterator<Item = std::ffi::OsString>, name: &str| {
            args.next()
                .and_then(|v| v.into_string().ok())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.to_str() {
            Some("--grants") => a.grants = Some(value(&mut args, "--grants")?),
            Some("--ring") => a.ring = Some(value(&mut args, "--ring")?),
            // The daemon's, which publishes ports, and the VM's, which lets them go.
            Some("--control") => a
                .controls
                .push((shards_net::Control::Daemon, value(&mut args, "--control")?)),
            Some("--release") => a
                .controls
                .push((shards_net::Control::Release, value(&mut args, "--release")?)),
            Some("--mac") => {
                let v = value(&mut args, "--mac")?;
                a.mac = Some(v.parse::<shards_net::Mac>().map_err(|e| format!("--mac {e}"))?.0);
            }
            Some("--bridge") => {
                a.bridge = Some(
                    value(&mut args, "--bridge")?
                        .parse()
                        .map_err(|e| format!("--bridge: {e}"))?,
                )
            }
            Some("--resolver") => {
                let v = value(&mut args, "--resolver")?;
                let at = v
                    .parse::<std::net::SocketAddrV4>()
                    .map_err(|e| format!("--resolver {v:?}: {e}"))?;
                a.resolvers.push((*at.ip(), at.port()));
            }
            Some("--policy") => {
                a.policy = Some(match value(&mut args, "--policy")?.as_str() {
                    "allow" => Some(shards_net::Policy::AllowAll),
                    "deny" => Some(shards_net::Policy::DenyAll),
                    // Its port and socket, given apart.
                    "proxy" => None,
                    other => return Err(format!("--policy {other:?}: allow, deny or proxy")),
                })
            }
            Some("--proxy-port") => {
                let v = value(&mut args, "--proxy-port")?;
                a.proxy_port = Some(v.parse::<u16>().map_err(|e| format!("--proxy-port {v:?}: {e}"))?);
            }
            Some("--proxy-socket") => {
                a.proxy_socket = Some(std::path::PathBuf::from(
                    args.next().ok_or("--proxy-socket needs a value")?,
                ));
            }
            // Where its confinement keeps it from the proxy's socket (App Sandbox, D31; an
            // AppContainer, D142): its spawner's dialer, which connects for it.
            Some("--proxy-dialer") => a.proxy_dialer = Some(value(&mut args, "--proxy-dialer")?),
            _ => return Err(format!("unknown argument {arg:?}")),
        }
    }
    Ok(a)
}

impl Args {
    /// The policy it was given, whole.
    fn policy(&mut self) -> Result<shards_net::Policy, String> {
        match (
            self.policy.take().ok_or("--policy is required")?,
            self.proxy_port,
            self.proxy_socket.take(),
        ) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                Err("--proxy-port and --proxy-socket go with --policy proxy alone".into())
            }
            (Some(policy), None, None) => Ok(policy),
            (None, Some(port), Some(socket)) => Ok(shards_net::Policy::Proxy { port, socket }),
            (None, _, _) => Err("--policy proxy needs --proxy-port and --proxy-socket".into()),
        }
    }

    /// The configuration of the guest's network: its policy, its bridge, its MAC (frames
    /// from any other are not its), and the host's resolvers.
    fn config(&mut self) -> Result<shards_net::Config, String> {
        let policy = self.policy()?;
        let bridge = self.bridge.take().ok_or("--bridge is required")?;
        let mac = self.mac.ok_or("--mac is required")?;
        let mut cfg = shards_net::Config::on_bridge(policy, mac, &bridge);
        cfg.resolvers = std::mem::take(&mut self.resolvers);
        Ok(cfg)
    }
}

#[cfg(unix)]
fn run() -> Result<(), String> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // In App Sandbox from its launch, as its signature says (resources/net.entitlements,
    // D31): one outside it serves no VM, and is told so before it reads anything.
    #[cfg(target_os = "macos")]
    if !shards_apple::sandbox::sandboxed()? {
        return Err("not in App Sandbox: a network process is signed into it \
                    (resources/net.entitlements, crates/shards/build.rs), and serves no VM outside it"
            .into());
    }
    let mut a = parse()?;
    let ring = a.ring.take().ok_or("--ring is required")?;
    let proxy = matches!(a.policy, Some(None));
    let cfg = a.config()?;
    let fds: Vec<i32> = ring
        .split(',')
        .map(|v| {
            v.parse()
                .map_err(|_| format!("--ring: {v:?} is not a descriptor"))
        })
        .collect::<Result<_, String>>()?;
    let [region, me, peer] = fds.as_slice() else {
        return Err("--ring takes REGION,WAKE_ME,WAKE_PEER".into());
    };
    let mut seen = Vec::new();
    let mut adopt = |fd: i32| -> Result<OwnedFd, String> {
        // SAFETY: fcntl(2) asks whether the descriptor is open.
        if fd < 3 || seen.contains(&fd) || unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(format!("--ring: {fd} is not a descriptor of its own"));
        }
        seen.push(fd);
        // SAFETY: an open descriptor the spawner left for this process alone.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        // SAFETY: as above, owned from here on.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    };
    let (region, me, peer) = (adopt(*region)?, adopt(*me)?, adopt(*peer)?);
    if let Some(fd) = a.proxy_dialer {
        let fd = fd
            .parse()
            .map_err(|_| format!("--proxy-dialer: {fd:?} is not a descriptor"))?;
        shards_net::tcp::set_proxy_dialer(std::os::unix::net::UnixStream::from(adopt(fd)?))
            .map_err(|e| e.to_string())?;
    }
    let controls = a
        .controls
        .iter()
        .map(|(role, fd)| {
            let fd = fd
                .parse()
                .map_err(|_| format!("{role:?} control: {fd:?} is not a descriptor"))?;
            Ok((*role, std::os::unix::net::UnixStream::from(adopt(fd)?)))
        })
        .collect::<Result<_, String>>()?;
    // Confined before it reads anything a guest sends (D31): no file, no TCP port of its
    // own, no TCP at all under a build's proxy.
    #[cfg(target_os = "linux")]
    shards_net::confine::at_start(proxy).map_err(|e| e.to_string())?;
    #[cfg(not(target_os = "linux"))]
    let _ = proxy;
    shards_net::serve(region, me, peer, cfg, controls).map_err(|e| e.to_string())
}

/// On Windows (D142): in an AppContainer of its own from its start, holding the network's
/// capabilities (PM M204), or it serves no VM. What its spawner handed it comes by name:
/// the frame ring (`net-region`) and doorbells (`net-wake-me`, `net-wake-peer`), its VM's
/// process (`vm`), whose end is its own, the controls and the proxy's dialer, each a socket
/// shared with it, named by its flag.
#[cfg(windows)]
fn run() -> Result<(), String> {
    use std::os::windows::io::{FromRawHandle as _, OwnedHandle, RawHandle};
    if !shards_ipc::in_app_container() {
        return Err(
            "not in an AppContainer: a network process is started confined (D142), \
                    and serves no VM outside it"
                .into(),
        );
    }
    let mut a = parse()?;
    let grants = a.grants.take().ok_or("--grants is required")?;
    let cfg = a.config()?;
    let pipe = grants
        .parse::<usize>()
        .map_err(|_| format!("--grants: {grants:?} is not a handle"))?;
    let mut given = shards_ipc::given(pipe).map_err(|e| format!("--grants: {e}"))?;
    let mut take = |name: &str| {
        let at = given
            .iter()
            .position(|(n, _)| n == name)
            .ok_or_else(|| format!("--grants: no {name} was given"))?;
        Ok::<_, String>(given.swap_remove(at).1)
    };
    let mut handle = |name: &str| -> Result<OwnedHandle, String> {
        match take(name)? {
            shards_ipc::Handed::Handle(v) if v != 0 => {
                // SAFETY: a handle the spawner duplicated into this process for this alone,
                // owned from here on.
                Ok(unsafe { OwnedHandle::from_raw_handle(v as RawHandle) })
            }
            _ => Err(format!("--grants: {name} is no handle")),
        }
    };
    let (region, me, peer, vm) = (
        handle("net-region")?,
        handle("net-wake-me")?,
        handle("net-wake-peer")?,
        handle("vm")?,
    );
    let mut socket = |name: &str| -> Result<shards_net::sys::UnixStream, String> {
        match take(name)? {
            shards_ipc::Handed::Socket(info) => {
                shards_net::sys::UnixStream::from_shared(&info).map_err(|e| format!("{name}: {e}"))
            }
            shards_ipc::Handed::Handle(_) => Err(format!("--grants: {name} is no socket")),
        }
    };
    if let Some(name) = a.proxy_dialer.take() {
        shards_net::tcp::set_proxy_dialer(socket(&name)?).map_err(|e| e.to_string())?;
    }
    let controls = a
        .controls
        .iter()
        .map(|(role, name)| Ok((*role, socket(name)?)))
        .collect::<Result<_, String>>()?;
    shards_net::serve(region, me, peer, vm, cfg, controls).map_err(|e| e.to_string())
}
