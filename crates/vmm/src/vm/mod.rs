//! A microVM: configuration, lifecycle, and exit reasons, independent of the backend
//! that runs it.

use std::path::PathBuf;

#[cfg(all(hv, target_arch = "aarch64"))]
mod aarch64;
#[cfg(all(hv, target_arch = "aarch64"))]
use aarch64 as machine;
#[cfg(all(hv, target_arch = "x86_64"))]
mod x86_64;
#[cfg(all(hv, target_arch = "x86_64"))]
use x86_64 as machine;
/// Whether this build's VMs can be snapshotted and restored: every backend's can.
pub const SNAPSHOTS: bool = cfg!(hv);
/// Whether a VM resumed from its snapshot records its working set, for restores to
/// prefetch: every backend's can, HVF's at stage 2 (hv::hvf::Watch), KVM's from the
/// pages its host maps (vm::x86_64::recorder).
pub const WORKING_SETS: bool = cfg!(hv);
/// Which run records it. HVF's: the one that saves the template, whose command recording
/// slows sixfold (PM M30), on a run that boots anyway. KVM's: the first warm restore
/// without one, for which recording costs nothing, and whose pages later restores touch,
/// where the saving run's they barely do (PM M33).
pub const RESTORES_RECORD: bool = cfg!(hv = "kvm");

#[cfg(any(hv, test))]
mod barrier;
#[cfg(hv)]
mod runtime;
#[cfg(hv)]
pub use runtime::{
    Handle, Running, accept_working_set, check_host, max_vcpus, restore, start, working_set_limit,
};

#[cfg(not(hv))]
mod unsupported;
#[cfg(not(hv))]
pub use unsupported::{
    Handle, Running, accept_working_set, check_host, max_vcpus, restore, start, working_set_limit,
};

/// A snapshot's vsock device and the restore's socket path come together or not at all.
#[cfg(hv)]
fn check_vsock(snap: &crate::snapshot::Snapshot, host: Option<&VsockHost>) -> Result<(), String> {
    match (snap.config.vsock, host) {
        (true, None) => Err(
            "the snapshot has a vsock device: give the restored VM its own socket with --vsock PATH".into(),
        ),
        (false, Some(_)) => Err("the snapshot has no vsock device for --vsock".into()),
        _ => Ok(()),
    }
}

/// `cfg`'s machine, its backing files resolved once to absolute paths: a snapshot of it
/// names the files the machine had, wherever it is restored from (audit A18).
#[cfg(hv)]
fn machine_config(cfg: &Config) -> Result<crate::snapshot::MachineConfig, String> {
    let resolve = |path: &std::path::Path| {
        crate::platform::input_path(path).map_err(|e| format!("{}: {e}", path.display()))
    };
    Ok(crate::snapshot::MachineConfig {
        vcpus: cfg.vcpus,
        memory_mib: cfg.memory_mib,
        disks: cfg
            .disks
            .iter()
            .map(|d| Ok((resolve(&d.path)?, d.read_only)))
            .collect::<Result<_, String>>()?,
        pmem: cfg
            .pmem
            .iter()
            .map(|p| resolve(p))
            .collect::<Result<_, String>>()?,
        vsock: cfg.vsock.is_some(),
    })
}

/// `working_set` if every page of it, `page` bytes long, lies in one of `ranges`, the
/// `(guest address, length)` of the guest's RAM and pmem. Else nothing: the restore goes
/// without it rather than prefetch what the guest does not have (audit A16).
#[cfg(hv)]
fn usable_working_set(
    working_set: Vec<crate::hv::Touch>,
    page: u64,
    ranges: &[(u64, u64)],
) -> Vec<crate::hv::Touch> {
    let inside = |gpa: u64| {
        gpa.checked_add(page).is_some_and(|end| {
            ranges
                .iter()
                .any(|&(start, len)| gpa >= start && start.checked_add(len).is_some_and(|stop| end <= stop))
        })
    };
    match working_set.iter().find(|t| !inside(t.gpa)) {
        None => working_set,
        Some(t) => {
            crate::warn!(
                "the working set names {:#x}, outside the guest's memory; restoring without prefetching it",
                t.gpa
            );
            Vec::new()
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub kernel: PathBuf,
    /// A prebuilt initramfs image.
    pub initrd: Option<PathBuf>,
    /// A guest executable to run as PID 1 from a generated initramfs (exclusive with
    /// `initrd`).
    pub init: Option<PathBuf>,
    pub cmdline: String,
    pub vcpus: u32,
    pub memory_mib: u64,
    /// Where guest console (UART) output goes.
    pub console: Console,
    /// virtio-blk disks, in the order the guest enumerates them (vda, vdb, ...).
    pub disks: Vec<Disk>,
    /// What to do when the guest asks for a snapshot; without it, requests are ignored.
    pub snapshot: Option<SnapshotPolicy>,
    /// Read-only virtio-pmem devices backed by these files, in guest order (pmem0, ...).
    pub pmem: Vec<PathBuf>,
    /// A virtio-vsock device, and where its host side is.
    pub vsock: Option<VsockHost>,
}

/// The host side of a VM's virtio-vsock device.
#[derive(Debug, Clone, Default)]
pub struct VsockHost {
    /// A Unix socket host clients dial (`CONNECT <port>`), and the prefix of the sockets
    /// `<path>_<port>` that guest connections to other host ports reach, as Firecracker's.
    pub path: Option<PathBuf>,
    /// Host ports this process serves itself. A guest connection to one arrives on its
    /// sender as one end of a socket pair, with no socket file anyone else could reach,
    /// dial first or need to be granted (D30).
    #[cfg(unix)]
    pub ports: Vec<(u32, std::sync::mpsc::Sender<std::os::unix::net::UnixStream>)>,
}

impl VsockHost {
    /// A host side at `path` alone.
    pub fn at(path: PathBuf) -> VsockHost {
        VsockHost {
            path: Some(path),
            #[cfg(unix)]
            ports: Vec::new(),
        }
    }
}

impl Config {
    /// A VM with shards' defaults: 1 CPU, 256 MiB, and a console on stdout.
    pub fn new(kernel: PathBuf, init: Option<PathBuf>) -> Config {
        Config {
            kernel,
            initrd: None,
            init,
            cmdline: "console=ttyS0 earlycon panic=-1".into(),
            vcpus: 1,
            memory_mib: 256,
            console: Console::Stdout,
            disks: Vec::new(),
            snapshot: None,
            pmem: Vec::new(),
            vsock: None,
        }
    }
}

/// Starts a VM from a snapshot instead of booting one.
#[derive(Debug, Clone)]
pub struct RestoreConfig {
    /// The snapshot directory.
    pub dir: PathBuf,
    pub console: Console,
    /// For snapshots the restored guest asks for.
    pub snapshot: Option<SnapshotPolicy>,
    /// Prepare everything, then wait for [`Handle::release`]: a warm VM whose start
    /// request costs only the release.
    pub hold: bool,
    /// This VM's vsock device's host side. A snapshot with a vsock device needs one: the
    /// original VM may still hold its own path.
    pub vsock: Option<VsockHost>,
    /// Prefetch the snapshot's working set, if it has one, before the guest runs: for a
    /// restore ahead of its request, which it moves off the request's path (PM M30).
    pub prefetch: bool,
    /// Record a working set for the snapshot, where it has none and this backend records
    /// restores' ([`RESTORES_RECORD`]), until [`Handle::save_working_set`] saves it.
    pub record: bool,
}

#[derive(Debug, Clone)]
pub struct SnapshotPolicy {
    /// Where to write the snapshot when the guest asks for one.
    pub dir: PathBuf,
    pub then: AfterSnapshot,
    /// Once resumed from its snapshot, record the pages the guest touches, until
    /// [`Handle::save_working_set`] saves them with the snapshot.
    pub working_set: bool,
}

/// What the VM does once its snapshot is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterSnapshot {
    Resume,
    Stop,
}

#[derive(Debug, Clone)]
pub struct Disk {
    pub path: PathBuf,
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    Stdout,
    Discard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// The guest powered off (PSCI SYSTEM_OFF on arm64).
    PowerOff,
    /// The guest requested a reset (Linux does on reboot and, with `panic=-1`, on panic).
    Reset,
    /// Stopped by the host.
    Stopped,
    /// Stopped after writing a snapshot ([`AfterSnapshot::Stop`]).
    Snapshotted,
    Error(String),
}

/// Boots a VM and waits for it to exit.
pub fn run(cfg: &Config) -> Result<ExitReason, String> {
    let (_, running) = start(cfg)?;
    Ok(running.wait())
}

/// The initrd a machine boots: `--initrd`'s file, or an initramfs holding `--init`'s. Its
/// file is read only once its length is seen to fit the `room` bytes of RAM there are for
/// it, and an init is read straight into its place in the archive (audit D13).
#[cfg(hv)]
fn initrd(cfg: &Config, room: u64) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read as _;
    let open = |p: &PathBuf| -> Result<(std::fs::File, usize), String> {
        let at = |e: std::io::Error| format!("{}: {e}", p.display());
        let file = crate::platform::open_input(p, false).map_err(at)?;
        let len = file.metadata().map_err(at)?.len();
        let len = usize::try_from(len).map_err(|_| format!("{}: {len} bytes", p.display()))?;
        Ok((file, len))
    };
    let fits = |p: &PathBuf, len: usize| {
        if len as u64 > room {
            return Err(format!(
                "{}: {len} bytes, more than the {room} the guest's RAM has room for",
                p.display()
            ));
        }
        Ok(())
    };
    match (&cfg.initrd, &cfg.init) {
        (Some(_), Some(_)) => Err("--initrd and --init are mutually exclusive".into()),
        (Some(p), None) => {
            let (file, len) = open(p)?;
            fits(p, len)?;
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(len)
                .map_err(|e| format!("{}: {e}", p.display()))?;
            // Only the length that was seen to fit, though the file grow meanwhile.
            file.take(len as u64)
                .read_to_end(&mut bytes)
                .map_err(|e| format!("{}: {e}", p.display()))?;
            if bytes.len() != len {
                return Err(format!("{}: shorter than its {len} bytes", p.display()));
            }
            Ok(Some(bytes))
        }
        (None, Some(p)) => {
            let (mut file, len) = open(p)?;
            fits(p, crate::initramfs::archive_len(len))?;
            crate::initramfs::with_init_from(&mut file, len)
                .map(Some)
                .map_err(|e| format!("{}: {e}", p.display()))
        }
        (None, None) => Ok(None),
    }
}

#[cfg(all(test, hv))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::hv::Touch;

    /// Backing files are resolved when the machine is built, so a snapshot of it names
    /// them wherever it is restored from (audit A18). Tests run in their package's
    /// directory, which holds `Cargo.toml`.
    #[test]
    fn a_machines_backing_files_resolve_to_absolute_paths() {
        let mut cfg = Config::new(PathBuf::from("kernel"), None);
        cfg.disks = vec![Disk {
            path: PathBuf::from("Cargo.toml"),
            read_only: true,
        }];
        cfg.pmem = vec![PathBuf::from("./src/../Cargo.toml")];
        let machine = machine_config(&cfg).unwrap();
        let absolute = std::fs::canonicalize("Cargo.toml").unwrap();
        assert!(absolute.is_absolute());
        assert_eq!(machine.disks, vec![(absolute.clone(), true)]);
        assert_eq!(machine.pmem, vec![absolute]);
        cfg.pmem = vec![PathBuf::from("no-such-file")];
        assert!(machine_config(&cfg).unwrap_err().starts_with("no-such-file: "));
    }

    /// An init or initrd is read only if it fits where it goes: one longer than the room
    /// is refused, saying so; one that fits becomes the archive, or the initrd, whole
    /// (audit D13). `Cargo.toml` stands in for either.
    #[test]
    fn an_initrd_is_read_only_if_it_fits() {
        let len = std::fs::metadata("Cargo.toml").unwrap().len() as usize;
        let mut cfg = Config::new(PathBuf::from("kernel"), Some(PathBuf::from("Cargo.toml")));
        let archive = crate::initramfs::archive_len(len) as u64;
        let e = initrd(&cfg, archive - 1).unwrap_err();
        assert!(e.contains("room for"), "{e}");
        let built = initrd(&cfg, archive).unwrap().unwrap();
        assert_eq!(
            built,
            crate::initramfs::with_init(&std::fs::read("Cargo.toml").unwrap())
        );
        cfg.init = None;
        cfg.initrd = Some(PathBuf::from("Cargo.toml"));
        assert!(initrd(&cfg, len as u64 - 1).unwrap_err().contains("room for"));
        assert_eq!(initrd(&cfg, len as u64).unwrap().unwrap().len(), len);
        cfg.init = Some(PathBuf::from("Cargo.toml"));
        assert!(initrd(&cfg, u64::MAX).is_err(), "both at once");
    }

    /// A working set naming a page the guest does not have is not prefetched at all
    /// (audit A16).
    #[test]
    fn a_working_set_outside_the_guest_is_dropped() {
        let page = 0x4000;
        let ranges = [(0x8000_0000, 0x10_0000), (0x1_0000_0000, 0x20_0000)];
        let touch = |gpa| Touch { gpa, written: false };
        let inside = vec![touch(0x8000_0000), touch(0x800f_c000), touch(0x1_001f_c000)];
        assert_eq!(usable_working_set(inside.clone(), page, &ranges), inside);
        for outside in [0x8010_0000, 0x7fff_c000, 0x1_0020_0000, !(page - 1)] {
            let set = vec![touch(0x8000_0000), touch(outside)];
            assert!(usable_working_set(set, page, &ranges).is_empty(), "{outside:#x}");
        }
    }
}
