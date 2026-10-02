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

#[cfg(unix)]
use std::collections::HashMap;
#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::io::{self, Read};
#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use shards_abi::build::{self, kind};
use shards_abi::build::{Step, Tree};
#[cfg(unix)]
use shards_abi::run;
#[cfg(unix)]
use shards_build::sync::{self, Scope};
use shards_build::upper::Applier;
use shards_build::vfs::Fs;
use shards_image::erofs::Source;

/// How long a builder may take to boot and dial back.
#[cfg(unix)]
const BOOT_PATIENCE: Duration = Duration::from_secs(30);

/// Where a snapshot's tree comes from, as a builder guest holds it. Only a builder reads
/// one: where none starts yet (Windows), nothing does.
#[derive(Debug)]
#[cfg_attr(not(unix), allow(dead_code))]
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
#[cfg(unix)]
pub struct Builder {
    vm: shards_ipc::Child,
    /// Its network process, which ends with the VM.
    net: shards_ipc::Child,
    conn: UnixStream,
    /// Where its vsock socket is: removed with the builder.
    dir: PathBuf,
    /// The layer each origin's own part is, once the guest has it, by the origin's id.
    layers: HashMap<u64, u32>,
    next: u32,
}

/// Where shards starts no builder yet: Windows, whose daemon and VM transport are still
/// to come. Each `RUN` there says so.
#[cfg(not(unix))]
#[derive(Debug)]
pub struct Builder;

#[cfg(not(unix))]
impl Builder {
    pub fn start(_: &Boot<'_>) -> Result<Builder, String> {
        Err(
            "RUN steps run in a builder microVM, which shards starts on Linux and macOS hosts only so far"
                .into(),
        )
    }

    pub fn tree(&mut self, _: &Rc<Origin>, _: &mut dyn Source) -> Result<Tree, String> {
        Err("no builder".into())
    }

    pub fn run(
        &mut self,
        _: Step,
        _: &mut dyn FnMut(u8, &[u8]),
        _: &mut Applier<'_>,
    ) -> Result<(Ended, u32), String> {
        Err("no builder".into())
    }
}

#[cfg(unix)]
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
#[cfg_attr(not(unix), allow(dead_code))]
pub struct Boot<'a> {
    pub kernel: &'a Path,
    pub init: &'a Path,
    pub cpus: u32,
    pub memory_mib: u64,
    pub bases: &'a [PathBuf],
}

/// Removes a directory as it goes out of scope, unless it was let go.
#[cfg(unix)]
struct DirGuard(Option<PathBuf>);

#[cfg(unix)]
impl Drop for DirGuard {
    fn drop(&mut self) {
        if let Some(d) = self.0.take() {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

#[cfg(unix)]
/// The longest socket path both Linux (108 bytes of sun_path) and macOS (104) bind, less
/// the `_<port>` the vsock muxer adds.
const SOCKET_PATH: usize = 104 - 1 - 11;

/// A directory of this builder's own for its sockets: made by mkdtemp(3), mode 0700 and a
/// name no other process chose, in the temporary directory, or in /tmp if that one's path
/// is too long for a socket's.
#[cfg(unix)]
fn socket_dir() -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    let tmp = std::env::temp_dir();
    let base = if tmp.as_os_str().len() + "/shards-XXXXXX/v".len() <= SOCKET_PATH {
        tmp
    } else {
        PathBuf::from("/tmp")
    };
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
    Ok(PathBuf::from(std::ffi::OsString::from_vec(template)))
}

#[cfg(unix)]
impl Builder {
    pub fn start(boot: &Boot<'_>) -> Result<Builder, String> {
        // Removed if the builder does not start; the builder removes it once it does.
        let guard = DirGuard(Some(socket_dir()?));
        let dir = guard.0.clone().unwrap_or_default();
        let vsock = dir.join("v");
        let mut dial = vsock.clone().into_os_string();
        dial.push(format!("_{}", build::PORT));
        let listener = UnixListener::bind(&dial).map_err(|e| format!("the builder's port: {e}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| format!("the builder's port: {e}"))?;
        let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
        // The builder's network: BuildKit's steps reach what their host does, so the
        // network process allows every flow but to the host itself (D31).
        let net_cfg = shards_net::Config::docker_default(shards_net::Policy::AllowAll);
        let ring = |e: io::Error| format!("the builder's network: {e}");
        let region = shards_netring::memory().map_err(ring)?;
        // The VM sleeps on the first doorbell and rings the second; the network process
        // the other way round.
        let (vm_sleeps, net_rings) = shards_netring::doorbell().map_err(ring)?;
        let (net_sleeps, vm_rings) = shards_netring::doorbell().map_err(ring)?;
        let net = shards_ipc::spawn(
            &exe.with_file_name(format!("shards-net{}", std::env::consts::EXE_SUFFIX)),
            &[
                "--ring".as_ref(),
                "3,4,5".as_ref(),
                "--policy".as_ref(),
                match net_cfg.policy {
                    shards_net::Policy::AllowAll => "allow",
                    shards_net::Policy::DenyAll => "deny",
                }
                .as_ref(),
            ],
            &[
                (io::stderr().as_fd(), 2),
                (region.as_fd(), 3),
                (net_sleeps.as_fd(), 4),
                (net_rings.as_fd(), 5),
            ],
            false,
        )
        .map_err(|e| format!("starting the builder's network: {e}"))?;
        drop((net_sleeps, net_rings));
        let mut args: Vec<OsString> = vec![
            "run".into(),
            "--kernel".into(),
            boot.kernel.into(),
            "--init".into(),
            boot.init.into(),
            "--cmdline".into(),
            format!(
                "console=ttyS0 earlycon panic=-1 shards_build=1 {}",
                net_cfg.cmdline()
            )
            .into(),
            "--cpus".into(),
            boot.cpus.to_string().into(),
            "--memory".into(),
            boot.memory_mib.to_string().into(),
            "--vsock".into(),
            vsock.clone().into(),
            "--net".into(),
            format!("4,5,6,{}", mac(&net_cfg.guest_mac)).into(),
            "--no-console".into(),
        ];
        for base in boot.bases {
            args.extend(["--pmem".into(), base.into()]);
        }
        // On macOS the VM is in App Sandbox and asks this process for what it opens.
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
            (region.as_fd(), 4),
            (vm_sleeps.as_fd(), 5),
            (vm_rings.as_fd(), 6),
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
        let vm = match shards_ipc::spawn(&shards_ipc::vm_binary(&exe), &argv, &fds, false) {
            Ok(vm) => vm,
            Err(e) => {
                let _ = net.kill(libc::SIGKILL);
                let _ = net.wait();
                return Err(format!("starting the builder: {e}"));
            }
        };
        // The two processes hold the ring now: once the VM goes, its network process
        // hears its doorbell hang up and goes too.
        drop(fds);
        drop((region, vm_sleeps, vm_rings));
        if let Some((ours, _)) = grants {
            #[cfg(target_os = "macos")]
            std::thread::Builder::new()
                .name("builder grants".into())
                .spawn(move || {
                    let _ = crate::grant_answer::serve(&ours);
                })
                .map_err(|e| format!("answering the builder: {e}"))?;
            #[cfg(not(target_os = "macos"))]
            drop(ours);
        }
        let began = Instant::now();
        let conn = loop {
            match listener.accept() {
                Ok((conn, _)) => break conn,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if began.elapsed() > BOOT_PATIENCE {
                        let _ = vm.kill(libc::SIGKILL);
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
        let mut guard = guard;
        guard.0 = None;
        Ok(Builder {
            vm,
            net,
            conn,
            dir,
            layers: HashMap::new(),
            next: 0,
        })
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
        let mut r = &self.conn;
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
    /// it succeeds, puts what it changed into `applier`. How it ended, and the layer its
    /// changes are in the guest.
    pub fn run(
        &mut self,
        mut step: Step,
        out: &mut dyn FnMut(u8, &[u8]),
        applier: &mut Applier<'_>,
    ) -> Result<(Ended, u32), String> {
        step.upper = self.layer_id();
        self.frame(kind::STEP, &step.encode())?;
        let mut buf = Vec::new();
        let mut not_run: Option<String> = None;
        let status = loop {
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
            return Ok((Ended::NotRun(why), step.upper));
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
        Ok((Ended::Status(status), step.upper))
    }
}

/// A MAC as `--net` takes it.
#[cfg(unix)]
fn mac(m: &[u8; 6]) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// Whether `origin` stands on a base image.
#[cfg(unix)]
fn has_base(origin: &Origin) -> bool {
    match origin {
        Origin::Image(_) => true,
        Origin::Scratch | Origin::Whole { .. } => false,
        Origin::Step { parent, .. } | Origin::Run { parent, .. } => has_base(parent),
        Origin::Merge(parts) => parts.first().is_some_and(|p| has_base(p)),
    }
}

#[cfg(unix)]
/// The snapshot an origin ends in, where it holds one, and the id to send it whole by:
/// another than its own part's, which is its changes alone.
fn whole_of(origin: &Origin) -> Option<(u64, Rc<Fs>)> {
    match origin {
        Origin::Step { id, fs, .. } | Origin::Whole { id, fs } => Some((id ^ (1 << 63), fs.clone())),
        _ => None,
    }
}

#[cfg(unix)]
impl Drop for Builder {
    fn drop(&mut self) {
        // The guest powers off as its connection closes.
        let _ = self.conn.shutdown(std::net::Shutdown::Both);
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.vm.try_wait().is_none() {
            if Instant::now() > deadline {
                let _ = self.vm.kill(libc::SIGKILL);
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // The network process goes with its VM; it is told outright if it lingers.
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.net.try_wait().is_none() {
            if Instant::now() > deadline {
                let _ = self.net.kill(libc::SIGKILL);
                let _ = self.net.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
