//! `shards run` boots a kernel directly in a microVM; `shards restore` resumes one
//! from a snapshot. Either can run a command in an image as `docker run` does
//! (workload.rs): `vm run --rootfs IMAGE -- COMMAND` boots into the image for it, and
//! `vm run --rootfs IMAGE --snapshot-dir DIR` saves a template, booted and mounted, that
//! `vm restore DIR -- COMMAND` resumes for each command.

use std::ffi::OsString;
use std::fmt::Display;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use shards_vmm::vm::{
    self, AfterSnapshot, Config, Console, Disk, ExitReason, Handle, RestoreConfig, Running, SnapshotPolicy,
    VsockHost,
};

use crate::spec::Options;
use crate::terminal::RawTerminal;

const RUN_USAGE: &str = "usage: shards run vm --kernel PATH [--initrd PATH | --init PATH] [--cmdline STR] [--cpus N] [--memory MIB] [--memory-max MIB] [--disk PATH[:ro]]... [--pmem PATH]... [--layer PATH] [--vsock PATH] [--nested] [--gpu] [--reporting] [--pci-test | --pci-tests SS.F,...] [--vfio NAME|NODE]... [--vfio-iommu NAME|NODE]... [--no-console] [--snapshot-dir DIR [--snapshot-then stop|resume]]
       shards run vm --kernel PATH --init SHARDS-INIT --rootfs IMAGE [OPTIONS] [WORKLOAD OPTIONS] -- COMMAND [ARG...]
       shards run vm --kernel PATH --init SHARDS-INIT --rootfs IMAGE --snapshot-dir DIR [OPTIONS]
  --pmem: a read-only virtio-pmem device backed by PATH: /dev/pmem0, pmem1, ... in order.
  --layer: a writable layer (D190): the ext4 file system in PATH, whole 2 MiB, on a
           virtio-pmem device after the --pmem ones, which the guest mounts with DAX and
           finds by the address shards' control page gives (LAYER).
  --reporting: a virtio-balloon device that gives the host back the memory the guest
               reports free.
  --rootfs: boot into the EROFS image IMAGE. With a COMMAND, run it there: its output is
            shards' output, and its exit status shards' exit status. With --snapshot-dir
            instead, save the VM to DIR once the image is mounted: a template for
            `shards restore vm DIR -- COMMAND`.
  Workload options: -e NAME[=VALUE], -w DIR, -u USER[:GROUP], --hostname NAME, -i.
  --vsock: a vsock device. Host programs connect to the Unix socket PATH and send
           `CONNECT <port>`; the guest's connections to host port P reach PATH_P.
  --snapshot-dir: where to write a snapshot when the guest asks for one (then stop, by default)
  --nested: virtualization for the guest itself: on arm64 its vCPUs have EL2, entered
            there, so that its kernel may be a hypervisor (KVM): on a Mac an M3 or
            later on macOS 15 or later, on Linux a CPU with FEAT_NV2 and a kernel booted
            with kvm-arm.mode=nested. Not with --snapshot-dir: its EL2 state is not
            saved.
  --memory-max: the most memory the guest may have: --memory at boot, and up to this
            much in all as it asks for more through shards' control page, plugged by a
            virtio-mem device. Whole 2 MiB blocks. A snapshot keeps none plugged.
  --gpu: this Mac's GPU, Vulkan through Venus, on the guest's render node
         /dev/dri/renderD128, where its kernel has DRM's virtio-gpu driver. With
         --snapshot-dir, a snapshot is refused once the guest has made a context or a
         resource of the GPU's: those are its host driver's, which no snapshot holds.
  --pci-test: a PCI bus with a test function at 00:01.0, the identification and liveness
              registers of QEMU's edu device in its BAR 0, for testing the bus. Not with
              --snapshot-dir.
  --pci-tests: test functions at each of these places on bus 0, as 01.0,02.0,02.1: a
               device of several a multi-function device, its function 0 alone with an
               INTx pin, and each device given its function 0.
  --vfio: the host's PCI function NAME (as 0000:01:00.0), passed through to the guest
          on its PCI bus: on Linux, bound to vfio-pci, as every other function in its
          IOMMU group must be. Or its VFIO device node (/dev/vfio/devices/vfioN), taken
          through /dev/iommu, not its group's node. Not with --snapshot-dir, --memory-max
          or --reporting: its DMA reaches all of guest RAM through mappings the host
          holds pinned.
  --vfio-iommu: as --vfio, the function behind the guest's own IOMMU (virtio-iommu):
                its DMA reaches only what the guest's IOMMU driver maps for it, as the
                guest's own VFIO needs to give it to a process, pinned as it is mapped,
                so --memory-max and --reporting work beside it. The guest's kernel needs
                VIRTIO_IOMMU. Not with --snapshot-dir.
  Console escape: Ctrl-A x stops the VM.";

const RESTORE_USAGE: &str = "usage: shards restore vm DIR [--hold] [--vsock PATH] [--no-console] [--gpu] [--backing PATH[:ro]]... [--layer PATH [--layer-cloned GENERATION]] [--snapshot-dir DIR [--snapshot-then stop|resume]]
       shards restore vm DIR [--hold] [--backing PATH[:ro]]... [--layer PATH [--layer-cloned GENERATION]] [WORKLOAD OPTIONS] -- COMMAND [ARG...]
       shards restore vm DIR --warm FD [--backing PATH[:ro]]... [--layer PATH [--layer-cloned GENERATION]]
       shards restore vm DIR --resume FD [--backing PATH[:ro]]... [--layer PATH [--layer-cloned GENERATION]]
       shards restore vm MICROVM [SNAPSHOT]
  Resumes the VM in snapshot directory DIR. Given a microVM's name rather than a directory
  holding a snapshot, resumes that stopped microVM from a snapshot of its own (`shards save
  vm`), its newest unless SNAPSHOT names one, where it was saved. With a COMMAND, DIR is a template saved by
  `shards run vm --rootfs`, and the command runs there.
  Workload options: -e NAME[=VALUE], -w DIR, -u USER[:GROUP], --hostname NAME, -i.
  --vsock: this VM's vsock socket; without a COMMAND, required when the snapshot has a
           vsock device.
  --hold: prepare the VM, print `shards-ready` on stderr, and start it when a line arrives
          on stdin: a warm VM whose start costs only the release. With a COMMAND, the VM
          resumes at once and connects, and the line runs the command: a warm VM whose
          request costs only the command.
  --warm: resume at once and connect, then take one command, with the stdio and connection
          of the client it is for, from the daemon on the Unix socket at descriptor FD.
  --resume: DIR is a run's snapshot, its command running already: take its run's stdio,
            log and swap from the daemon at descriptor FD, then resume it.
  --backing: a file the snapshot restores against, read-only with :ro: given, a snapshot
             naming any other is refused, its state being what the VM saving it wrote.
  --gpu: restore in the GPU's VM process, which a snapshot of a VM with a GPU needs.
  --layer: the restore's own file for the writable layer its snapshot kept (D190): empty,
           which it fills with a copy of the layer of the generation it resumes; or, with
           --layer-cloned, a copy-on-write clone of GENERATION's, taken as it is where that
           is the generation it resumes.
  Console escape: Ctrl-A x stops the VM.";

/// An argument that is text, with an error naming it if it is not UTF-8. Paths are taken
/// as the OS gives them: a home's may be any bytes on Linux.
fn text(arg: OsString) -> Result<String, String> {
    arg.into_string()
        .map_err(|a| format!("argument {a:?} is not valid UTF-8"))
}

/// `--vfio`'s or `--vfio-iommu`'s function: by its name, through its group; or by its
/// VFIO device node, `/dev/vfio/devices/vfioN`, through iommufd (D154).
fn vfio(given: &str) -> Result<vm::Vfio, String> {
    if given.starts_with("/dev/vfio/devices/") {
        vm::Vfio::of_cdev(Path::new(given))
    } else {
        vm::Vfio::of(given)
    }
}

/// `--pci-tests`' places, each `SS.F` in hex, as `01.0,02.0,02.1` (D153): devices 1 to 31
/// of bus 0, functions 0 to 7, each once, and each device with its function 0, by which a
/// guest finds its others.
fn pci_tests(text: &str) -> Result<Vec<(u8, u8)>, String> {
    let bad = |what: String| format!("--pci-tests {text}: {what}");
    let mut places: Vec<(u8, u8)> = Vec::new();
    for place in text.split(',') {
        let (device, function) = place
            .split_once('.')
            .ok_or_else(|| bad(format!("{place:?} is not SS.F")))?;
        let device = u8::from_str_radix(device, 16)
            .ok()
            .filter(|d| (1..32).contains(d))
            .ok_or_else(|| bad(format!("{place:?}: its device is one of 01 to 1f")))?;
        let function = u8::from_str_radix(function, 16)
            .ok()
            .filter(|f| *f < 8)
            .ok_or_else(|| bad(format!("{place:?}: its function is one of 0 to 7")))?;
        if places.contains(&(device, function)) {
            return Err(bad(format!("{place} twice")));
        }
        places.push((device, function));
    }
    if let Some(&(device, _)) = places.iter().find(|&&(d, _)| !places.contains(&(d, 0))) {
        return Err(bad(format!(
            "device {device:02x} has no function 0, by which a guest finds its others"
        )));
    }
    Ok(places)
}

/// A `--disk` value: its path, and whether it ends in `:ro`.
fn disk(spec: OsString) -> Disk {
    let bytes = spec.as_encoded_bytes();
    match bytes.strip_suffix(b":ro") {
        Some(path) => Disk {
            // SAFETY: split just before `:ro`, a non-empty UTF-8 substring, which
            // `OsStr::from_encoded_bytes_unchecked` documents as a valid boundary.
            path: PathBuf::from(unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(path) }),
            read_only: true,
        },
        None => Disk {
            path: PathBuf::from(spec),
            read_only: false,
        },
    }
}

/// Options `run` and `restore` share.
struct Common {
    console: Console,
    snapshot_dir: Option<PathBuf>,
    then: AfterSnapshot,
    vsock: Option<PathBuf>,
    /// A network device, and its network process's side (D31).
    net: Option<shards_vmm::devices::virtio::net::NetHost>,
    workload: Options,
    /// Whether a workload option was given.
    workload_options: bool,
}

impl Common {
    fn new() -> Common {
        Common {
            console: Console::Stdout,
            snapshot_dir: None,
            then: AfterSnapshot::Stop,
            vsock: None,
            net: None,
            workload: Options::default(),
            workload_options: false,
        }
    }

    /// Takes `arg` if it is a shared option; returns whether it was.
    fn option(
        &mut self,
        arg: &str,
        value: &mut dyn FnMut(&str) -> Result<OsString, String>,
    ) -> Result<bool, String> {
        let w = &mut self.workload;
        match arg {
            "--no-console" => self.console = Console::Discard,
            "--snapshot-dir" => self.snapshot_dir = Some(PathBuf::from(value("--snapshot-dir")?)),
            "--snapshot-then" => {
                self.then = match text(value("--snapshot-then")?)?.as_str() {
                    "stop" => AfterSnapshot::Stop,
                    "resume" => AfterSnapshot::Resume,
                    other => return Err(format!("--snapshot-then: {other:?} is not stop or resume")),
                }
            }
            "--vsock" => self.vsock = Some(PathBuf::from(value("--vsock")?)),
            "--net" => {
                if self.net.is_some() {
                    return Err("--net given twice".into());
                }
                self.net = Some(net_host(&text(value("--net")?)?)?);
            }
            #[cfg(unix)]
            "--net-release" => {
                let fd = text(value("--net-release")?)?;
                let fd: i32 = fd
                    .parse()
                    .map_err(|_| format!("--net-release: {fd:?} is not a descriptor"))?;
                crate::warm::adopt_release(fd)?;
            }
            "--cwd" => {
                let dir = PathBuf::from(value("--cwd")?);
                if !dir.is_absolute() {
                    return Err(format!("--cwd: {} is not an absolute path", dir.display()));
                }
                CWD.set(dir).map_err(|_| "--cwd given twice".to_string())?;
            }
            "--grants" => grants(&text(value("--grants")?)?)?,
            "-e" | "--env" => w.env.push(text(value("--env")?)?),
            "-w" | "--workdir" => w.workdir = text(value("--workdir")?)?,
            "-u" | "--user" => w.user = text(value("--user")?)?,
            "--hostname" => w.hostname = Some(text(value("--hostname")?)?),
            "-i" | "--interactive" => w.interactive = true,
            _ => return Ok(false),
        }
        self.workload_options |= matches!(
            arg,
            "-e" | "--env" | "-w" | "--workdir" | "-u" | "--user" | "--hostname" | "-i" | "--interactive"
        );
        Ok(true)
    }

    fn policy(&mut self) -> Option<SnapshotPolicy> {
        let then = self.then;
        self.snapshot_dir.take().map(|dir| SnapshotPolicy {
            dir,
            then,
            working_set: false,
        })
    }

    /// Checks that workload options come with a command.
    fn check_workload(&self) -> Result<(), String> {
        if self.workload_options && self.workload.argv.is_empty() {
            return Err("-e, -w, -u, --hostname and -i need a command after --".into());
        }
        Ok(())
    }
}

/// What `vm run` boots.
enum Mode {
    /// The kernel and init as given.
    Plain,
    /// Into an image, for a command.
    Workload(PathBuf),
    /// Into an image, to save it as a template.
    Template(PathBuf),
    /// Into an image, as a warm VM that takes one command from the daemon on `fd`;
    /// with a snapshot policy, it saves a template on the way.
    Warm { rootfs: PathBuf, fd: i32 },
}

struct Run {
    cfg: Config,
    mode: Mode,
    workload: Options,
    /// `--attach FD` (Windows: `--attach NAME`, the name its spawner handed it by): the
    /// socket a builder attaches files to the VM's join disk on (D134).
    attach: Option<String>,
}

fn parse_run(args: impl Iterator<Item = OsString>) -> Result<Run, String> {
    let mut args = args;
    let mut rootfs = None;
    let mut cfg = Config::new(PathBuf::new(), None);
    // Its disks and shared directories charge its I/O limits, whose rates its run brings
    // (D178).
    cfg.io_limit = Some(&IO);
    let (mut kernel, mut common, mut warm) = (None, Common::new(), None);
    let mut memory_max = None;
    let mut attach = None;
    while let Some(arg) = args.next() {
        let arg = text(arg)?;
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        if common.option(&arg, &mut value)? {
            continue;
        }
        match arg.as_str() {
            "--kernel" => kernel = Some(PathBuf::from(value("--kernel")?)),
            "--initrd" => cfg.initrd = Some(PathBuf::from(value("--initrd")?)),
            "--init" => cfg.init = Some(PathBuf::from(value("--init")?)),
            "--cmdline" => cfg.cmdline = text(value("--cmdline")?)?,
            "--cpus" => {
                cfg.vcpus = text(value("--cpus")?)?
                    .parse()
                    .map_err(|e| format!("--cpus: {e}"))?
            }
            "--memory" => {
                cfg.memory_mib = text(value("--memory")?)?
                    .parse()
                    .map_err(|e| format!("--memory: {e}"))?
            }
            "--memory-max" => {
                memory_max = Some(
                    text(value("--memory-max")?)?
                        .parse::<u64>()
                        .map_err(|e| format!("--memory-max: {e}"))?,
                )
            }
            "--disk" => cfg.disks.push(disk(value("--disk")?)),
            // Shared directories its run gives it (D38): slots, filled as the run arrives.
            #[cfg(unix)]
            "--shares" => {
                let n: u32 = text(value("--shares")?)?
                    .parse()
                    .map_err(|e| format!("--shares: {e}"))?;
                cfg.shares = (0..n).map(|_| std::sync::Arc::default()).collect();
                let _ = SHARES.set(cfg.shares.clone());
            }
            // A join disk, empty until a container joins the VM's network (D119).
            "--join" => cfg.join = Some(give_join_disk()),
            // A DAX window of this many MiB for each shared directory (D189).
            "--share-window" => {
                cfg.share_window_mib = text(value("--share-window")?)?
                    .parse()
                    .map_err(|e| format!("--share-window: {e}"))?
            }
            // A swap window of this many MiB (D159), a run's swap mapped into it.
            "--swap-window" => {
                cfg.swap_mib = text(value("--swap-window")?)?
                    .parse()
                    .map_err(|e| format!("--swap-window: {e}"))?
            }
            // A join disk its builder attaches files to on this socket, a cache's layers
            // and the files its steps write theirs to (D134); the VM ends as the socket
            // closes, its builder gone (D137).
            "--attach" => {
                attach = Some(text(value("--attach")?)?);
                JOIN.take_writes();
                cfg.join = Some(give_join_disk());
            }
            // A join share, served by none until a joiner brings volumes (D119).
            #[cfg(unix)]
            "--join-share" => cfg.join_share = Some(give_join_share()),
            "--pmem" => cfg.pmem.push(PathBuf::from(value("--pmem")?)),
            // A writable layer (D190), its ext4 file system in this file.
            "--layer" => {
                cfg.layer = Some(PathBuf::from(value("--layer")?));
                LAYERED.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            "--nested" => cfg.nested = true,
            "--gpu" => cfg.gpu = true,
            "--pci-test" => cfg.pci_test = vec![(vm::TEST_DEVICE, 0)],
            "--pci-tests" => cfg.pci_test = pci_tests(&text(value("--pci-tests")?)?)?,
            "--vfio" => cfg.vfio.push(vfio(&text(value("--vfio")?)?)?),
            "--vfio-iommu" => cfg.vfio.push(vm::Vfio {
                iommu: true,
                ..vfio(&text(value("--vfio-iommu")?)?)?
            }),
            "--reporting" => cfg.reporting = true,
            "--rootfs" => rootfs = Some(PathBuf::from(value("--rootfs")?)),
            "--warm" => {
                let fd = text(value("--warm")?)?;
                warm = Some(
                    fd.parse::<i32>()
                        .map_err(|_| format!("--warm: {fd:?} is not a descriptor"))?,
                );
            }
            "--" => {
                common.workload.argv = args.by_ref().map(text).collect::<Result<_, _>>()?;
                break;
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    common.check_workload()?;
    if !cfg.pci_test.is_empty() && common.snapshot_dir.is_some() {
        return Err("--pci-test with --snapshot-dir: a machine with a PCI bus is not snapshotted, its functions' state their devices' own".into());
    }
    if !cfg.vfio.is_empty() {
        if common.snapshot_dir.is_some() {
            return Err(
                "--vfio or --vfio-iommu with --snapshot-dir: a host's device is not snapshotted, its state its own".into(),
            );
        }
        if cfg.vfio.iter().any(|f| !f.iommu) && (memory_max.is_some() || cfg.reporting) {
            return Err("--vfio with --memory-max or --reporting: the device's DMA reaches all of guest RAM through mappings the host holds pinned, which neither may take away; behind the guest's IOMMU (--vfio-iommu) it reaches what the guest maps alone".into());
        }
    }
    if cfg.nested && common.snapshot_dir.is_some() {
        return Err("--nested with --snapshot-dir: a VM with virtualization of its own is not snapshotted, its EL2 state not saved".into());
    }
    if let Some(max) = memory_max {
        cfg.hotplug_mib = max
            .checked_sub(cfg.memory_mib)
            .ok_or_else(|| format!("--memory-max {max} is less than --memory {}", cfg.memory_mib))?;
    }
    cfg.kernel = kernel.ok_or("--kernel is required")?;
    cfg.snapshot = common.policy();
    cfg.console = common.console;
    cfg.vsock = common.vsock.map(VsockHost::at);
    cfg.net = common.net.take();
    let command = !common.workload.argv.is_empty();
    if let Some(fd) = warm {
        let rootfs = rootfs.ok_or("--warm needs --rootfs")?;
        if command || common.workload_options || cfg.vsock.is_some() {
            return Err(
                "--warm takes its command from the daemon: no --vsock, workload options or command".into(),
            );
        }
        // A template saved on the way resumes to serve its request, and records what that
        // touches: the working set its restores prefetch (PM M30).
        if let Some(policy) = cfg.snapshot.as_mut() {
            policy.then = AfterSnapshot::Resume;
            policy.working_set = true;
        }
        if attach.is_some() {
            return Err("--warm takes its files from the daemon: no --attach".into());
        }
        return Ok(Run {
            cfg,
            mode: Mode::Warm { rootfs, fd },
            workload: common.workload,
            attach,
        });
    }
    let mode = match rootfs {
        Some(rootfs) if command => Mode::Workload(rootfs),
        Some(rootfs) if cfg.snapshot.is_some() => Mode::Template(rootfs),
        Some(_) => return Err("--rootfs needs a command after --, or --snapshot-dir for a template".into()),
        None if command => return Err("a command needs --rootfs".into()),
        None => Mode::Plain,
    };
    Ok(Run {
        cfg,
        mode,
        workload: common.workload,
        attach,
    })
}

struct Restore {
    cfg: RestoreConfig,
    workload: Options,
    /// `--warm FD`: the daemon's socket.
    warm: Option<i32>,
    /// `--resume FD`, in place of `--warm FD`: the snapshot is a run's, whose command goes
    /// on (D176).
    resume: bool,
}

fn parse_restore(args: impl Iterator<Item = OsString>) -> Result<Restore, String> {
    let mut args = args;
    let (mut dir, mut common, mut hold, mut warm, mut resume) = (None, Common::new(), false, None, false);
    let mut backing = Vec::new();
    let mut layer = None;
    let mut layer_cloned = None;
    while let Some(arg) = args.next() {
        // The snapshot directory is a path, in whatever bytes; every option is text.
        let Some(flag) = arg.to_str().map(str::to_owned) else {
            if dir.is_some() {
                return Err(format!("unexpected argument {arg:?}"));
            }
            dir = Some(PathBuf::from(arg));
            continue;
        };
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        if common.option(&flag, &mut value)? {
            continue;
        }
        match flag.as_str() {
            "-h" | "--help" => return Err(String::new()),
            "--hold" => hold = true,
            // The GPU's VM process restores it, which a snapshot of a machine with a GPU
            // needs (D150): the binary is chosen by it, the GPU by the snapshot.
            "--gpu" => {}
            "--backing" => backing.push(disk(value("--backing")?)),
            // The restore's own copy of the writable layer its snapshot kept (D190).
            "--layer" => {
                layer = Some(PathBuf::from(value("--layer")?));
                LAYERED.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            // Which generation's layer that file is a clone of.
            "--layer-cloned" => layer_cloned = Some(text(value("--layer-cloned")?)?),
            "--warm" => {
                let fd = text(value("--warm")?)?;
                warm = Some(
                    fd.parse::<i32>()
                        .map_err(|_| format!("--warm: {fd:?} is not a descriptor"))?,
                );
            }
            // A run's snapshot (D176): served as a warm VM's run is, its command running
            // already.
            "--resume" => {
                let fd = text(value("--resume")?)?;
                warm = Some(
                    fd.parse::<i32>()
                        .map_err(|_| format!("--resume: {fd:?} is not a descriptor"))?,
                );
                resume = true;
            }
            "--" => {
                common.workload.argv = args.by_ref().map(text).collect::<Result<_, _>>()?;
                break;
            }
            flag if flag.starts_with('-') => return Err(format!("unknown argument {flag:?}")),
            _ if dir.is_some() => return Err(format!("unexpected argument {flag:?}")),
            _ => dir = Some(PathBuf::from(flag)),
        }
    }
    common.check_workload()?;
    if !backing.is_empty() {
        let given = backing.into_iter().map(|d: Disk| (d.path, d.read_only)).collect();
        BACKING
            .set(given)
            .map_err(|_| "--backing given twice".to_string())?;
    }
    if warm.is_some()
        && (hold
            || common.workload_options
            || !common.workload.argv.is_empty()
            || common.snapshot_dir.is_some()
            || common.vsock.is_some())
    {
        return Err("--warm takes its command from the daemon: no --hold, --vsock, --snapshot-dir, workload options or command".into());
    }
    let dir = dir.ok_or("the snapshot directory is required")?;
    if layer_cloned.is_some() && layer.is_none() {
        return Err("--layer-cloned says whose clone --layer's file is: it needs --layer".into());
    }
    let cfg = RestoreConfig {
        dir,
        console: common.console,
        snapshot: common.policy(),
        // A run's, held until its request maps its swap back and its guest's connections
        // are awaited, which its guest dials as it resumes (D176).
        hold: hold || resume,
        vsock: common.vsock.map(VsockHost::at),
        net: common.net.take(),
        // Its slots are made once its snapshot is read (`restore_vm`).
        #[cfg(unix)]
        shares: Vec::new(),
        // As its join disk and join share are, where the snapshot has them.
        join: None,
        #[cfg(unix)]
        join_share: None,
        io_limit: Some(&IO),
        // Restored ahead of its request: the prefetch costs the request nothing.
        prefetch: hold || warm.is_some(),
        // A warm VM's request ends the recording as it ends a template's (RECORD_FOR); a
        // run's snapshot is no template, which restores prefetch from.
        record: warm.is_some() && !resume,
        swap: None,
        layer,
        layer_cloned,
    };
    Ok(Restore {
        cfg,
        workload: common.workload,
        warm,
        resume,
    })
}

/// Whether this VM's writable layer is an ext4 file system in a file of its own (D190), its
/// host's to keep, rather than a tmpfs in its memory.
static LAYERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// [`LAYERED`]: read where a run keeps its layer (workload.rs), a Unix host's.
#[cfg(unix)]
pub fn layered() -> bool {
    LAYERED.load(std::sync::atomic::Ordering::Relaxed)
}

/// This VM's shared directories' slots, which its run fills (D38).
#[cfg(unix)]
pub static SHARES: std::sync::OnceLock<Vec<shards_vmm::devices::virtio::fs::Share>> =
    std::sync::OnceLock::new();

/// This VM's join disk, which containers joining its network are given ranges of (D119):
/// one VM a process, so one disk, a `static` its device and its join requests both reach.
static JOIN: shards_vmm::devices::virtio::block::Join = shards_vmm::devices::virtio::block::Join::new();
/// This VM's I/O limits (D178), which its disks and shared directories charge: one VM a
/// process, so one set, given their rates as its run arrives.
static IO: shards_vmm::devices::virtio::limit::Limiter = shards_vmm::devices::virtio::limit::Limiter::new();
/// Whether this VM has [`JOIN`]: its device given it.
static HAS_JOIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Saves this VM for its daemon (D176): into the files of a generation the daemon
/// prepared, then on or stopped ([`Handle::save`]).
#[cfg(unix)]
pub fn save(save: vm::Save) -> Result<vm::Saved, String> {
    RUNNING.get().ok_or("the microVM has not started")?.save(save)
}

/// Sets the most memory this VM's guest may plug past what it booted with, its run's
/// limit updated (`PLUG`, D148).
#[cfg(unix)]
pub fn set_plug_most(bytes: u64) -> Result<(), String> {
    RUNNING
        .get()
        .ok_or("the microVM has not started")?
        .set_plug_most(bytes)
}

/// Holds this VM's vCPUs together to `cpus` CPUs' time of the host's, its run's CPU
/// limits updated (`CPUS`, D191).
#[cfg(unix)]
pub fn hold_cpus(cpus: u32) -> Result<(), String> {
    RUNNING
        .get()
        .ok_or("the microVM has not started")?
        .hold_cpus(cpus)
}

/// This VM's join disk, for its device.
fn give_join_disk() -> &'static shards_vmm::devices::virtio::block::Join {
    HAS_JOIN.store(true, std::sync::atomic::Ordering::Release);
    &JOIN
}

/// Serves the requests of the builder that spawned this VM (`--attach`, D134) on a thread
/// of its own, until it hangs up: each a file attached to the VM's join disk, read-only
/// or writable as it asks, answered with its range, or a range let go.
#[cfg(unix)]
fn serve_attach(fd: &str) -> Result<(), String> {
    use shards_ipc::kind;
    let fd = fd
        .parse::<i32>()
        .map_err(|_| format!("--attach: {fd:?} is not a descriptor"))?;
    let socket = crate::warm::inherited_socket("--attach", fd)?;
    let answer = |m: shards_ipc::Message| -> Result<(u8, Vec<u8>), String> {
        match m.kind {
            kind::DISK_ATTACH => {
                let mut fds = m.fds.into_iter();
                let (Some(fd), None) = (fds.next(), fds.next()) else {
                    return Err("an attach without its one file".into());
                };
                let file = std::fs::File::from(fd);
                let (start, len) = match m.payload.as_slice() {
                    [0] => JOIN.attach(file),
                    [1] => JOIN.attach_writable(file),
                    _ => Err("an attach neither read-only nor writable".into()),
                }?;
                Ok((
                    kind::DISK_ATTACH,
                    [start.to_be_bytes(), len.to_be_bytes()].concat(),
                ))
            }
            kind::DISK_DETACH => {
                let start = m
                    .payload
                    .first_chunk::<8>()
                    .map(|b| u64::from_be_bytes(*b))
                    .ok_or("a detach without its range")?;
                JOIN.detach(start)?;
                Ok((kind::DISK_DETACH, Vec::new()))
            }
            other => Err(format!("message kind {other}")),
        }
    };
    std::thread::Builder::new()
        .name("attach".into())
        .spawn(move || {
            while let Ok(Some(m)) = shards_ipc::recv(&socket) {
                let (kind, payload) = answer(m).unwrap_or_else(|e| (kind::DISK_REFUSED, e.into_bytes()));
                if shards_ipc::send(&socket, kind, &payload, &[]).is_err() {
                    break;
                }
            }
            // Its builder is gone, even killed: so is the builder's VM, whatever its guest
            // is doing (a build held at its memory may hear nothing, D137), and with it
            // its network process, which hears its doorbell hang up. The builder holds
            // this socket for as long as it holds the VM.
            report("its builder is gone, and so this builder ends");
            std::process::exit(1);
        })
        .map(drop)
        .map_err(|e| format!("serving its builder: {e}"))
}

/// [`serve_attach`] on Windows: the socket its spawner handed it as `name`, and each
/// attached file a handle the builder duplicated into this process, its value after the
/// mode, as a message on Windows carries no handle (shards_ipc).
#[cfg(windows)]
fn serve_attach(name: &str) -> Result<(), String> {
    use shards_ipc::kind;
    use std::os::windows::io::{FromRawHandle as _, OwnedHandle, RawHandle};
    let socket = {
        let mut given = GIVEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let at = given
            .iter()
            .position(|(n, h)| n == name && matches!(h, shards_ipc::Handed::Socket(_)))
            .ok_or_else(|| format!("--attach: no socket {name} was given"))?;
        let shards_ipc::Handed::Socket(info) = given.swap_remove(at).1 else {
            return Err(format!("--attach: {name} is no socket"));
        };
        shards_vmm::platform::UnixStream::from_shared(&info).map_err(|e| format!("--attach: {e}"))?
    };
    let answer = |m: shards_ipc::Message| -> Result<(u8, Vec<u8>), String> {
        match m.kind {
            kind::DISK_ATTACH => {
                let (mode, value) = m
                    .payload
                    .split_first()
                    .and_then(|(mode, rest)| Some((*mode, u64::from_le_bytes(*rest.first_chunk::<8>()?))))
                    .ok_or("an attach without its one file")?;
                let value = usize::try_from(value)
                    .ok()
                    .filter(|v| *v != 0)
                    .ok_or("an attach without its one file")?;
                // SAFETY: a handle the builder duplicated into this process for this attach
                // alone, owned from here on.
                let file = std::fs::File::from(unsafe { OwnedHandle::from_raw_handle(value as RawHandle) });
                let (start, len) = match mode {
                    0 => JOIN.attach(file),
                    1 => JOIN.attach_writable(file),
                    _ => Err("an attach neither read-only nor writable".into()),
                }?;
                Ok((
                    kind::DISK_ATTACH,
                    [start.to_be_bytes(), len.to_be_bytes()].concat(),
                ))
            }
            kind::DISK_DETACH => {
                let start = m
                    .payload
                    .first_chunk::<8>()
                    .map(|b| u64::from_be_bytes(*b))
                    .ok_or("a detach without its range")?;
                JOIN.detach(start)?;
                Ok((kind::DISK_DETACH, Vec::new()))
            }
            other => Err(format!("message kind {other}")),
        }
    };
    std::thread::Builder::new()
        .name("attach".into())
        .spawn(move || {
            while let Ok(Some(m)) = shards_ipc::recv(&socket) {
                let (kind, payload) = answer(m).unwrap_or_else(|e| (kind::DISK_REFUSED, e.into_bytes()));
                if shards_ipc::send(&socket, kind, &payload).is_err() {
                    break;
                }
            }
            // Its builder is gone, even killed: so is the builder's VM, as on Unix.
            report("its builder is gone, and so this builder ends");
            std::process::exit(1);
        })
        .map(drop)
        .map_err(|e| format!("serving its builder: {e}"))
}

/// This VM's handle, once it runs: what a joiner's swap is mapped through (D159).
#[cfg(unix)]
static RUNNING: std::sync::OnceLock<Handle> = std::sync::OnceLock::new();

/// Maps `file`, a joiner's swap, into this VM's swap window (D159), unless a member's is
/// mapped already, which they share: its length where this one was mapped.
#[cfg(unix)]
pub fn attach_swap(file: std::fs::File) -> Result<Option<u64>, String> {
    RUNNING
        .get()
        .ok_or("the microVM is not running yet")?
        .attach_swap(file)
}

/// This VM's join disk, where it has one (D119).
#[cfg_attr(not(unix), allow(dead_code))]
pub fn join_disk() -> Option<&'static shards_vmm::devices::virtio::block::Join> {
    HAS_JOIN
        .load(std::sync::atomic::Ordering::Acquire)
        .then_some(&JOIN)
}

/// This VM's join share (D119): served by none until a container joining its network brings
/// volumes; one VM a process, so one share, a `static` its device and its joiners reach.
#[cfg(unix)]
static JOIN_SHARE: shards_vmm::devices::virtio::fs::Slot = shards_vmm::devices::virtio::fs::Slot::new();
/// Whether this VM has [`JOIN_SHARE`]: its device given it.
#[cfg(unix)]
static HAS_JOIN_SHARE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// This VM's join share, for its device.
#[cfg(unix)]
fn give_join_share() -> &'static shards_vmm::devices::virtio::fs::Slot {
    HAS_JOIN_SHARE.store(true, std::sync::atomic::Ordering::Release);
    &JOIN_SHARE
}

/// This VM's join share, where it has one (D119).
#[cfg(unix)]
pub fn join_share() -> Option<&'static shards_vmm::devices::virtio::fs::Slot> {
    HAS_JOIN_SHARE
        .load(std::sync::atomic::Ordering::Acquire)
        .then_some(&JOIN_SHARE)
}

/// Console output that never fails the caller (e.g. with stderr closed).
fn report(message: impl Display) {
    let _ = writeln!(std::io::stderr(), "shards: {message}");
}

fn forward_stdin(handle: vm::Handle) {
    let spawned = std::thread::Builder::new()
        .name("console-in".into())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 256];
            let mut escape = false;
            loop {
                let n = match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                for &b in buf.iter().take(n) {
                    match (escape, b) {
                        (true, b'x') => return handle.stop(),
                        (true, 0x01) => handle.console_input(&[0x01]), // Ctrl-A Ctrl-A sends one
                        (true, other) => handle.console_input(&[0x01, other]),
                        (false, 0x01) => {}
                        (false, other) => handle.console_input(&[other]),
                    }
                    escape = !escape && b == 0x01;
                }
            }
        });
    if let Err(e) = spawned {
        report(format!("console input unavailable: {e}"));
    }
}

/// The parsed configuration, or the exit code after printing help or a usage error.
fn parsed<T>(parse: Result<T, String>, usage: &str) -> Result<T, ExitCode> {
    match parse {
        Ok(cfg) => Ok(cfg),
        Err(e) if e.is_empty() => {
            let _ = writeln!(std::io::stdout(), "{usage}");
            Err(ExitCode::SUCCESS)
        }
        Err(e) => {
            report(format!("{e}\n{usage}"));
            Err(ExitCode::from(2))
        }
    }
}

/// Boots `cfg` into the image `rootfs` and runs `workload` there, as `vm run --rootfs`
/// does. Exits as the workload does.
pub fn run_in(mut cfg: Config, rootfs: PathBuf, workload: &Options) -> ExitCode {
    boot_into(&mut cfg, rootfs, false);
    cfg.console = Console::Discard;
    let source = Source::Given {
        options: workload,
        hold: false,
        gpu: cfg.gpu,
    };
    serve_workload(cfg.vsock.take(), source, move |vsock| {
        cfg.vsock = Some(vsock);
        start(&cfg)
    })
}

/// Boots into the image `rootfs`: /dev/pmem0, which shards-init mounts as the root.
fn boot_into(cfg: &mut Config, rootfs: PathBuf, template: bool) {
    cfg.pmem.insert(0, rootfs);
    // A workload's output is shards' output, so the kernel need not print to the console.
    // Without automatic task groups: in most templates, a restored guest's first setsid(2)
    // stalled until the next tick (docs/benchmarks.md, "Run"). Docker's containers never
    // get them anyway, since they live in cgroups (kernel/sched/autogroup.c).
    // Without real-time group scheduling (D155): a workload's cgroup, which cgroup v2 gives
    // no real-time time (its files are cgroup v1's, kernel/sched/core.c
    // cpu_rt_group_init), was refused SCHED_FIFO and SCHED_RR even with CAP_SYS_NICE
    // (EPERM, kernel/sched/syscalls.c); `rt_group_sched=0` (Linux 6.16) leaves real-time
    // time to the guest's own budget, which a run's `--cpu-rt-period` and
    // `--cpu-rt-runtime` set.
    cfg.cmdline
        .push_str(" quiet noautogroup rt_group_sched=0 shards_root=/dev/pmem0");
    if template {
        cfg.cmdline.push_str(" shards_template=1");
    }
}

pub fn run(args: impl Iterator<Item = OsString>) -> ExitCode {
    let Run {
        mut cfg,
        mode,
        workload,
        attach,
    } = match parsed(parse_run(args), RUN_USAGE) {
        Ok(run) => run,
        Err(code) => return code,
    };
    if let Some(at) = attach
        && let Err(e) = serve_attach(&at)
    {
        report(e);
        return ExitCode::FAILURE;
    }
    match mode {
        Mode::Plain => supervise(start(&cfg), cfg.console, false),
        Mode::Template(rootfs) => {
            boot_into(&mut cfg, rootfs, true);
            // Restored copies dial the host through this device, served by whatever
            // restores them.
            cfg.vsock.get_or_insert_with(VsockHost::default);
            supervise(start(&cfg), cfg.console, true)
        }
        Mode::Workload(rootfs) => run_in(cfg, rootfs, &workload),
        Mode::Warm { rootfs, fd } => warm_boot(cfg, rootfs, fd),
    }
}

pub fn restore(args: impl Iterator<Item = OsString>) -> ExitCode {
    let Restore {
        mut cfg,
        workload,
        warm,
        resume,
    } = match parsed(parse_restore(args), RESTORE_USAGE) {
        Ok(restore) => restore,
        Err(code) => return code,
    };
    if let Some(fd) = warm {
        return warm_restore(cfg, fd, resume);
    }
    if !workload.argv.is_empty() {
        cfg.console = Console::Discard;
        // A held run resumes now, and holds the command instead: whatever a restored guest
        // does first (its reseed, its connection) is done before the request.
        let hold = std::mem::take(&mut cfg.hold);
        let source = Source::Given {
            options: &workload,
            hold,
            gpu: false,
        };
        return serve_workload(cfg.vsock.take(), source, move |vsock| {
            cfg.vsock = Some(vsock);
            restore_vm(cfg)
        });
    }
    let (hold, console) = (cfg.hold, cfg.console);
    let started = restore_vm(cfg);
    if hold && let Ok((handle, _)) = &started {
        wait_for_release(handle);
    }
    supervise(started, console, false)
}

/// `--hold`: announces the prepared VM and releases it when a line (or end of input)
/// arrives on stdin.
fn wait_for_release(handle: &Handle) {
    let _ = writeln!(std::io::stderr(), "shards-ready");
    let _ = std::io::stdin().read_line(&mut String::new());
    handle.release();
}

/// Where a served VM's command comes from.
// Where shards has no vsock yet (Windows), nothing serves a command.
#[cfg_attr(not(unix), allow(dead_code))]
enum Source<'a> {
    /// The command line's: sent once the guest connects or, with `hold`, once a line then
    /// arrives on stdin; given the GPU's render node where the VM has a GPU.
    Given {
        options: &'a Options,
        hold: bool,
        gpu: bool,
    },
    /// A warm VM's: one request from the daemon (warm.rs); with `true`, for a run its
    /// microVM was restored from a save of (D176).
    #[cfg(unix)]
    Warm(crate::warm::Link, bool),
}

/// The setup entry that gives a GPU VM's workload its render node, as
/// `--device /dev/dri/renderD128` gives it (setup.rs `devices`): `-`, not privileged, then
/// the device, its path in the workload and its access (D144).
#[cfg(unix)]
const RENDER_NODE: &[u8] = b"devices=-\0d\0/dev/dri/renderD128\0/dev/dri/renderD128\0rwm\0";

/// A warm VM's restore: `vm restore DIR --warm FD`, or a run's, `--resume FD` (D176).
#[cfg(unix)]
fn warm_restore(mut cfg: RestoreConfig, fd: i32, resume: bool) -> ExitCode {
    let daemon = match crate::warm::Link::new(fd) {
        Ok(link) => link,
        Err(e) => {
            report(e);
            return ExitCode::FAILURE;
        }
    };
    cfg.console = Console::Discard;
    serve_workload(None, Source::Warm(daemon, resume), move |vsock| {
        cfg.vsock = Some(vsock);
        restore_vm(cfg)
    })
}

#[cfg(not(unix))]
fn warm_restore(_: RestoreConfig, _: i32, _: bool) -> ExitCode {
    report("warm VMs need Unix sockets, which shards does not support on this platform yet");
    ExitCode::from(125)
}

/// A warm VM's boot: `vm run --rootfs IMAGE [--snapshot-dir DIR] --warm FD`.
#[cfg(unix)]
fn warm_boot(mut cfg: Config, rootfs: PathBuf, fd: i32) -> ExitCode {
    let daemon = match crate::warm::Link::new(fd) {
        Ok(link) => link,
        Err(e) => {
            report(e);
            return ExitCode::FAILURE;
        }
    };
    let template = cfg.snapshot.is_some();
    boot_into(&mut cfg, rootfs, template);
    cfg.console = Console::Discard;
    crate::warm::HOST_DEVICES.store(!cfg.vfio.is_empty(), std::sync::atomic::Ordering::Relaxed);
    serve_workload(None, Source::Warm(daemon, false), move |vsock| {
        cfg.vsock = Some(vsock);
        start(&cfg)
    })
}

#[cfg(not(unix))]
fn warm_boot(_: Config, _: PathBuf, _: i32) -> ExitCode {
    report("warm VMs need Unix sockets, which shards does not support on this platform yet");
    ExitCode::from(125)
}

/// Runs a workload in the VM `start` starts, whose vsock device's host side it gives:
/// `given`, if the command line gave one, and the run's ports, which this process serves.
/// Exits as the workload does.
#[cfg(unix)]
fn serve_workload(
    given: Option<VsockHost>,
    source: Source<'_>,
    start: impl FnOnce(VsockHost) -> Result<(Handle, Running), String>,
) -> ExitCode {
    use crate::spec::NOT_RUN;
    use crate::workload::{self, Request};
    let failed = |e: String| {
        report(e);
        ExitCode::from(NOT_RUN)
    };
    /// The command, resolved before anything starts.
    enum Command {
        Given {
            spec: Box<shards_abi::run::Spec>,
            interactive: bool,
            hold: bool,
        },
        Warm(crate::warm::Link, bool),
    }
    let command = match source {
        Source::Given { options, hold, gpu } => {
            match crate::spec::spec(options, |name| std::env::var_os(name)) {
                Ok(mut spec) => {
                    if gpu {
                        spec.setup.push(RENDER_NODE.to_vec());
                    }
                    Command::Given {
                        spec: Box::new(spec),
                        interactive: options.interactive,
                        hold,
                    }
                }
                Err(e) => return failed(e),
            }
        }
        Source::Warm(daemon, resumed) => Command::Warm(daemon, resumed),
    };
    {
        let mut vsock = given.unwrap_or_default();
        let (run_port, listener) = std::sync::mpsc::channel();
        let (signal_port, signals) = std::sync::mpsc::channel();
        // A run restored from a save of it has no ports of its own: its guest dials the
        // exec port again, by the save's tokens, where its workload, running already, could
        // dial a fresh port first (D176).
        if !matches!(command, Command::Warm(_, true)) {
            vsock.ports.push((shards_abi::run::PORT, run_port));
            vsock.ports.push((shards_abi::run::SIGNAL_PORT, signal_port));
        }
        let (exec_port, execs) = std::sync::mpsc::channel();
        vsock.every.push((shards_abi::run::EXEC_PORT, exec_port));
        // A warm VM's run may have its own engine (D181), its daemon's to serve: its port's
        // connections wait for the run, which takes them or lets the port go.
        let engines = matches!(command, Command::Warm(..)).then(|| {
            let (engine_port, engines) = std::sync::mpsc::channel();
            vsock.every.push((shards_abi::run::ENGINE_PORT, engine_port));
            engines
        });
        // A warm VM's signals come from its client. The command line's are this process's:
        // blocked before the VM's threads start, so that they inherit the mask.
        // One workload a process (M34): what its threads share lives as long as they do.
        static TO_GUEST: workload::ToGuest = workload::ToGuest::new(workload::Signals::new());
        static TIMING: workload::Timing = workload::Timing::new();
        let to_guest = &TO_GUEST;
        let warm = matches!(command, Command::Warm(..));
        if let Command::Given { interactive, .. } = &command {
            // SAFETY: isatty(3) on this process's stdin.
            let reads_terminal = *interactive && unsafe { libc::isatty(0) } == 1;
            if let Err(e) = workload::forward_signals(to_guest, reads_terminal) {
                return failed(e);
            }
        }
        // After the signals are blocked, which it inherits.
        if let Err(e) = std::thread::Builder::new()
            .name("execs".into())
            .spawn(move || workload::accept_execs(execs))
        {
            return failed(format!("the exec port's thread: {e}"));
        }
        let (handle, running) = match start(vsock) {
            Ok(started) => started,
            Err(e) => return failed(e),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let timing = &TIMING;
        let _ = RUNNING.set(handle.clone());
        let stopper = handle.clone();
        let served = std::thread::Builder::new()
            .name("workload".into())
            .spawn(move || {
                let guest_abi = || stopper.guest_abi();
                // How the workload ended, and whether its client has been told.
                let outcome = match command {
                    Command::Given {
                        spec,
                        interactive,
                        hold,
                    } => {
                        // Held, the guest is connected and waiting: the request is a line
                        // on stdin. A snapshot this VM saves is committed, and out of its
                        // reach, before the command goes to the guest.
                        let gate = || {
                            stopper.wait_for_snapshot();
                            seal()?;
                            moved()?;
                            if hold {
                                let _ = writeln!(std::io::stderr(), "shards-ready");
                                let _ = std::io::stdin().read_line(&mut String::new());
                            }
                            Ok(workload::Asked {
                                spec: (*spec).clone(),
                                interactive,
                                detached: false,
                                log: None,
                                started: None,
                                layer_in: None,
                                layer_out: None,
                                told: None,
                                saved: None,
                                kept: None,
                                processes: None,
                                host_net: None,
                                resumed: None,
                            })
                        };
                        let request = if hold || SEAL.get().is_some() {
                            Request::Later(&gate)
                        } else {
                            Request::Now {
                                spec: spec.clone(),
                                interactive,
                            }
                        };
                        (
                            (
                                workload::serve(Some((&listener, signals)), request, to_guest, timing, &guest_abi),
                                false,
                            ),
                            None,
                        )
                    }
                    Command::Warm(link, resumed) => {
                        let client: std::sync::OnceLock<Option<std::os::unix::net::UnixStream>> =
                            std::sync::OnceLock::new();
                        let engines = std::cell::Cell::new(engines);
                        let started = || {
                            crate::warm::started(&link);
                            end_recording_in(&stopper, RECORD_FOR);
                        };
                        // The daemon and the client hear how the command ended as soon as it
                        // has, before its layer is saved (D37): once, here or after.
                        let finished = std::sync::atomic::AtomicBool::new(false);
                        let finish = |served: &Result<workload::Ended, String>| {
                            if finished.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                return;
                            }
                            let Some(connection) = client.get() else {
                                return;
                            };
                            let timing = timing
                                .asked
                                .load(std::sync::atomic::Ordering::Relaxed)
                                .then(|| timing_json(&stopper, Some(timing)));
                            // What the run touched goes to the daemon with its end, for the
                            // template it was restored from, which no VM writes (D30). A
                            // failure costs later runs their prefetch, and nothing else.
                            let working_set = stopper.take_working_set().unwrap_or_else(|e| {
                                shards_vmm::debug!("{e}");
                                None
                            });
                            crate::warm::finish(
                                &link,
                                connection.as_ref(),
                                served,
                                timing.as_deref(),
                                working_set.as_ref(),
                            );
                        };
                        let saved = |used| crate::warm::layer_saved(&link, used);
                        // A layer that stays in its file (D190) is the daemon's to read
                        // once the machine is gone, nothing of it writing the file then:
                        // whether this run's may stay, and whether its guest kept it.
                        let layer_stays = std::sync::atomic::AtomicBool::new(false);
                        let layer_kept = std::sync::atomic::AtomicBool::new(false);
                        let kept = |used| match crate::warm::hold_kept(&link, used) {
                            Ok(()) => layer_kept.store(true, std::sync::atomic::Ordering::Relaxed),
                            Err(e) => shards_vmm::debug!("holding the kept layer's word: {e}"),
                        };
                        let processes = |records: &[u8]| crate::warm::processes(&link, records);
                        let host_net = |frame: &[u8]| crate::warm::host_net(&link, frame);
                        let ask = || {
                            // A template this VM saved is in place before the daemon hears the
                            // VM is ready, and settles it: its commit runs as the guest does.
                            // Its directory is given up before the run comes.
                            // Its directory is given up before the daemon hears it is ready,
                            // and out of its reach before it takes the run. What fails is
                            // said here, in its daemon's log, before its link closes.
                            stopper.wait_for_snapshot();
                            seal().inspect_err(|e| report(e))?;
                            let request = crate::warm::receive(
                                &link,
                                to_guest,
                                &|| moved().inspect_err(|e| report(e)),
                                resumed,
                            )?;
                            layer_stays.store(
                                request.layer_out.is_some() && crate::vm_run::layered(),
                                std::sync::atomic::Ordering::Relaxed,
                            );
                            // Its own engine (D181), before its guest hears of the run, where
                            // it has one; where not, its port is let go, and refuses. A resumed
                            // run's token comes after its save's (D176): its guest's relay says
                            // the one it said before.
                            let (tokens, engine) = match &request.tokens {
                                Some(t) => {
                                    let (run, engine) = t.split_at_checked(2 * shards_abi::run::TOKEN).unwrap_or((t, &[]));
                                    (Some(run.to_vec()), <[u8; shards_abi::run::TOKEN]>::try_from(engine).ok())
                                }
                                None => (None, crate::warm::engine_token(&request.spec)),
                            };
                            if let (Some(engines), Some(token)) = (engines.take(), engine) {
                                crate::warm::serve_engine(&link, token, engines).inspect_err(|e| report(e))?;
                            }
                            // The run's own most memory to plug, before its guest hears of
                            // it (D148), none included: the template's is the host's. A VM
                            // with none to plug has what it booted with.
                            if let Err(e) = stopper.set_plug_most(request.plug) {
                                shards_vmm::debug!("{e}");
                            }
                            // The CPUs its vCPUs are held to (D191), before its guest
                            // hears of the run: what its workload may use whatever its
                            // guest does.
                            stopper.hold_cpus(request.cpus).inspect_err(|e| report(e))?;
                            // Its I/O limits, before its guest hears of the run (D178).
                            IO.set(request.io.0, request.io.1);
                            // Its swap (D159), mapped into the guest's swap window before
                            // its guest hears of the run, which init swaps to before the
                            // command starts.
                            // A run restored from a save of it maps back a copy of the swap its
                            // save kept, which its guest swaps to already (D176).
                            let mut spec = request.spec;
                            match request.swap {
                                Some(file) if resumed => stopper
                                    .restore_swap(file)
                                    .map_err(|e| format!("its swap: {e}"))
                                    .inspect_err(|e| report(e))?,
                                Some(file) => {
                                    if let Some(len) = stopper
                                        .attach_swap(file)
                                        .map_err(|e| format!("its swap: {e}"))
                                        .inspect_err(|e| report(e))?
                                    {
                                        spec.setup.push(format!("swap={len}").into_bytes());
                                    }
                                }
                                None => {}
                            }
                            // And its connections, awaited before its guest runs, which dials
                            // them again as it resumes, by its save's tokens; in a guest whose
                            // init speaks this shards' protocol, as a run is handed only to
                            // such a guest.
                            let conns = match &tokens {
                                Some(tokens) => {
                                    let conns = workload::await_resumed(tokens).inspect_err(|e| report(e))?;
                                    let abi = stopper.guest_abi();
                                    if abi != Some(shards_abi::IDENTITY) {
                                        let spoken = abi.map_or_else(|| "none".to_string(), |abi| format!("{abi:016x}"));
                                        return Err(format!(
                                            "the saved microVM's shards-init speaks another shards' protocol ({spoken}, \
                                             where this shards speaks {:016x}): restore it with the shards that saved it",
                                            shards_abi::IDENTITY
                                        ));
                                    }
                                    stopper.release();
                                    Some(conns)
                                }
                                None => None,
                            };
                            timing
                                .asked
                                .store(request.timing, std::sync::atomic::Ordering::Relaxed);
                            let _ = client.set(request.client);
                            Ok(workload::Asked {
                                spec,
                                interactive: request.interactive,
                                detached: request.detached,
                                log: request.log,
                                started: Some(&started),
                                layer_in: request.layer_in,
                                layer_out: request.layer_out,
                                told: Some(&finish),
                                saved: Some(&saved),
                                kept: Some(&kept),
                                processes: Some(&processes),
                                host_net: Some(&host_net),
                                resumed: conns,
                            })
                        };
                        let ports = (!resumed).then_some((&listener, signals));
                        let served = workload::serve(
                            ports,
                            Request::Later(&ask),
                            to_guest,
                            timing,
                            &guest_abi,
                        );
                        // Some once the request came: its client, if it has one. A run that
                        // failed before its end was told is told now.
                        let outcome = match client.get() {
                            Some(_) => {
                                finish(&served);
                                (served, true)
                            }
                            // The daemon went without a request: nobody will ever send one.
                            None => {
                                stopper.stop();
                                (served, false)
                            }
                        };
                        // Where its layer may stay in its file, the daemon hears the VM has
                        // gone, its link closing, only once the machine is down and its layer
                        // let go of: the link goes with the outcome to the thread that waits
                        // for that. Its layer kept, the guest has written all it had (init's
                        // KEEP syncs every file system): the machine stops now, rather than
                        // as its guest powers off, and that thread tells the daemon as soon
                        // as nothing of it writes the layer (`warm::tell_kept`).
                        if layer_stays.into_inner() {
                            if layer_kept.into_inner() {
                                stopper.stop();
                            }
                            (outcome, Some(link))
                        } else {
                            (outcome, None)
                        }
                    }
                };
                let _ = tx.send(outcome);
            });
        if let Err(e) = served {
            handle.stop();
            return failed(format!("workload thread: {e}"));
        }
        let reason = running.wait_then(crate::warm::tell_kept);
        // Its joiners (D119), which the guest ended with its workload, tell the daemon how
        // each ended before this process goes, as the relay has had 5 s to (below).
        #[cfg(unix)]
        crate::warm::await_joiners(std::time::Duration::from_secs(5));
        // A warm VM's client printed its timing line from the exit status.
        if !warm {
            report_timing(&handle, Some(timing));
        }
        if let ExitReason::Error(e) = &reason {
            report(e);
        }
        // The guest waits for its status to be read before it powers off, so the relay
        // has finished unless the guest never ran the command.
        // A warm VM's link to its daemon, where it kept its layer, closes as this returns:
        // after the machine is down (D190).
        match rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .map(|(outcome, _link)| outcome)
        {
            // A warm VM's client has its status, and any error on its stderr: the warm
            // VM's own status says that it served.
            Ok((_, true)) => ExitCode::SUCCESS,
            Ok((Ok(ended), false)) => match &ended.not_run {
                // As `docker run` would say it, and exit.
                Some(why) => {
                    let (said, _) = shards_cmdline::commands::start_failed(why);
                    let (text, status) = crate::spec::not_run(&said);
                    let _ = writeln!(std::io::stderr(), "{text}");
                    ExitCode::from(status)
                }
                None => ExitCode::from(ended.status),
            },
            Ok((Err(e), false)) => failed(e),
            Err(_) => failed("the guest stopped without running the command".into()),
        }
    }
}

#[cfg(not(unix))]
fn serve_workload(
    _: Option<VsockHost>,
    _: Source<'_>,
    _: impl FnOnce(VsockHost) -> Result<(Handle, Running), String>,
) -> ExitCode {
    report("running a command in a microVM needs vsock, which shards does not support on this platform yet");
    ExitCode::from(125)
}

/// How long a VM records its working set after its command starts, at most, unless the
/// command answers first: a command still running then has long passed its start. The
/// whole way from the request to the start is recorded, however long a host takes over
/// it (a nested KVM host took 28 ms, PM M33).
/// - HVF: 50 ms. Recording slows the guest about sixfold (a pooled `true` took 9.9 ms
///   against 1.5, PM M30), so this is some 8 ms of the command's own time, past the 5 ms
///   a run should take.
/// - KVM: 1 s. Recording costs nothing, and a nested host took 125 ms to run `true` (PM
///   M33).
#[cfg(unix)]
const RECORD_FOR: std::time::Duration = if shards_vmm::vm::RESTORES_RECORD {
    std::time::Duration::from_secs(1)
} else {
    std::time::Duration::from_millis(50)
};

/// Saves the working set `after` from now, if the VM still records it then.
#[cfg(unix)]
fn end_recording_in(handle: &Handle, after: std::time::Duration) {
    if !handle.recording() {
        return;
    }
    let handle = handle.clone();
    let spawned = std::thread::Builder::new()
        .name("working-set".into())
        .spawn(move || {
            std::thread::sleep(after);
            if let Err(e) = handle.end_recording() {
                shards_vmm::debug!("{e}");
            }
        });
    if let Err(e) = spawned {
        shards_vmm::debug!("the working set's timer: {e}");
    }
}

/// The workload's request and answer times, where there is a workload.
#[cfg(unix)]
type WorkloadTiming = crate::workload::Timing;
#[cfg(not(unix))]
type WorkloadTiming = ();

/// With `SHARDS_TIMING` set: one machine-readable line on stderr, for benchmark harnesses.
fn report_timing(handle: &Handle, workload: Option<&WorkloadTiming>) {
    if std::env::var_os("SHARDS_TIMING").is_none() {
        return;
    }
    let _ = writeln!(
        std::io::stderr(),
        "shards-timing {}",
        timing_json(handle, workload)
    );
}

/// The timing line's fields, as JSON: the VM's clock readings in microseconds since the
/// process started, 0 for any not yet taken; the process's peak RSS so far and its own
/// memory now, in KiB; what it holds of the memory to plug (D133), in KiB; and what its
/// guest has reported free (D151), in KiB.
fn timing_json(handle: &Handle, workload: Option<&WorkloadTiming>) -> String {
    let markers: Vec<String> = handle
        .markers()
        .iter()
        .map(|(m, t)| format!("[{m},{t}]"))
        .collect();
    #[cfg(unix)]
    let (request, answered) = workload.map_or((0, 0), |t| {
        (
            t.request_us.get().copied().unwrap_or(0),
            t.answered_us.get().copied().unwrap_or(0),
        )
    });
    #[cfg(not(unix))]
    let (request, answered) = {
        let _ = workload;
        (0, 0)
    };
    format!(
        "{{\"released_us\":{},\"entry_us\":{},\"exit_us\":{},\"request_us\":{request},\"answered_us\":{answered},\"prefetched\":{},\"rss_kib\":{},\"footprint_kib\":{},\"hotplug_held_kib\":{},\"reported_kib\":{},\"markers\":[{}]}}",
        handle.released_at_us().unwrap_or(0),
        handle.entered_at_us().unwrap_or(0),
        handle.exited_at_us().unwrap_or(0),
        handle.prefetched(),
        max_rss_kib(),
        footprint_kib(),
        handle.hotplug_held().unwrap_or(0) / 1024,
        handle.reported() / 1024,
        markers.join(",")
    )
}

/// This process's own memory now, in KiB, as memory per VM is counted; 0 where unknown.
fn footprint_kib() -> u64 {
    #[cfg(unix)]
    {
        shards_ipc::footprint_kib()
    }
    #[cfg(not(unix))]
    0
}

/// This process's peak resident set so far, in KiB; 0 where unknown.
fn max_rss_kib() -> u64 {
    #[cfg(unix)]
    {
        shards_ipc::peak_rss_kib()
    }
    #[cfg(not(unix))]
    0
}

/// Starts the VM `cfg` describes, once this process is confined to the files it names, the
/// snapshot directory it saves to and its vsock socket: on Linux by Landlock, on macOS by
/// what its spawner grants it (D30). A warm VM's container log comes open from its daemon.
fn start(cfg: &Config) -> Result<(Handle, Running), String> {
    // Whole paths: grants and Landlock's rules name each file whole, a VM on macOS runs in
    // its sandbox's container rather than the caller's directory, and a snapshot records
    // its files by absolute path anyway.
    let mut cfg = cfg.clone();
    for path in [Some(&mut cfg.kernel), cfg.initrd.as_mut(), cfg.init.as_mut()]
        .into_iter()
        .flatten()
        .chain(cfg.pmem.iter_mut())
        .chain(cfg.layer.as_mut())
        .chain(cfg.disks.iter_mut().map(|d| &mut d.path))
        .chain(cfg.snapshot.as_mut().map(|p| &mut p.dir))
        .chain(cfg.vsock.as_mut().and_then(|v| v.path.as_mut()))
    {
        *path = absolute(path)?;
    }
    let cfg = &cfg;
    let mut paths = crate::confine::Paths::default();
    paths.read.push(cfg.kernel.clone());
    paths.read.extend(cfg.initrd.iter().cloned());
    paths.read.extend(cfg.init.iter().cloned());
    paths.read.extend(cfg.pmem.iter().cloned());
    // A function passed through (D154): VFIO's container and its group, or iommufd and its
    // device node.
    let (cdevs, groups): (Vec<&vm::Vfio>, Vec<&vm::Vfio>) = cfg.vfio.iter().partition(|v| v.cdev.is_some());
    if !groups.is_empty() {
        paths.devices.push(PathBuf::from("/dev/vfio/vfio"));
        paths.devices.extend(groups.iter().map(|v| v.group.clone()));
    }
    if !cdevs.is_empty() {
        paths.devices.push(PathBuf::from("/dev/iommu"));
        paths.devices.extend(cdevs.iter().filter_map(|v| v.cdev.clone()));
    }
    // Its writable layer, which its guest writes (D190).
    paths.write.extend(cfg.layer.iter().cloned());
    for disk in &cfg.disks {
        let list = if disk.read_only {
            &mut paths.read
        } else {
            &mut paths.write
        };
        list.push(disk.path.clone());
    }
    written(
        &mut paths,
        cfg.snapshot.as_ref().map(|p| p.dir.as_path()),
        cfg.vsock.as_ref().and_then(|v| v.path.as_deref()),
    );
    #[cfg(unix)]
    sealing(&paths, cfg.snapshot.as_ref().map(|p| p.dir.as_path()));
    // A host that runs no VM says so, before its files are confined: the check reads no
    // input of the VM's.
    vm::check_host()?;
    if cfg.snapshot.is_some() {
        snapshots()?;
    }
    confine(&paths)?;
    vm::start(cfg)
}

/// A host whose microVMs take no snapshots says so before a VM starts that would take or
/// restore one (D140).
fn snapshots() -> Result<(), String> {
    if vm::SNAPSHOTS {
        return Ok(());
    }
    Err("this host's microVMs take no snapshots yet".into())
}

/// [`start`] for a restore: the snapshot's directory, and the files it restores against.
fn restore_vm(mut cfg: RestoreConfig) -> Result<(Handle, Running), String> {
    for path in std::iter::once(&mut cfg.dir)
        .chain(cfg.snapshot.as_mut().map(|p| &mut p.dir))
        .chain(cfg.vsock.as_mut().and_then(|v| v.path.as_mut()))
        .chain(cfg.layer.as_mut())
    {
        *path = absolute(path)?;
    }
    // A host that runs no VM says so, before its files are confined: the check reads no
    // input of the VM's.
    vm::check_host()?;
    snapshots()?;
    let mut paths = crate::confine::Paths::default();
    // The snapshot says which files it restores against: on macOS it is granted first, to
    // be read for them.
    #[cfg(target_os = "macos")]
    granted_template(&cfg.dir)?;
    #[cfg(not(target_os = "macos"))]
    paths.read_under.push(cfg.dir.clone());
    written(
        &mut paths,
        cfg.snapshot.as_ref().map(|p| p.dir.as_path()),
        cfg.vsock.as_ref().and_then(|v| v.path.as_deref()),
    );
    #[cfg(unix)]
    sealing(&paths, cfg.snapshot.as_ref().map(|p| p.dir.as_path()));
    // Its writable layer, which its guest writes (D190).
    paths.write.extend(cfg.layer.iter().cloned());
    let add = |paths: &mut crate::confine::Paths, files: &[(PathBuf, bool)]| {
        for (path, read_only) in files {
            let list = if *read_only {
                &mut paths.read
            } else {
                &mut paths.write
            };
            list.push(path.clone());
        }
    };
    // A spawner that says which files the template restores against gives those alone,
    // and this process is confined to them before it reads the template's state, which a
    // VM process wrote and may name anything: what it names past them is refused, and a
    // state made to mislead its reader is read where nothing else can be reached. Without
    // a spawner's word, the state names them, and is read first.
    // The state is read once, and the generation checked is the one resumed (review
    // 1.12).
    let pinned = match BACKING.get() {
        Some(given) => {
            add(&mut paths, given);
            confine(&paths)?;
            let pinned = shards_vmm::snapshot::read(&cfg.dir)?;
            let backing = shards_vmm::snapshot::backing_of(&pinned);
            // Each given file as its snapshot named the files it was taken with: resolved,
            // links and all (vmm platform::input_path), so that one given through a link,
            // macOS's /var to /private/var among them, is the file it names, not another.
            let given: Vec<(PathBuf, bool)> = given
                .iter()
                .map(|(path, read_only)| {
                    let real = shards_vmm::platform::input_path(path).unwrap_or_else(|_| path.clone());
                    (real, *read_only)
                })
                .collect();
            let unasked: Vec<String> = backing
                .iter()
                .filter(|b| !given.contains(b))
                .map(|(path, read_only)| format!("{}{}", path.display(), if *read_only { ":ro" } else { "" }))
                .collect();
            if !unasked.is_empty() {
                return Err(format!(
                    "{}: the template names files it was not given: {}",
                    cfg.dir.display(),
                    unasked.join(", ")
                ));
            }
            pinned
        }
        None => {
            let pinned = shards_vmm::snapshot::read(&cfg.dir)?;
            add(&mut paths, &shards_vmm::snapshot::backing_of(&pinned));
            confine(&paths)?;
            pinned
        }
    };
    // A slot for each of its shared directories, which its run fills (D38), and its own
    // join disk where the snapshot has one (D119).
    #[cfg(unix)]
    {
        cfg.shares = (0..pinned.snapshot.config.shares)
            .map(|_| std::sync::Arc::default())
            .collect();
        let _ = SHARES.set(cfg.shares.clone());
    }
    if pinned.snapshot.config.join {
        cfg.join = Some(give_join_disk());
    }
    #[cfg(unix)]
    if pinned.snapshot.config.join_share {
        cfg.join_share = Some(give_join_share());
    }
    vm::restore_from(&cfg, pinned)
}

/// `path` made whole against the working directory, and normalized as
/// `std::path::absolute` normalizes it (on Windows, its separators made `\`).
fn absolute(path: &Path) -> Result<PathBuf, String> {
    let whole = match CWD.get() {
        Some(cwd) if path.is_relative() => cwd.join(path),
        _ => path.to_path_buf(),
    };
    std::path::absolute(&whole).map_err(|e| format!("{}: {e}", path.display()))
}

/// The files a restore's spawner gives it to restore against (`--backing PATH[:ro]`): a
/// template naming another is refused.
static BACKING: std::sync::OnceLock<Vec<(PathBuf, bool)>> = std::sync::OnceLock::new();

/// The directory relative paths are of (`--cwd DIR`): the VM's starter's own on macOS, where App
/// Sandbox starts a VM process in its container instead.
static CWD: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// What a VM that saves a snapshot gives up once it is committed (review 8.2): the
/// snapshot's directory, which only the save writes. A template's saver goes on to serve a
/// run, and a guest that took its process over could otherwise rewrite what every later
/// run of the image restores.
#[cfg(unix)]
struct Seal {
    /// The directory given up.
    dir: PathBuf,
    /// All else the process keeps: on Linux, applied as a Landlock layer of its own, which
    /// leaves the first's rules but the directory's.
    #[cfg(target_os = "linux")]
    kept: crate::confine::Paths,
}

#[cfg(unix)]
static SEAL: std::sync::OnceLock<Seal> = std::sync::OnceLock::new();

/// Records what [`seal`] gives up once a snapshot `paths` lets this VM save is committed.
#[cfg(unix)]
fn sealing(paths: &crate::confine::Paths, snapshot: Option<&Path>) {
    let Some(dir) = snapshot else { return };
    #[cfg(target_os = "linux")]
    let kept = {
        let mut kept = paths.clone();
        kept.write_under.retain(|d| d != dir);
        kept
    };
    #[cfg(not(target_os = "linux"))]
    let _ = paths;
    let _ = SEAL.set(Seal {
        dir: dir.to_path_buf(),
        #[cfg(target_os = "linux")]
        kept,
    });
}

/// Gives up the directory of the snapshot this VM saved, committed by now, before its
/// spawner hears it is ready, and so before anything of a run's workload runs in its
/// guest. On Linux, a Landlock layer without it, checked: making a file there must be
/// refused. Nothing of the snapshot is open by then (vmm snapshot::Staged::commit closes
/// its files), and Landlock takes back nothing open. On macOS nothing can be given up:
/// App Sandbox's grants last the process's life (PM M102); [`moved`] holds instead. A VM
/// that saved no snapshot has nothing to give up.
#[cfg(unix)]
fn seal() -> Result<(), String> {
    let Some(seal) = SEAL.get() else {
        return Ok(());
    };
    #[cfg(target_os = "linux")]
    {
        crate::confine::landlock(&seal.kept)?;
        let probe = seal.dir.join(format!(".sealed-{}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
            Err(e) => return Err(format!("{}: checking it is given up: {e}", seal.dir.display())),
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                return Err(format!(
                    "{}: still writable once its snapshot was committed",
                    seal.dir.display()
                ));
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = seal;
    Ok(())
}

/// Checks, once a run has come and before this VM takes it, that the snapshot it saved is
/// out of its reach: on macOS, where App Sandbox's grants name paths and cannot be given
/// up (PM M102), by being no longer at the path it was granted, as the daemon moves a
/// template into place before it hands over a run. A VM that would reach it takes no run,
/// and its spawner gives the run to another (daemon.rs, `hand_over`).
#[cfg(unix)]
fn moved() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if let Some(seal) = SEAL.get()
        && shards_vmm::snapshot::exists(&seal.dir)
    {
        return Err(format!(
            "{}: the snapshot this VM saved is still where it was granted, and a run here would reach it",
            seal.dir.display()
        ));
    }
    Ok(())
}

/// The directory a VM writes in, the snapshot it saves, and the one holding the vsock
/// socket path it was given, if any.
fn written(paths: &mut crate::confine::Paths, snapshot: Option<&Path>, vsock: Option<&Path>) {
    paths.write_under.extend(snapshot.map(Path::to_path_buf));
    // Landlock holds a directory by its inode, so the one a snapshot goes to is made now,
    // as its snapshot would make it (confine.rs, `landlock`). On macOS the spawner makes it
    // as it grants it: a sandboxed process makes nothing outside its grants.
    #[cfg(target_os = "linux")]
    if let Some(dir) = snapshot {
        let _ = std::fs::create_dir_all(dir);
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        paths.vsock = vsock.map(Path::to_path_buf);
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    paths
        .sockets_under
        .extend(vsock.and_then(Path::parent).map(Path::to_path_buf));
}

/// The socket this VM asks its spawner for access on (`--grants FD`): one a process, as
/// its VM is.
#[cfg(target_os = "macos")]
static GRANTS: std::sync::OnceLock<std::os::unix::net::UnixStream> = std::sync::OnceLock::new();

/// The pipes this VM asks its spawner for access on (`--grants HANDLE`): taken, and so
/// closed, once it has all it needs, unless its guest's connections to host ports go
/// through them.
#[cfg(windows)]
static GRANTS: std::sync::Mutex<Option<crate::grant::Link>> = std::sync::Mutex::new(None);

/// What else its spawner handed this process (`--grants HANDLE`), by name, until taken:
/// its network's ring and doorbells (`--net`).
#[cfg(windows)]
static GIVEN: std::sync::Mutex<Vec<(String, shards_ipc::Handed)>> = std::sync::Mutex::new(Vec::new());

/// `--net REGION,WAKE_ME,WAKE_PEER,MAC`: the frame ring and doorbells its spawner left this
/// process at those descriptors, which it owns from here on, and the guest's MAC.
#[cfg(unix)]
fn net_host(spec: &str) -> Result<shards_vmm::devices::virtio::net::NetHost, String> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let parts: Vec<&str> = spec.split(',').collect();
    let [region, me, peer, mac] = parts.as_slice() else {
        return Err(format!("--net {spec}: not REGION,WAKE_ME,WAKE_PEER,MAC"));
    };
    let mut seen = Vec::new();
    let mut adopt = |what: &str, v: &str| -> Result<std::sync::Arc<OwnedFd>, String> {
        let fd: i32 = v
            .parse()
            .map_err(|_| format!("--net: {what} {v:?} is not a descriptor"))?;
        // SAFETY: fcntl(2) asks whether the descriptor is open.
        if fd < 3 || seen.contains(&fd) || unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            return Err(format!("--net: {what} {fd} is not a descriptor of its own"));
        }
        seen.push(fd);
        // SAFETY: an open descriptor the spawner left for this process alone, owned from
        // here on, closed on exec.
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        // SAFETY: as above.
        Ok(std::sync::Arc::new(unsafe { OwnedFd::from_raw_fd(fd) }))
    };
    let region = adopt("REGION", region)?;
    let wake_me = adopt("WAKE_ME", me)?;
    let wake_peer = adopt("WAKE_PEER", peer)?;
    let mac = mac
        .parse::<shards_net::Mac>()
        .map_err(|e| format!("--net: {e}"))?
        .0;
    // Asked as the command ends, so that what it sent goes before the run does.
    let flush = shards_vmm::devices::virtio::net::TxFlush::default();
    crate::warm::adopt_flush(flush.clone());
    Ok(shards_vmm::devices::virtio::net::NetHost {
        region,
        wake_me,
        wake_peer,
        mac,
        flush,
    })
}

/// `--net MAC` (Windows): the frame ring and doorbells its spawner handed this process by
/// name (`net-region`, `net-wake-me`, `net-wake-peer`), which it owns from here on, as no
/// spawner can know a handle's value in a child before it hands it over; and the guest's
/// MAC. The doorbell it rings its network process by is an AF_UNIX socket, which that
/// process waits on with its sockets (PM M213); the one it sleeps on, an event.
#[cfg(windows)]
fn net_host(spec: &str) -> Result<shards_vmm::devices::virtio::net::NetHost, String> {
    use std::os::windows::io::{FromRawHandle as _, OwnedHandle};
    let mut given = GIVEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut take = |name: &str| -> Result<OwnedHandle, String> {
        let at = given
            .iter()
            .position(|(n, h)| n == name && matches!(h, shards_ipc::Handed::Handle(v) if *v != 0))
            .ok_or_else(|| format!("--net: no {name} was given"))?;
        let shards_ipc::Handed::Handle(value) = given.swap_remove(at).1 else {
            return Err(format!("--net: {name} is no handle"));
        };
        // SAFETY: a handle the spawner duplicated into this process for this alone, owned
        // from here on.
        Ok(unsafe { OwnedHandle::from_raw_handle(value as std::os::windows::io::RawHandle) })
    };
    let region = std::sync::Arc::new(take("net-region")?);
    let wake_me = std::sync::Arc::new(shards_netring::Bell::Event(take("net-wake-me")?));
    let wake_peer = {
        let at = given
            .iter()
            .position(|(n, h)| n == "net-wake-peer" && matches!(h, shards_ipc::Handed::Socket(_)))
            .ok_or("--net: no net-wake-peer socket was given")?;
        let shards_ipc::Handed::Socket(info) = given.swap_remove(at).1 else {
            return Err("--net: net-wake-peer is no socket".into());
        };
        let rings = shards_vmm::platform::UnixStream::from_shared(&info)
            .map_err(|e| format!("--net: net-wake-peer: {e}"))?;
        rings
            .set_nonblocking(true)
            .map_err(|e| format!("--net: net-wake-peer: {e}"))?;
        std::sync::Arc::new(shards_netring::Bell::Socket(rings))
    };
    let mac = spec
        .parse::<shards_net::Mac>()
        .map_err(|e| format!("--net: {e}"))?
        .0;
    let flush = shards_vmm::devices::virtio::net::TxFlush::default();
    Ok(shards_vmm::devices::virtio::net::NetHost {
        region,
        wake_me,
        wake_peer,
        mac,
        flush,
    })
}

/// Takes the socket at descriptor `fd` to ask the spawner for access on (macOS).
#[cfg(target_os = "macos")]
fn grants(fd: &str) -> Result<(), String> {
    let fd = fd
        .parse::<i32>()
        .map_err(|_| format!("--grants: {fd:?} is not a descriptor"))?;
    let link = crate::warm::inherited_socket("--grants", fd)?;
    GRANTS.set(link).map_err(|_| "--grants given twice".to_string())
}

/// Takes what the spawner gave this process (`--grants HANDLE`, shards_ipc's pipe): the
/// pipes to ask it for access on.
#[cfg(windows)]
fn grants(pipe: &str) -> Result<(), String> {
    use std::os::windows::io::FromRawHandle as _;
    let pipe = pipe
        .parse::<usize>()
        .map_err(|_| format!("--grants: {pipe:?} is not a handle"))?;
    let given = shards_ipc::given(pipe).map_err(|e| format!("--grants: {e}"))?;
    let handle = |name: &str| -> Result<std::fs::File, String> {
        let value = given
            .iter()
            .find_map(|(n, v)| match v {
                shards_ipc::Handed::Handle(v) if n == name && *v != 0 => Some(*v),
                _ => None,
            })
            .ok_or_else(|| format!("--grants: no {name} pipe was given"))?;
        // SAFETY: a pipe's handle the spawner duplicated into this process for it alone,
        // owned from here on.
        Ok(unsafe { std::fs::File::from_raw_handle(value as std::os::windows::io::RawHandle) })
    };
    let link = crate::grant::Link {
        from: handle("answer")?,
        to: handle("ask")?,
    };
    GIVEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .extend(given.into_iter().filter(|(n, _)| n != "ask" && n != "answer"));
    let mut grants = GRANTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if grants.is_some() {
        return Err("--grants given twice".into());
    }
    *grants = Some(link);
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn grants(_: &str) -> Result<(), String> {
    Err("--grants: macOS and Windows alone grant a VM its files".into())
}

/// On macOS, where the VM process is in App Sandbox from its launch, asks its spawner for
/// `wanted` (grant). It fails closed, as Landlock does: a VM process not signed into App
/// Sandbox starts no VM.
#[cfg(target_os = "macos")]
fn ask(wanted: &[crate::grant::Wanted]) -> Result<(), String> {
    if !crate::grant_ask::sandboxed() {
        return Err(
            "shards-vm is not signed into App Sandbox, which confines every VM on macOS: sign it \
             with resources/vm.entitlements"
                .into(),
        );
    }
    let link = GRANTS
        .get()
        .ok_or("nothing was granted to this VM: start it with `shards run vm --kernel` or `shards restore vm`, which grant it its files")?;
    crate::grant_ask::obtain(link, wanted)
}

/// On Windows, where the VM process is in an AppContainer from its start (D141), asks its
/// spawner for `wanted` (grant). It fails closed: a VM process outside one, started by
/// anything but `shards`, starts no VM.
#[cfg(windows)]
fn ask(wanted: &[crate::grant::Wanted]) -> Result<(), String> {
    if !shards_ipc::in_app_container() {
        return Err(
            "shards-vm is not in an AppContainer, which confines every VM on Windows: start it with \
             `shards run vm --kernel`, which confines it"
                .into(),
        );
    }
    let grants = GRANTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let link = grants.as_ref().ok_or(
        "nothing was granted to this VM: start it with `shards run vm --kernel`, which grants it its files",
    )?;
    crate::grant_ask::obtain(link, wanted)
}

/// `paths` as the VM asks for them: files it reads read-only, files it writes read-write,
/// and the directories it writes in made first. A directory cannot be granted read-only
/// (PM M70): a restore asks for its template's files instead.
#[cfg(any(target_os = "macos", windows))]
fn granted(paths: &crate::confine::Paths) -> Result<(), String> {
    use crate::grant::Access;
    if !paths.read_under.is_empty() {
        return Err("a directory cannot be granted read-only to a confined VM process".into());
    }
    if !paths.sockets_under.is_empty() {
        return Err("a directory of sockets cannot be granted to a confined VM process".into());
    }
    if !paths.devices.is_empty() {
        return Err("a host's device is passed through by VFIO, Linux's (D154)".into());
    }
    let wanted: Vec<crate::grant::Wanted> = paths
        .read
        .iter()
        .map(|p| (Access::Read, p.clone()))
        .chain(paths.write.iter().map(|p| (Access::Write, p.clone())))
        .chain(paths.write_under.iter().map(|p| (Access::MakeDir, p.clone())))
        .chain(paths.vsock.iter().map(|p| (Access::Listen, p.clone())))
        .collect();
    ask(&wanted)
}

/// Has the spawner dial, beside `vsock`, every host port the guest connects to: App
/// Sandbox lets this process dial nothing outside its container (PM M67).
#[cfg(target_os = "macos")]
fn dial_through_spawner(vsock: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt as _;
    static DIALING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let mut prefix = vsock.as_os_str().as_bytes().to_vec();
    prefix.push(b'_');
    shards_vmm::platform::set_dialer(Box::new(move |target: &Path| {
        let port = target
            .as_os_str()
            .as_bytes()
            .strip_prefix(prefix.as_slice())
            .and_then(|p| std::str::from_utf8(p).ok())
            .and_then(|p| p.parse::<u32>().ok())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a vsock host port"))?;
        let link = GRANTS.get().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotConnected, "no spawner to dial through")
        })?;
        // A request and its answer at a time.
        let _one = DIALING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::grant_ask::dial(link, port)
    }))
}

/// Has the spawner dial, beside `vsock`, every host port the guest connects to: an
/// AppContainer dials no socket of its user's (PM M201, M203).
#[cfg(windows)]
fn dial_through_spawner(vsock: &Path) -> Result<(), String> {
    let prefix = format!("{}_", vsock.to_str().ok_or("a vsock path that is not UTF-8")?);
    shards_vmm::platform::set_dialer(Box::new(move |target: &Path| {
        let port = target
            .to_str()
            .and_then(|t| t.strip_prefix(prefix.as_str()))
            .and_then(|p| p.parse::<u32>().ok())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a vsock host port"))?;
        // A request and its answer at a time: the lock is held through both.
        let grants = GRANTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let link = grants.as_ref().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotConnected, "no spawner to dial through")
        })?;
        crate::grant_ask::dial(link, port)
    }))
}

/// A restore's template, as a VM in App Sandbox is granted it: its pointer, then the
/// files of the generation it names, never its directory.
#[cfg(target_os = "macos")]
fn granted_template(dir: &Path) -> Result<(), String> {
    use crate::grant::Access;
    ask(&[(Access::ReadIfThere, shards_vmm::snapshot::pointer(dir))])?;
    let ([state, memory], [working_set, layer_head, layer]) = shards_vmm::snapshot::generation_files(dir)?;
    ask(&[
        (Access::Read, state),
        (Access::Read, memory),
        (Access::ReadIfThere, working_set),
        (Access::ReadIfThere, layer_head),
        (Access::ReadIfThere, layer),
    ])
}

/// Applies `paths` where the OS confines by path: App Sandbox's grants on macOS, Landlock
/// on Linux (D30), beside Linux's seccomp filter, which is on the whole process already.
fn confine(paths: &crate::confine::Paths) -> Result<(), String> {
    // The last it asks for: the link closes, and the spawner grants nothing more, unless
    // the guest's connections to host ports go through it.
    #[cfg(target_os = "macos")]
    return granted(paths).and_then(|()| match &paths.vsock {
        Some(vsock) => dial_through_spawner(vsock),
        None => {
            if let Some(link) = GRANTS.get() {
                shards_ipc::end(link);
            }
            Ok(())
        }
    });
    #[cfg(target_os = "linux")]
    return crate::confine::landlock(paths);
    // As on macOS; and once its partition is set up, before anything of its guest's is
    // read, this process forbids itself dynamic code (D141, PM M202).
    #[cfg(windows)]
    {
        #[cfg(target_arch = "x86_64")]
        shards_vmm::hv::forbid_dynamic_code_once_set_up();
        granted(paths)?;
        match &paths.vsock {
            Some(vsock) => dial_through_spawner(vsock),
            None => {
                drop(
                    GRANTS
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take(),
                );
                Ok(())
            }
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = paths;
        Ok(())
    }
}

/// Runs a started VM to its end: console, timing report, exit code. A template must end
/// in its snapshot.
fn supervise(started: Result<(Handle, Running), String>, console: Console, template: bool) -> ExitCode {
    let (handle, running) = match started {
        Ok(v) => v,
        Err(e) => {
            report(e);
            return ExitCode::FAILURE;
        }
    };
    let terminal = (console == Console::Stdout).then(RawTerminal::enable).flatten();
    forward_stdin(handle.clone());
    let reason = running.wait();
    drop(terminal);
    report_timing(&handle, None);
    match reason {
        ExitReason::Snapshotted => ExitCode::SUCCESS,
        ExitReason::PowerOff | ExitReason::Stopped if template => {
            report("the guest stopped before its template was saved");
            ExitCode::FAILURE
        }
        ExitReason::PowerOff | ExitReason::Stopped => ExitCode::SUCCESS,
        ExitReason::Reset => {
            report("guest requested a reset");
            ExitCode::from(3)
        }
        ExitReason::Error(e) => {
            report(e);
            ExitCode::FAILURE
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// `--pci-tests` takes places on bus 0 in hex, each once, each device with its
    /// function 0; anything else it refuses, saying why.
    #[test]
    fn test_functions_are_placed_as_asked() {
        assert_eq!(pci_tests("01.0,02.0,02.1"), Ok(vec![(1, 0), (2, 0), (2, 1)]));
        assert_eq!(pci_tests("1f.7,1f.0"), Ok(vec![(31, 7), (31, 0)]));
        for (bad, why) in [
            ("00.0", "its device is one of 01 to 1f"),
            ("20.0", "its device is one of 01 to 1f"),
            ("01.8", "its function is one of 0 to 7"),
            ("01", "is not SS.F"),
            ("01.0,01.0", "01.0 twice"),
            ("01.0,02.1", "device 02 has no function 0"),
            ("", "is not SS.F"),
        ] {
            let e = pci_tests(bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
    }
    use std::os::unix::ffi::OsStrExt as _;

    fn args(list: &[&[u8]]) -> impl Iterator<Item = OsString> {
        list.iter()
            .map(|a| std::ffi::OsStr::from_bytes(a).to_os_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// Paths reach a VM in whatever bytes the OS gave them, as a home on Linux may be
    /// named: the daemon passes its templates' and images' paths under it. Options that
    /// are text must be UTF-8.
    #[test]
    fn paths_are_taken_in_any_bytes() {
        let run = parse_run(args(&[
            b"--kernel",
            b"/h\xff/k",
            b"--init",
            b"/h\xff/i",
            b"--disk",
            b"/h\xff/d:ro",
            b"--disk",
            b"/h\xff/e",
            b"--rootfs",
            b"/h\xff/r",
            b"--snapshot-dir",
            b"/h\xff/t",
            b"--warm",
            b"3",
        ]))
        .unwrap();
        let bytes = |p: &std::path::Path| p.as_os_str().as_bytes().to_vec();
        assert_eq!(bytes(&run.cfg.kernel), b"/h\xff/k");
        assert_eq!(bytes(run.cfg.init.as_deref().unwrap()), b"/h\xff/i");
        let disks: Vec<(Vec<u8>, bool)> = run
            .cfg
            .disks
            .iter()
            .map(|d| (bytes(&d.path), d.read_only))
            .collect();
        assert_eq!(
            disks,
            [(b"/h\xff/d".to_vec(), true), (b"/h\xff/e".to_vec(), false)]
        );
        assert_eq!(bytes(&run.cfg.snapshot.unwrap().dir), b"/h\xff/t");
        let Mode::Warm { rootfs, fd: 3 } = run.mode else {
            panic!("not a warm VM");
        };
        assert_eq!(bytes(&rootfs), b"/h\xff/r");

        let restore = parse_restore(args(&[b"/h\xff/t", b"--vsock", b"/h\xff/v"])).unwrap();
        assert_eq!(bytes(&restore.cfg.dir), b"/h\xff/t");
        assert_eq!(
            bytes(
                restore
                    .cfg
                    .vsock
                    .as_ref()
                    .and_then(|v| v.path.as_deref())
                    .unwrap()
            ),
            b"/h\xff/v"
        );

        let e = parse_run(args(&[b"--kernel", b"/k", b"--cpus", b"\xff"]))
            .err()
            .unwrap();
        assert!(e.contains("not valid UTF-8"), "{e}");
        let e = parse_restore(args(&[b"/t", b"/\xff"])).err().unwrap();
        assert!(e.contains("unexpected argument"), "{e}");
    }

    /// --memory-max is the guest's memory in all, its order among the options no matter:
    /// what it may have plugged is the rest; less than --memory is refused, and so is a
    /// snapshot of memory that is plugged.
    #[test]
    fn memory_max_is_what_the_guest_may_have_in_all() {
        let run = parse_run(args(&[
            b"--memory-max",
            b"4096",
            b"--kernel",
            b"/k",
            b"--memory",
            b"512",
        ]))
        .unwrap();
        assert_eq!((run.cfg.memory_mib, run.cfg.hotplug_mib), (512, 3584));
        let run = parse_run(args(&[
            b"--kernel",
            b"/k",
            b"--memory",
            b"512",
            b"--memory-max",
            b"512",
        ]))
        .unwrap();
        assert_eq!(run.cfg.hotplug_mib, 0);
        let e = parse_run(args(&[
            b"--kernel",
            b"/k",
            b"--memory",
            b"512",
            b"--memory-max",
            b"256",
        ]))
        .err()
        .unwrap();
        assert_eq!(e, "--memory-max 256 is less than --memory 512");
        // A snapshot keeps the memory to plug, none of it plugged (D148).
        let run = parse_run(args(&[
            b"--kernel",
            b"/k",
            b"--memory-max",
            b"1024",
            b"--snapshot-dir",
            b"/t",
        ]))
        .unwrap();
        assert_eq!(run.cfg.hotplug_mib, 1024 - run.cfg.memory_mib);
        let e = parse_run(args(&[b"--kernel", b"/k", b"--memory-max", b"lots"]))
            .err()
            .unwrap();
        assert!(e.starts_with("--memory-max: "), "{e}");
    }
}
