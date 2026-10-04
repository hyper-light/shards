//! shards-net: one microVM's network process (docs/design/architecture.md D31). Its
//! spawner hands it the VM's frame ring and doorbells; it serves the guest's flows until
//! the VM goes, which its doorbell's hang-up says.
//!
//!     shards-net --ring REGION,WAKE_ME,WAKE_PEER --policy allow|deny --mac MAC
//!         --bridge SUBNET/BITS [--control FD]

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

#[cfg(unix)]
fn run() -> Result<(), String> {
    use std::os::fd::{FromRawFd, OwnedFd};
    // The daemon's socket, on which published ports come, and the VM's, on which they go.
    let mut controls: Vec<String> = Vec::new();
    let mut ring = None;
    let mut mac = None;
    // No default: a spawner that forgot to say gets an error, not open access.
    let mut policy = None;
    // Nor a subnet of its own: the guest's is its spawner's to elect.
    let mut bridge: Option<shards_net::bridge::Bridge> = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        let value = |args: &mut dyn Iterator<Item = std::ffi::OsString>, name: &str| {
            args.next()
                .and_then(|v| v.into_string().ok())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match a.to_str() {
            Some("--ring") => ring = Some(value(&mut args, "--ring")?),
            Some("--control") => controls.push(value(&mut args, "--control")?),
            Some("--mac") => {
                let v = value(&mut args, "--mac")?;
                let octets: Vec<u8> = v
                    .split(':')
                    .map(|h| u8::from_str_radix(h, 16))
                    .collect::<Result<_, _>>()
                    .map_err(|_| format!("--mac {v:?} is not a MAC"))?;
                mac = Some(<[u8; 6]>::try_from(octets).map_err(|_| format!("--mac {v:?} is not a MAC"))?);
            }
            Some("--bridge") => {
                bridge = Some(
                    value(&mut args, "--bridge")?
                        .parse()
                        .map_err(|e| format!("--bridge: {e}"))?,
                )
            }
            Some("--policy") => {
                policy = Some(match value(&mut args, "--policy")?.as_str() {
                    "allow" => shards_net::Policy::AllowAll,
                    "deny" => shards_net::Policy::DenyAll,
                    other => return Err(format!("--policy {other:?}: allow or deny")),
                })
            }
            _ => return Err(format!("unknown argument {a:?}")),
        }
    }
    let ring = ring.ok_or("--ring is required")?;
    let policy = policy.ok_or("--policy is required")?;
    let bridge = bridge.ok_or("--bridge is required")?;
    // The guest's MAC, which the VM's device has: frames from any other are not its.
    let mac = mac.ok_or("--mac is required")?;
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
    let controls = controls
        .iter()
        .map(|fd| {
            let fd = fd
                .parse()
                .map_err(|_| format!("--control: {fd:?} is not a descriptor"))?;
            Ok(std::os::unix::net::UnixStream::from(adopt(fd)?))
        })
        .collect::<Result<_, String>>()?;
    shards_net::serve(
        region,
        me,
        peer,
        shards_net::Config::on_bridge(policy, mac, &bridge),
        controls,
    )
    .map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn run() -> Result<(), String> {
    Err("a network process runs on Linux and macOS hosts only so far".into())
}
