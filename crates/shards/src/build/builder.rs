//! The builder VM a build's `RUN` steps run in (docs/design/architecture.md D34): one
//! microVM per build, booted with shards-init as a builder (`shards_build=1`), each base
//! image the build's steps stand on as a virtio-pmem device, and its build port
//! (shards_abi::build::PORT) reaching this process through the vsock muxer's socket
//! `<vsock>_<port>`. The VM process asks this process for what it may open, as it asks
//! the daemon for a run's (D30).
//!
//! The guest keeps layers this process names: each host step's changes are sent once,
//! when a `RUN` first stands on them, and each `RUN`'s own changes stay in the guest as
//! the layer its step named, so nothing goes over the wire twice.
//!
//! A step's caches (D134) never cross the wire: their layers' files are attached to the
//! builder's join disk, each once for the builder's life, through its VM process's
//! `--attach` socket, and a file for each one's changes too, which the guest writes.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use shards_abi::build::{self, Kept, Step, Tree, kind};
use shards_abi::run;
use shards_build::sync::{self, Scope};
use shards_build::upper::Applier;
use shards_build::vfs::Fs;
use shards_cmdline::buildflags::Agent;
use shards_image::erofs::Source;
use shards_vmm::platform::{AsRawSocket as _, UnixListener, UnixStream};

/// How long a builder may take to boot and dial back.
const BOOT_PATIENCE: Duration = Duration::from_secs(30);

/// The memory a builder boots with, in MiB, where it may have more: what its page
/// structures cost it at boot grows with it (PM M82), and the rest is plugged as the build
/// needs it, its init asking (D133, PM M193).
pub const BOOT_MEMORY_MIB: u64 = 512;

/// Where a snapshot's tree comes from, as a builder guest holds it.
#[derive(Debug)]
pub enum Origin {
    /// Nothing: scratch.
    Scratch,
    /// A base image's root filesystem: the builder's pmem device of this index.
    Image(u32),
    /// Step `step`, which made `fs` from `parent`: its own changes, a layer of their own,
    /// known to the builder by `id`.
    Step {
        id: u64,
        parent: Rc<Origin>,
        fs: Rc<Fs>,
        step: u32,
    },
    /// The whole of `fs`, a layer of its own known by `id`: what has no base the guest
    /// holds.
    Whole { id: u64, fs: Rc<Fs> },
    /// A `RUN` on `parent`, whose changes the guest keeps as `layer`.
    Run { parent: Rc<Origin>, layer: u32 },
    /// `parts` stacked in order, as a merge stacks them.
    Merge(Vec<Rc<Origin>>),
}

/// A fresh id for an origin that becomes a layer: never an address, which a dropped
/// origin's successor could take.
pub fn origin_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A step's output mounts' changes: each the layer it became and its changes' stream.
pub type Outputs = Vec<(u32, Vec<u8>)>;

/// What a step came to: how it ended, the layer its changes are in the guest, each output
/// mount's changes with its layer, and what its requests came to under the build's proxy
/// (D110).
pub type Ran = (Ended, u32, Outputs, Option<super::proxy::capture::Capture>);

/// How a step's process ended.
#[derive(Debug)]
#[cfg_attr(not(unix), allow(dead_code))]
pub enum Ended {
    /// Its status, as `docker run` reports one.
    Status(u32),
    /// It never ran, and why.
    NotRun(String),
}

/// A builder VM, and the layers it holds.
pub struct Builder {
    vm: shards_ipc::Child,
    /// Its network process, which ends with the VM.
    #[cfg(unix)]
    net: shards_ipc::Child,
    #[cfg(windows)]
    net: crate::netproc::NetProcess,
    /// Its VM's AppContainer (Windows, D141), whose profile goes once the VM has.
    #[cfg(windows)]
    _container: shards_ipc::AppContainer,
    conn: UnixStream,
    /// Where its steps' SSH agent sockets are relayed to (build::SSH_PORT).
    ssh: UnixListener,
    /// Where its vsock socket is: removed with the builder, after its VM.
    _dir: SocketDir,
    /// The layer each origin's own part is, once the guest has it, by the origin's id.
    layers: HashMap<u64, u32>,
    next: u32,
    /// The build's proxy's URL, as its steps reach it (the gateway's [`super::proxy::PORT`]),
    /// where its network is the proxy's alone (D110).
    proxy_url: Option<String>,
    /// Its VM process's `--attach` socket (D134).
    attach: UnixStream,
    /// The range of its join disk each cache layer's file was given, by the layer.
    attached: HashMap<String, (u64, u64)>,
}

/// A step's proxy, where it has one (D110): the build's, and what answers its requests'
/// questions on the build's thread.
pub type StepProxy<'a, 'p> = Option<(&'p super::proxy::Proxy, &'a mut dyn FnMut(&str, &str) -> bool)>;

/// A step's questions of the build's policies, and what answers them (D110).
type Asked<'q, 'a> = Option<(&'q super::proxy::Questions, &'a mut dyn FnMut(&str, &str) -> bool)>;

/// The build's proxy (D110), listening in a directory of its own as the builder's sockets
/// are, its files of request bodies leaving the disk what `limits` keeps free.
pub fn proxy(limits: &shards_image::store::Limits) -> Result<super::proxy::Proxy, String> {
    super::proxy::Proxy::new(socket_dir()?, limits)
}

impl std::fmt::Debug for Builder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder")
            .field("vm", &self.vm.id())
            .finish_non_exhaustive()
    }
}

/// What a builder boots: its kernel and init, its shape, and the base images it can stand
/// steps on, in pmem order.
#[derive(Debug)]
pub struct Boot<'a> {
    pub kernel: &'a Path,
    pub init: &'a Path,
    pub cpus: u32,
    /// The most memory the builder may have, in MiB: it boots with [`BOOT_MEMORY_MIB`] of
    /// it, and the rest is plugged as the build needs it (D133).
    pub memory_mib: u64,
    pub bases: &'a [PathBuf],
    /// The build's proxy (D110): where there is one, the builder's network reaches it
    /// alone.
    pub proxy: Option<&'a super::proxy::Proxy>,
    /// Whether it has this Mac's GPU, for steps given its render node (D145).
    pub gpu: bool,
    /// The emulators its steps' platforms need (D179): the emulator image's root
    /// filesystem, one more pmem device after the bases, and the OCI architectures it
    /// registers of it.
    pub emulators: Option<(&'a Path, &'a [&'static str])>,
}

/// A directory of a builder's or a build proxy's own for its sockets ([`socket_dir`]),
/// removed as it drops; and the lock that says its owner lives, which a later build takes
/// once that owner is gone, even killed, to remove what it left ([`sweep`], D137).
#[derive(Debug)]
pub struct SocketDir {
    path: PathBuf,
    /// The lock, let go as the directory goes: Windows removes no file one holds.
    held: Option<std::fs::File>,
}

impl SocketDir {
    /// `path`, an empty directory of this process's own, with its lock taken: held before
    /// it is put in place, so that a lock in place is held.
    #[cfg(unix)]
    pub fn held(path: PathBuf) -> Result<SocketDir, String> {
        let fresh = path.join(".shards-lock-new");
        let held = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&fresh)
            .and_then(|held| {
                // SAFETY: flock(2) of a descriptor this process owns.
                if unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                std::fs::rename(&fresh, path.join(SOCKET_DIR_LOCK))?;
                Ok(held)
            });
        match held {
            Ok(held) => Ok(SocketDir {
                path,
                held: Some(held),
            }),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&path);
                Err(format!("a directory for the builder: {e}"))
            }
        }
    }

    /// `path`, an empty directory of this process's own, with its lock taken (Windows): made
    /// held, shared with no one, which no other process opens while this one holds it, as
    /// shards' AppContainer records are (D141).
    #[cfg(windows)]
    pub fn held(path: PathBuf) -> Result<SocketDir, String> {
        use std::os::windows::fs::OpenOptionsExt as _;
        let held = std::fs::File::options()
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(path.join(SOCKET_DIR_LOCK));
        match held {
            Ok(held) => Ok(SocketDir {
                path,
                held: Some(held),
            }),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&path);
                Err(format!("a directory for the builder: {e}"))
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SocketDir {
    fn drop(&mut self) {
        drop(self.held.take());
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The lock in a [`SocketDir`] its owner holds while it lives (flock(2); on Windows, open
/// and shared with no one).
const SOCKET_DIR_LOCK: &str = ".shards-lock";

/// Removes what builders and build proxies gone before left in `base`, even killed: each
/// directory of theirs, `shards-` and six characters, this user's, whose lock no process
/// holds. Its owner put the lock in place only once it held it, so one free is one whose
/// owner is gone.
fn sweep(base: &Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix("shards-")) else {
            continue;
        };
        if rest.len() != 6 {
            continue;
        }
        let path = entry.path();
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir() && mine(&m))
            && lock_free(&path.join(SOCKET_DIR_LOCK))
        {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Whether this user owns what `m` describes. On Windows the temporary directory is the
/// user's own (`%LOCALAPPDATA%\Temp`), and so is what is in it.
fn mine(m: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: getuid(2).
        m.uid() == unsafe { libc::getuid() }
    }
    #[cfg(windows)]
    {
        let _ = m;
        true
    }
}

/// Whether the lock at `lock` is in place and no process holds it.
fn lock_free(lock: &Path) -> bool {
    #[cfg(unix)]
    {
        let Ok(lock) = std::fs::File::open(lock) else {
            return false;
        };
        // SAFETY: flock(2) of a descriptor this process owns.
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        // A held lock refuses another open (ERROR_SHARING_VIOLATION); a missing one is a
        // directory whose owner has not yet taken it.
        std::fs::File::options()
            .read(true)
            .share_mode(0)
            .open(lock)
            .is_ok()
    }
}

/// Where a builder's VM process finds its `--attach` socket (D134): a descriptor on Unix,
/// the name its spawner handed it by on Windows.
#[cfg(unix)]
const ATTACH: &str = "4";
#[cfg(windows)]
const ATTACH: &str = "attach";

/// The longest socket path a host binds, less the `_<port>` the vsock muxer adds: what
/// both Linux (108 bytes of sun_path) and macOS (104) take, and Windows' afunix (108, its
/// NUL among them).
#[cfg(unix)]
const SOCKET_PATH: usize = 104 - 1 - 11;
#[cfg(windows)]
const SOCKET_PATH: usize = 108 - 1 - 11;

/// A directory of this builder's own for its sockets: made by mkdtemp(3), mode 0700 and a
/// name no other process chose, in the temporary directory, or in /tmp if that one's path
/// is too long for a socket's; what builders gone before left there removed first.
#[cfg(unix)]
fn socket_dir() -> Result<SocketDir, String> {
    use std::os::unix::ffi::OsStringExt;
    #[expect(
        clippy::disallowed_methods,
        reason = "the builder's sockets' directory, made by mkdtemp(3) and removed with the builder (DirGuard)"
    )]
    let tmp = std::env::temp_dir();
    let base = if tmp.as_os_str().len() + "/shards-XXXXXX/v".len() <= SOCKET_PATH {
        tmp
    } else {
        PathBuf::from("/tmp")
    };
    sweep(&base);
    let mut template = base.join("shards-XXXXXX").into_os_string().into_vec();
    template.push(0);
    // SAFETY: a NUL-terminated template mkdtemp rewrites in place.
    if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
        return Err(format!(
            "a directory for the builder: {}",
            io::Error::last_os_error()
        ));
    }
    template.pop();
    SocketDir::held(PathBuf::from(std::ffi::OsString::from_vec(template)))
}

/// A directory of this builder's own for its sockets (Windows): a name of six random
/// characters no other directory has, made where none is (CreateDirectoryW), in the
/// user's own temporary directory; what builders gone before left there removed first.
#[cfg(windows)]
fn socket_dir() -> Result<SocketDir, String> {
    const NAMES: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    #[expect(
        clippy::disallowed_methods,
        reason = "the builder's sockets' directory, removed with the builder (SocketDir)"
    )]
    let base = std::env::temp_dir();
    if base.as_os_str().len() + "\\shards-XXXXXX\\v".len() > SOCKET_PATH {
        return Err(format!(
            "a directory for the builder's sockets: the temporary directory's path ({}) is too long for a \
             socket's ({SOCKET_PATH} bytes)",
            base.display()
        ));
    }
    sweep(&base);
    for _ in 0..64 {
        let mut random = [0u8; 6];
        shards_vmm::platform::fill_random(&mut random)
            .map_err(|e| format!("a directory for the builder: {e}"))?;
        let name: String = random
            .iter()
            .map(|b| char::from(NAMES.get(usize::from(*b) % NAMES.len()).copied().unwrap_or(b'x')))
            .collect();
        let path = base.join(format!("shards-{name}"));
        match std::fs::create_dir(&path) {
            Ok(()) => return SocketDir::held(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("a directory for the builder: {e}")),
        }
    }
    Err("a directory for the builder: every name tried was taken".into())
}

/// A builder's VM process and network process, started: and on Windows the AppContainer
/// its VM runs in.
struct Launched {
    vm: shards_ipc::Child,
    #[cfg(unix)]
    net: shards_ipc::Child,
    #[cfg(windows)]
    net: crate::netproc::NetProcess,
    #[cfg(windows)]
    container: shards_ipc::AppContainer,
}

/// Starts a builder's network process and its VM process, the VM's command `args`, its
/// `--attach` socket `attach`: the network process first, whose side of the ring the VM is
/// given, as a VM's (D31); the VM in App Sandbox on macOS, asking this process for what it
/// opens (D30).
#[cfg(unix)]
fn launch(
    args: &[OsString],
    policy: shards_net::Policy,
    mac: &[u8; 6],
    bridge: &shards_net::bridge::Bridge,
    attach: UnixStream,
) -> Result<Launched, String> {
    let (net, side) = crate::netproc::start(policy, mac, bridge, None)?;
    let grants = if cfg!(target_os = "macos") {
        Some(UnixStream::pair().map_err(|e| format!("the builder's grants: {e}"))?)
    } else {
        None
    };
    let null = std::fs::File::open("/dev/null").map_err(|e| format!("/dev/null: {e}"))?;
    let err = io::stderr();
    let fds: Vec<_> = [
        (null.as_fd(), 0),
        (err.as_fd(), 1),
        (err.as_fd(), 2),
        (side.region.as_fd(), crate::netproc::VM_FDS[0]),
        (side.sleeps.as_fd(), crate::netproc::VM_FDS[1]),
        (side.rings.as_fd(), crate::netproc::VM_FDS[2]),
        (attach.as_fd(), 4),
    ]
    .into_iter()
    .chain(grants.as_ref().map(|(_, theirs)| (theirs.as_fd(), 3)))
    .collect();
    let given: &[OsString] = if grants.is_some() {
        &["--grants".into(), "3".into()]
    } else {
        &[]
    };
    let argv: Vec<&std::ffi::OsStr> = args
        .iter()
        .take(1)
        .chain(given)
        .chain(args.iter().skip(1))
        .map(OsString::as_os_str)
        .collect();
    let mut env = crate::netproc::child_env();
    // A GPU's builder is the GPU's VM process (D144), which says MoltenVK's errors alone.
    let vm_binary = if args.iter().any(|a| *a == "--gpu") {
        env.push(("MVK_CONFIG_LOG_LEVEL", "1".into()));
        crate::helpers::vm_gpu()?
    } else {
        crate::helpers::vm()?
    };
    let vm = match shards_ipc::spawn_in(&vm_binary, &argv, &fds, false, &crate::netproc::env_pairs(&env)) {
        Ok(vm) => vm,
        Err(e) => {
            let _ = net.kill(libc::SIGKILL);
            let _ = net.wait();
            return Err(format!("starting the builder: {e}"));
        }
    };
    // The two processes hold the ring now: once the VM goes, its network process hears
    // its doorbell hang up and goes too.
    drop(fds);
    drop(side);
    drop(attach);
    if let Some((ours, _)) = grants {
        #[cfg(target_os = "macos")]
        std::thread::Builder::new()
            .name("builder grants".into())
            .spawn(move || {
                let _ = crate::grant_answer::serve(&ours, None);
            })
            .map_err(|e| format!("answering the builder: {e}"))?;
        #[cfg(not(target_os = "macos"))]
        drop(ours);
    }
    Ok(Launched { vm, net })
}

/// Starts a builder's VM process and network process (Windows, D141, D142): the VM
/// confined in an AppContainer of its own, made suspended, so that its network process,
/// started next in one holding the network's capabilities, is given the VM's process to
/// watch for its end; then the VM runs, and this process answers what it asks for (its
/// base images, its vsock socket, its dials to this process's ports) on a thread of its
/// own, which ends as the VM does.
#[cfg(windows)]
fn launch(
    args: &[OsString],
    policy: shards_net::Policy,
    mac: &[u8; 6],
    bridge: &shards_net::bridge::Bridge,
    attach: UnixStream,
) -> Result<Launched, String> {
    use std::os::windows::io::{AsHandle as _, OwnedHandle};
    let (ready, side) = crate::netproc::prepare(policy, mac, bridge)?;
    let vm_binary = crate::helpers::vm()?;
    let container = crate::helpers::containers().and_then(|records| {
        shards_ipc::AppContainer::new("vm", &records).map_err(|e| format!("the builder's AppContainer: {e}"))
    })?;
    // A pipe each way: the VM's requests, and the answers.
    let pipes = std::io::pipe().and_then(|asks| Ok((asks, std::io::pipe()?)));
    let ((asks_from, asks_to), (answers_from, answers_to)) =
        pipes.map_err(|e| format!("the builder's grants: {e}"))?;
    let (asks_to, answers_from) = (OwnedHandle::from(asks_to), OwnedHandle::from(answers_from));
    let argv: Vec<&std::ffi::OsStr> = args.iter().map(OsString::as_os_str).collect();
    let mut given = vec![
        shards_ipc::Given {
            name: "ask",
            handle: asks_to.as_handle(),
        },
        shards_ipc::Given {
            name: "answer",
            handle: answers_from.as_handle(),
        },
    ];
    given.extend(side.given());
    let err = io::stderr();
    let env = crate::netproc::child_env();
    let vm = shards_ipc::spawn_suspended(&shards_ipc::Spawn {
        program: &vm_binary,
        args: &argv,
        grants_at: 1,
        given: &given,
        stdin: None,
        stdout: err.as_handle(),
        stderr: err.as_handle(),
        sockets: &[
            shards_ipc::Shared {
                name: ATTACH,
                socket: attach.raw_socket() as u64,
            },
            side.shared(),
        ],
        env: &crate::netproc::env_pairs(&env),
        container: Some(&container),
        holds: shards_ipc::Holds::Hypervisor,
    })
    .map_err(|e| format!("starting the builder: {e}"))?;
    // Ended with the suspended VM, a Job's, where its network process does not start.
    let net = crate::netproc::start(ready, &vm)?;
    let giver = vm.giver().map_err(|e| format!("answering the builder: {e}"))?;
    let vm = vm.resume().map_err(|e| format!("starting the builder: {e}"))?;
    // Its ends are the VM's now: its exit ends its requests, and its network process,
    // which watches it, goes with it.
    drop(given);
    drop((asks_to, answers_from, side, attach));
    let link = crate::grant::Link {
        from: std::fs::File::from(OwnedHandle::from(asks_from)),
        to: std::fs::File::from(OwnedHandle::from(answers_to)),
    };
    std::thread::Builder::new()
        .name("builder grants".into())
        .spawn(move || {
            let _ = crate::grant_answer::serve(&link, &giver);
        })
        .map_err(|e| format!("answering the builder: {e}"))?;
    Ok(Launched { vm, net, container })
}

/// Ends a builder's VM process that does not start.
fn kill(vm: &shards_ipc::Child) {
    #[cfg(unix)]
    let _ = vm.kill(libc::SIGKILL);
    #[cfg(windows)]
    let _ = vm.kill();
}

impl Builder {
    pub fn start(boot: &Boot<'_>) -> Result<Builder, String> {
        // Removed if the builder does not start; the builder removes it once it does.
        let dir = socket_dir()?;
        let vsock = dir.path().join("v");
        let mut dial = vsock.clone().into_os_string();
        dial.push(format!("_{}", build::PORT));
        let listener = UnixListener::bind(&dial).map_err(|e| format!("the builder's port: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("the builder's port: {e}"))?;
        let mut ssh_at = vsock.clone().into_os_string();
        ssh_at.push(format!("_{}", build::SSH_PORT));
        let ssh = UnixListener::bind(&ssh_at).map_err(|e| format!("the builder's SSH port: {e}"))?;
        ssh.set_nonblocking(true)
            .map_err(|e| format!("the builder's SSH port: {e}"))?;
        // The builder's network: BuildKit's steps reach what their host does, so the
        // network process allows every flow but to the host itself (D31); under the build's
        // proxy (D110), the gateway's proxy port alone, carried to the proxy's socket.
        let mac = shards_net::random_mac().map_err(|e| format!("the builder's MAC: {e}"))?;
        // On the default bridge, as dockerd's builder runs its steps, elected as the
        // daemon elects it: a host on 172.17.0.0/16 keeps its own network reachable.
        let bridge = shards_net::bridge::elected_here(&mut |_| {}).ok_or(shards_net::bridge::NO_SUBNET)?;
        let policy = match boot.proxy {
            Some(p) => shards_net::Policy::Proxy {
                port: super::proxy::PORT,
                socket: p.socket().to_path_buf(),
            },
            None => shards_net::Policy::AllowAll,
        };
        let proxy_url = boot
            .proxy
            .map(|_| format!("http://{}:{}", bridge.gateway(), super::proxy::PORT));
        let mut args: Vec<OsString> = vec![
            "run".into(),
            "--kernel".into(),
            boot.kernel.into(),
            "--init".into(),
            boot.init.into(),
            "--cmdline".into(),
            // Memory plugged comes online as the kernel's own, all of it usable by the
            // kernel's allocations as well as the build's: a builder never gives it back,
            // where memory online as movable alone could leave the kernel short with
            // memory free (Documentation/admin-guide/mm/memory-hotplug.rst, "ZONE_MOVABLE
            // Sizing Considerations").
            // Real-time scheduling for a step that asks for it, as for a run (D155).
            // Each step's /dev/shm half the memory the builder may have (D156), as a run's.
            format!(
                "console=ttyS0 earlycon panic=-1 shards_build=1 memhp_default_state=online_kernel rt_group_sched=0 shards_shm={} {}",
                crate::run::shm_bytes(boot.memory_mib),
                bridge.cmdline()
            )
            .into(),
            "--cpus".into(),
            boot.cpus.to_string().into(),
            "--memory".into(),
            boot.memory_mib.min(BOOT_MEMORY_MIB).to_string().into(),
            "--memory-max".into(),
            boot.memory_mib.to_string().into(),
            "--vsock".into(),
            vsock.clone().into(),
            "--net".into(),
            crate::netproc::VmSide::arg(&mac).into(),
            "--no-console".into(),
        ];
        for base in boot.bases {
            args.extend(["--pmem".into(), base.into()]);
        }
        if let Some((image, _)) = boot.emulators {
            args.extend(["--pmem".into(), image.into()]);
        }
        if boot.gpu {
            args.push("--gpu".into());
        }
        // Memory its steps free given back to the host as the guest reports it free
        // (D151): a build's peak is not held to its end. Not yet over WHP, whose taking a
        // part of guest RAM away and back is unmeasured.
        if cfg!(unix) {
            args.push("--reporting".into());
        }
        // The files this process attaches to its join disk for its steps' caches (D134).
        let (attach, attach_theirs) =
            UnixStream::pair().map_err(|e| format!("the builder's join disk: {e}"))?;
        args.extend(["--attach".into(), ATTACH.into()]);
        let launched = launch(&args, policy, &mac, &bridge, attach_theirs)?;
        let vm = launched.vm;
        let began = Instant::now();
        let conn = loop {
            match listener.accept() {
                Ok((conn, _)) => break conn,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if began.elapsed() > BOOT_PATIENCE {
                        // One the host has not let run yet is not the builder's failure
                        // to boot, and says so (PM M166).
                        #[cfg(unix)]
                        let unstarted = shards_vmm::platform::launched(vm.id()) == Some(false);
                        #[cfg(windows)]
                        let unstarted = false;
                        kill(&vm);
                        if unstarted {
                            return Err(format!(
                                "the builder was not started by the host in {BOOT_PATIENCE:?}: its \
                                 process ran nothing, as macOS admits each launch after it has \
                                 assessed the executables launched before it"
                            ));
                        }
                        return Err("the builder did not start".into());
                    }
                    if vm.try_wait().is_some() {
                        return Err("the builder ended as it started".into());
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(format!("the builder's port: {e}")),
            }
        };
        conn.set_nonblocking(false)
            .map_err(|e| format!("the builder's connection: {e}"))?;
        let mut builder = Builder {
            vm,
            net: launched.net,
            #[cfg(windows)]
            _container: launched.container,
            conn,
            ssh,
            _dir: dir,
            layers: HashMap::new(),
            next: 0,
            proxy_url,
            attach,
            attached: HashMap::new(),
        };
        if let Some((_, architectures)) = boot.emulators {
            let device = u32::try_from(boot.bases.len()).map_err(|_| "too many base images")?;
            builder.emulate(device, architectures)?;
        }
        Ok(builder)
    }

    /// Has the guest register the emulators of `architectures` (D179), from the emulator
    /// image on its pmem device `device`, before any step.
    fn emulate(&mut self, device: u32, architectures: &[&str]) -> Result<(), String> {
        let mut payload = device.to_be_bytes().to_vec();
        for a in architectures {
            payload.extend_from_slice(a.as_bytes());
            payload.push(0);
        }
        self.frame(kind::EMULATE, &payload)?;
        let mut buf = Vec::new();
        match self.read_frame(&mut buf)? {
            kind::EMULATED if buf.is_empty() => Ok(()),
            kind::EMULATED => Err(format!(
                "the builder could not register its emulators: {}",
                String::from_utf8_lossy(&buf)
            )),
            other => Err(format!("the builder answered its emulators with frame {other}")),
        }
    }

    /// Attaches `file` to the builder's join disk, read-only or for the guest to write too:
    /// the range it was given, where it starts and its length.
    fn attach(&mut self, file: &std::fs::File, writable: bool) -> Result<(u64, u64), String> {
        use shards_ipc::kind;
        #[cfg(unix)]
        shards_ipc::send(
            &self.attach,
            kind::DISK_ATTACH,
            &[u8::from(writable)],
            &[file.as_fd()],
        )
        .map_err(|e| format!("attaching a cache to the builder: {e}"))?;
        // The file's handle given the VM process, its value there after the mode: a
        // message on Windows carries no handle (shards_ipc).
        #[cfg(windows)]
        {
            use std::os::windows::io::AsHandle as _;
            let theirs = self
                .vm
                .give(file.as_handle())
                .map_err(|e| format!("attaching a cache to the builder: {e}"))?;
            let mut payload = vec![u8::from(writable)];
            payload.extend((theirs as u64).to_le_bytes());
            shards_ipc::send(&self.attach, kind::DISK_ATTACH, &payload)
                .map_err(|e| format!("attaching a cache to the builder: {e}"))?;
        }
        let m = shards_ipc::recv(&self.attach)
            .map_err(|e| format!("attaching a cache to the builder: {e}"))?
            .ok_or("the builder's VM went as a cache was attached")?;
        match (m.kind, m.payload.split_first_chunk::<8>()) {
            (kind::DISK_ATTACH, Some((start, len))) => Ok((
                u64::from_be_bytes(*start),
                len.try_into()
                    .map(u64::from_be_bytes)
                    .map_err(|_| "a malformed range")?,
            )),
            (kind::DISK_REFUSED, _) => Err(format!(
                "the builder refused a cache: {}",
                String::from_utf8_lossy(&m.payload)
            )),
            (other, _) => Err(format!("the builder answered an attach with message {other}")),
        }
    }

    /// The range of the join disk that cache layer `layer`, open as `file`, has: given
    /// once for the builder's life, read-only.
    pub fn attach_layer(&mut self, layer: &str, file: &std::fs::File) -> Result<(u64, u64), String> {
        if let Some(&range) = self.attached.get(layer) {
            return Ok(range);
        }
        let range = self.attach(file, false)?;
        self.attached.insert(layer.to_string(), range);
        Ok(range)
    }

    /// A range of the join disk the guest writes a cache's changes to: `file`'s, until it
    /// is let go ([`Builder::detach`]).
    pub fn attach_out(&mut self, file: &std::fs::File) -> Result<(u64, u64), String> {
        self.attach(file, true)
    }

    /// Lets go of the range that starts at `start`: the VM process closes its file.
    pub fn detach(&mut self, start: u64) -> Result<(), String> {
        use shards_ipc::kind;
        #[cfg(unix)]
        let sent = shards_ipc::send(&self.attach, kind::DISK_DETACH, &start.to_be_bytes(), &[]);
        #[cfg(windows)]
        let sent = shards_ipc::send(&self.attach, kind::DISK_DETACH, &start.to_be_bytes());
        sent.map_err(|e| format!("detaching a cache from the builder: {e}"))?;
        let m = shards_ipc::recv(&self.attach)
            .map_err(|e| format!("detaching a cache from the builder: {e}"))?
            .ok_or("the builder's VM went as a cache was detached")?;
        match m.kind {
            kind::DISK_DETACH => Ok(()),
            kind::DISK_REFUSED => Err(format!(
                "the builder refused to let a cache go: {}",
                String::from_utf8_lossy(&m.payload)
            )),
            other => Err(format!("the builder answered a detach with message {other}")),
        }
    }

    /// The URL its steps reach the build's proxy at, where it has one (D110).
    pub fn proxy_url(&self) -> Option<&str> {
        self.proxy_url.as_deref()
    }

    fn frame(&self, which: u8, payload: &[u8]) -> Result<(), String> {
        let len = u32::try_from(payload.len()).map_err(|_| "a frame too long".to_string())?;
        let mut w = &self.conn;
        io::Write::write_all(&mut w, &run::header(which, len))
            .and_then(|()| io::Write::write_all(&mut w, payload))
            .map_err(|e| format!("telling the builder: {e}"))
    }

    fn read_frame(&self, buf: &mut Vec<u8>) -> Result<u8, String> {
        let mut h = [0u8; run::HEADER];
        let mut r = super::reading(&self.conn);
        r.read_exact(&mut h)
            .map_err(|e| format!("hearing the builder: {e}"))?;
        let (which, len) = run::parse_header(h).ok_or("the builder sent a malformed frame")?;
        buf.resize(len as usize, 0);
        r.read_exact(buf)
            .map_err(|e| format!("hearing the builder: {e}"))?;
        Ok(which)
    }

    /// A fresh layer id.
    fn layer_id(&mut self) -> u32 {
        let id = self.next;
        self.next += 1;
        id
    }

    /// Sends `fs` as a layer, the step's changes or all of it, and waits until the guest
    /// has it.
    fn send(&mut self, fs: &Fs, scope: Scope, data: &mut dyn Source) -> Result<u32, String> {
        let id = self.layer_id();
        let mut pending: Vec<u8> = id.to_be_bytes().to_vec();
        let limit = run::MAX_PAYLOAD as usize;
        let mut failed: Option<String> = None;
        sync::write(fs, scope, data, &mut |bytes| {
            let mut bytes = bytes;
            while !bytes.is_empty() {
                let room = limit - pending.len();
                let (now, later) = bytes.split_at(room.min(bytes.len()));
                pending.extend_from_slice(now);
                bytes = later;
                if pending.len() == limit {
                    if let Err(e) = self.frame(kind::LAYER, &pending) {
                        failed = Some(e);
                        return Err(io::Error::other("the builder is gone"));
                    }
                    pending.truncate(4);
                }
            }
            Ok(())
        })
        .map_err(|e| failed.take().unwrap_or(e.0))?;
        if pending.len() > 4 {
            self.frame(kind::LAYER, &pending)?;
        }
        let mut buf = Vec::new();
        match self.read_frame(&mut buf)? {
            kind::LAYERED => Ok(id),
            other => Err(format!("the builder answered a layer with frame {other}")),
        }
    }

    /// The tree `origin` is, as layers in the guest over a base, sending what the guest
    /// lacks of it.
    pub fn tree(&mut self, origin: &Rc<Origin>, data: &mut dyn Source) -> Result<Tree, String> {
        let mut tree = Tree::default();
        self.stack(origin, data, &mut tree)?;
        Ok(tree)
    }

    /// The layer the guest holds as `id`, sending `fs` as `scope` first if it has none.
    fn layer(&mut self, id: u64, fs: &Fs, scope: Scope, data: &mut dyn Source) -> Result<u32, String> {
        if let Some(&layer) = self.layers.get(&id) {
            return Ok(layer);
        }
        let layer = self.send(fs, scope, data)?;
        self.layers.insert(id, layer);
        Ok(layer)
    }

    fn stack(&mut self, origin: &Rc<Origin>, data: &mut dyn Source, tree: &mut Tree) -> Result<(), String> {
        match &**origin {
            Origin::Scratch => {}
            Origin::Image(n) => tree.base = Some(*n),
            Origin::Run { parent, layer } => {
                self.stack(parent, data, tree)?;
                tree.layers.push(*layer);
            }
            Origin::Merge(parts) => {
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 && has_base(part) {
                        // A part on an image cannot stack as layers: it goes whole.
                        let (id, fs) = whole_of(part).ok_or("a merged image's tree is not at hand")?;
                        tree.layers.push(self.layer(id, &fs, Scope::Whole, data)?);
                    } else {
                        self.stack(part, data, tree)?;
                    }
                }
            }
            Origin::Step { id, parent, fs, step } => {
                self.stack(parent, data, tree)?;
                tree.layers.push(self.layer(*id, fs, Scope::Step(*step), data)?);
            }
            Origin::Whole { id, fs } => tree.layers.push(self.layer(*id, fs, Scope::Whole, data)?),
        }
        Ok(())
    }

    /// Runs `step` (its `upper` set here), passing its output to `out` as it comes; once
    /// it succeeds, puts what it changed into `applier`, and returns each output mount's
    /// changes (`outputs`' indexes, at most `max` bytes each) with the layer it became.
    /// How it ended, the layer its changes are in the guest, and what its requests came to
    /// under the build's proxy; what its changes to each cache it writes came to comes
    /// next, whatever its end, each for [`Builder::kept`].
    ///
    /// Under the build's proxy (`proxy`, D110), its connections to the proxy are served
    /// while it runs, each request's question answered here, on the build's thread, by
    /// the proxy's `check`; as it ends they are shut.
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        &mut self,
        mut step: Step,
        out: &mut dyn FnMut(u8, &[u8]),
        applier: &mut Applier<'_>,
        agents: &super::Agents,
        max: u64,
        proxy: StepProxy<'_, '_>,
    ) -> Result<Ran, String> {
        step.upper = self.layer_id();
        for o in &mut step.outputs {
            o.1 = self.layer_id();
        }
        let layers: Vec<u32> = step.outputs.iter().map(|o| o.1).collect();
        // Its session, begun before it starts: its requests go upstream through the
        // proxies the build's environment names, as BuildKit's through its daemon's.
        let (session, questions, mut check) = match proxy {
            Some((p, check)) => {
                let upstream = shards_registry::proxy::Proxies::from_env(&|k| std::env::var(k).ok());
                let (session, questions) = super::proxy::Session::begin(p, upstream)?;
                (Some(session), Some(questions), Some(check))
            }
            None => (None, None, None),
        };
        let (stop_proxy, proxy_stopped) =
            UnixStream::pair().map_err(|e| format!("the build's proxy: {e}"))?;
        self.frame(kind::STEP, &step.encode())?;
        // The SSH agents this step may reach: each mount's token and the agent's id.
        let grants: Vec<([u8; 16], Vec<u8>)> = step
            .mounts
            .iter()
            .filter_map(|(_, m)| match m {
                build::Mount::Ssh { token, id, .. } => Some((*token, id.clone())),
                _ => None,
            })
            .collect();
        // Every step's relayed connections are taken while it runs, and those of no grant
        // of its closed at once: one a step makes of its own (an insecure step can dial the
        // port) waits on nothing. The step's end wakes the taker (`wake`).
        let ssh = self
            .ssh
            .try_clone()
            .map_err(|e| format!("the builder's SSH port: {e}"))?;
        let (wake, woken) = UnixStream::pair().map_err(|e| format!("the builder's SSH port: {e}"))?;
        let ran = std::thread::scope(|scope| {
            let (ssh, grants, woken) = (&ssh, &grants, &woken);
            let _ = std::thread::Builder::new()
                .name("ssh agents".into())
                .spawn_scoped(scope, move || accept_agents(scope, ssh, grants, agents, woken));
            if let Some(s) = &session {
                let stopped = &proxy_stopped;
                let _ = std::thread::Builder::new()
                    .name("build proxy".into())
                    .spawn_scoped(scope, move || s.serve(scope, stopped));
            }
            let asked = match (&questions, check.take()) {
                (Some(q), Some(c)) => Some((q, c)),
                _ => None,
            };
            let r = self.frames(step.upper, out, applier, &layers, max, asked);
            drop(wake);
            // The step is over: its proxy takes no more, its connections shut, its
            // questions unanswered refused.
            drop(stop_proxy);
            if let Some(s) = &session {
                s.end();
            }
            drop(questions);
            r
        });
        let (ended, upper, outputs) = ran?;
        Ok((ended, upper, outputs, session.map(|s| s.capture())))
    }

    /// Waits until the builder has a frame to read, answering the step's questions of the
    /// build's policies as they come (D110).
    fn wait_frame(
        &self,
        questions: &super::proxy::Questions,
        check: &mut dyn FnMut(&str, &str) -> bool,
    ) -> Result<(), String> {
        loop {
            questions.answer(check);
            let ready = shards_vmm::platform::wait_readable(
                &[self.conn.raw_socket(), questions.woken.raw_socket()],
                None,
            )
            .map_err(|e| format!("hearing the builder: {e}"))?;
            if ready.first() == Some(&true) {
                return Ok(());
            }
        }
    }

    /// What the step's changes to its next cache that it writes came to (D134), as the
    /// guest wrote them to the cache's `out` range.
    pub fn kept(&mut self) -> Result<Kept, String> {
        let mut buf = Vec::new();
        match self.read_frame(&mut buf)? {
            kind::MOUNT_END => {
                Kept::decode(&buf).ok_or_else(|| "the builder said a malformed cache's end".into())
            }
            other => Err(format!("the builder sent frame {other} for a cache's changes")),
        }
    }

    /// A step's frames: its output, then its status, then, if it succeeded, its changes.
    /// Its questions of the build's policies, where it is under the build's proxy, are
    /// answered until its status comes.
    fn frames(
        &mut self,
        upper: u32,
        out: &mut dyn FnMut(u8, &[u8]),
        applier: &mut Applier<'_>,
        layers: &[u32],
        max: u64,
        mut asked: Asked<'_, '_>,
    ) -> Result<(Ended, u32, Outputs), String> {
        let mut buf = Vec::new();
        let mut not_run: Option<String> = None;
        let status = loop {
            if let Some((questions, check)) = &mut asked {
                self.wait_frame(questions, &mut **check)?;
            }
            match self.read_frame(&mut buf)? {
                which @ (run::kind::STDOUT | run::kind::STDERR) => out(which, &buf),
                run::kind::SYSTEM_ERR => {
                    not_run = Some(String::from_utf8_lossy(&buf).into_owned());
                    continue;
                }
                run::kind::EXIT => {
                    let b: [u8; 4] = buf.as_slice().try_into().map_err(|_| "a malformed status")?;
                    break u32::from_be_bytes(b);
                }
                other => return Err(format!("the builder sent frame {other} during a step")),
            }
        };
        if let Some(why) = not_run {
            return Ok((Ended::NotRun(why), upper, Vec::new()));
        }
        if status == 0 {
            loop {
                match self.read_frame(&mut buf)? {
                    kind::CHANGES => applier.feed(&buf).map_err(|e| e.0)?,
                    other => return Err(format!("the builder sent frame {other} for a step's changes")),
                }
                if applier.ended() {
                    break;
                }
            }
        }
        // Each output mount's changes, whole, in order.
        let mut outputs = Vec::new();
        if status == 0 {
            for &layer in layers {
                let mut stream = Vec::new();
                loop {
                    match self.read_frame(&mut buf)? {
                        kind::MOUNT_CHANGES => {
                            if (stream.len() + buf.len()) as u64 > max {
                                return Err(format!("an output mount's changes past {max} bytes"));
                            }
                            stream.extend_from_slice(&buf);
                        }
                        kind::MOUNT_END => break,
                        other => return Err(format!("the builder sent frame {other} for a mount's changes")),
                    }
                }
                outputs.push((layer, stream));
            }
        }
        Ok((Ended::Status(status), upper, outputs))
    }
}

/// The longest message an SSH agent takes or sends (OpenSSH's AGENT_MAX_MSGLEN).
pub(crate) const AGENT_MAX: usize = 256 * 1024;

/// What a step may ask of an agent, as BuildKit's read-only agent lets it
/// (sshprovider readOnlyAgent): its keys, signatures, and unlocking; adding, removing or
/// locking keys and extensions are refused.
pub(crate) const AGENT_ALLOWED: [u8; 3] = [11, 13, 23];

/// Takes a step's relayed agent connections until `woken` says the step has ended (its
/// other end closed), each on a thread of `scope`.
fn accept_agents<'s, 'e>(
    scope: &'s std::thread::Scope<'s, 'e>,
    ssh: &'e UnixListener,
    grants: &'e [([u8; 16], Vec<u8>)],
    agents: &'e super::Agents,
    woken: &'e UnixStream,
) {
    loop {
        let Ok(ready) = shards_vmm::platform::wait_readable(&[ssh.raw_socket(), woken.raw_socket()], None)
        else {
            return;
        };
        if ready.get(1) != Some(&false) {
            return;
        }
        let Ok((conn, _)) = ssh.accept() else { continue };
        let _ = std::thread::Builder::new()
            .name("ssh agent".into())
            .spawn_scoped(scope, move || {
                let _ = serve_agent(conn, grants, agents);
            });
    }
}

/// One relayed connection: opened with a grant's token and its agent's id, or closed; then
/// each request the step makes, passed to the agent if a read-only agent takes it,
/// refused (SSH_AGENT_FAILURE) if not.
fn serve_agent(
    conn: UnixStream,
    grants: &[([u8; 16], Vec<u8>)],
    agents: &super::Agents,
) -> std::io::Result<()> {
    use std::io::{Read as _, Write as _};
    conn.set_nonblocking(false)?;
    let mut from = super::reading(&conn);
    let mut token = [0u8; 16];
    from.read_exact(&mut token)?;
    let mut len = [0u8; 2];
    from.read_exact(&mut len)?;
    let mut id = vec![0u8; usize::from(u16::from_be_bytes(len))];
    from.read_exact(&mut id)?;
    if !grants.iter().any(|(t, i)| *t == token && *i == id) {
        return Ok(());
    }
    let Some(agent) = agents.get(&*String::from_utf8_lossy(&id)) else {
        return Ok(());
    };
    let read_msg = |from: &mut dyn std::io::Read| -> std::io::Result<Option<Vec<u8>>> {
        let mut n = [0u8; 4];
        match from.read_exact(&mut n) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let n = usize::try_from(u32::from_be_bytes(n)).unwrap_or(usize::MAX);
        if n == 0 || n > AGENT_MAX {
            return Err(std::io::Error::other("an agent message of a bad length"));
        }
        let mut body = vec![0u8; n];
        from.read_exact(&mut body)?;
        Ok(Some(body))
    };
    let write_msg = |to: &mut dyn std::io::Write, body: &[u8]| -> std::io::Result<()> {
        to.write_all(&u32::try_from(body.len()).unwrap_or(0).to_be_bytes())?;
        to.write_all(body)
    };
    let path = match agent {
        Agent::Socket(path) => path,
        // The keys of files: answered here, as BuildKit's keyring answers, read-only.
        Agent::Keys(keys) => {
            while let Some(request) = read_msg(&mut from)? {
                write_msg(&mut &conn, &super::sshkey::answer(keys, &request))?;
            }
            return Ok(());
        }
    };
    let mut agent = super::ssh::agent_stream(path)?;
    while let Some(request) = read_msg(&mut from)? {
        let allowed = request.first().is_some_and(|t| AGENT_ALLOWED.contains(t));
        if !allowed {
            (&conn).write_all(&[0, 0, 0, 1, 5])?;
            continue;
        }
        write_msg(&mut agent, &request)?;
        let Some(answer) = read_msg(&mut agent)? else {
            return Ok(());
        };
        write_msg(&mut &conn, &answer)?;
    }
    Ok(())
}

/// Whether `origin` stands on a base image.
fn has_base(origin: &Origin) -> bool {
    match origin {
        Origin::Image(_) => true,
        Origin::Scratch | Origin::Whole { .. } => false,
        Origin::Step { parent, .. } | Origin::Run { parent, .. } => has_base(parent),
        Origin::Merge(parts) => parts.first().is_some_and(|p| has_base(p)),
    }
}

/// The snapshot an origin ends in, where it holds one, and the id to send it whole by:
/// another than its own part's, which is its changes alone.
fn whole_of(origin: &Origin) -> Option<(u64, Rc<Fs>)> {
    match origin {
        Origin::Step { id, fs, .. } | Origin::Whole { id, fs } => Some((id ^ (1 << 63), fs.clone())),
        _ => None,
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        // The guest powers off as its connection closes.
        super::end(&self.conn);
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.vm.try_wait().is_none() {
            if Instant::now() > deadline {
                kill(&self.vm);
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // The network process goes with its VM; its directory and AppContainer, fields,
        // after this.
        crate::netproc::reap(&self.net);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sweep removes the directories builds gone before left, each whose lock no one
    /// holds, and none whose build lives, nor one whose lock is not yet in place, nor
    /// anything not a build's (D137).
    #[test]
    fn a_sweep_removes_what_dead_builds_left() {
        let base = shards_testdir::TempDir::new("sweep").unwrap();
        let made = |name: &str| {
            let path = base.join(name);
            std::fs::create_dir(&path).unwrap();
            path
        };
        let live = SocketDir::held(made("shards-live01")).unwrap();
        // A build gone, even killed: its lock in place, and no one holding it.
        let gone = made("shards-gone01");
        std::fs::write(gone.join(SOCKET_DIR_LOCK), b"").unwrap();
        // One whose owner is putting its lock in place: on Unix, held and not yet renamed
        // into place; on Windows, not yet made.
        let starting = made("shards-start1");
        #[cfg(unix)]
        std::fs::write(starting.join(".shards-lock-new"), b"").unwrap();
        let other = made("shards-other-thing");
        std::fs::write(other.join(SOCKET_DIR_LOCK), b"").unwrap();
        sweep(&base);
        assert!(live.path().exists(), "a live build's");
        assert!(!gone.exists(), "a lock no one holds");
        assert!(starting.exists(), "a lock not yet in place");
        assert!(other.exists(), "no build's");
        drop(live);
        assert!(!base.join("shards-live01").exists(), "removed as it drops");
    }
}
