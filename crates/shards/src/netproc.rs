//! A VM's network process, started beside it (docs/design/architecture.md D31): the
//! frame ring and doorbells the two share, `shards-net` given its side of them, and the
//! VM's side ready for `shards-vm --net`.

use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

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
}

impl VmSide {
    /// `--net`'s value for a VM given this side at [`VM_FDS`] with `mac`.
    pub fn arg(mac: &[u8; 6]) -> String {
        let [r, s, w] = VM_FDS;
        let mac: Vec<String> = mac.iter().map(|b| format!("{b:02x}")).collect();
        format!("{r},{s},{w},{}", mac.join(":"))
    }
}

/// Starts the network process of one VM whose guest has `mac`, its binary beside
/// `vm_binary`, under `policy`.
/// Its side of the ring goes to it; the VM's comes back. Once the VM process holds its
/// side and the caller drops this one, the VM's end alone keeps the network process's
/// doorbell open, so the network process goes with the VM.
pub fn start(
    vm_binary: &Path,
    policy: shards_net::Policy,
    mac: &[u8; 6],
) -> Result<(shards_ipc::Child, VmSide), String> {
    let at = |e: std::io::Error| format!("a VM's network: {e}");
    let region = shards_netring::memory().map_err(at)?;
    let (vm_sleeps, net_rings) = shards_netring::doorbell().map_err(at)?;
    let (net_sleeps, vm_rings) = shards_netring::doorbell().map_err(at)?;
    let binary = vm_binary.with_file_name(format!("shards-net{}", std::env::consts::EXE_SUFFIX));
    let policy = match policy {
        shards_net::Policy::AllowAll => "allow",
        shards_net::Policy::DenyAll => "deny",
    };
    let err = std::io::stderr();
    let mac: Vec<String> = mac.iter().map(|b| format!("{b:02x}")).collect();
    let mac = mac.join(":");
    let child = shards_ipc::spawn(
        &binary,
        &[
            "--ring".as_ref(),
            "3,4,5".as_ref(),
            "--policy".as_ref(),
            policy.as_ref(),
            "--mac".as_ref(),
            mac.as_ref(),
        ],
        &[
            (err.as_fd(), 2),
            (region.as_fd(), 3),
            (net_sleeps.as_fd(), 4),
            (net_rings.as_fd(), 5),
        ],
        false,
    )
    .map_err(|e| format!("starting {}: {e}", binary.display()))?;
    Ok((
        child,
        VmSide {
            region,
            sleeps: vm_sleeps,
            rings: vm_rings,
        },
    ))
}

/// Waits for a network process whose VM is gone: it goes as its doorbell hangs up, and is
/// ended after a second if it has not.
pub fn reap(net: &shards_ipc::Child) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while net.try_wait().is_none() {
        if std::time::Instant::now() > deadline {
            let _ = net.kill(libc::SIGKILL);
            let _ = net.wait();
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
