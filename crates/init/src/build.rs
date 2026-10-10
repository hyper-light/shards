//! A builder guest: runs a build's `RUN` steps as BuildKit runs them
//! (docs/research/buildkit-run.md), over layers the host sends and the steps leave, and
//! sends back what each step changed (docs/design/architecture.md D34,
//! shards_abi::build).
//!
//! Layers are directories under `/b/l`, written once and stacked by overlayfs, named by
//! the host's ids in base 36 so that a long stack still fits the one page of a mount's
//! options. A step runs on an overlay of its tree with a fresh upper directory, in mount,
//! PID, UTS, IPC, network and cgroup namespaces of its own, its command PID 1 there; its
//! upper directory becomes the layer it names.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use shards_abi::build::{self, Mount, Network, Step, Tree, kind};
use shards_abi::run;

use crate::defaults::{self, CAPS, DEVICES, LINKS, MASKED, READONLY};
use crate::inroot::{self, Root};
use crate::linux::power_off;
use crate::proxyca;
use crate::run::{dial, loopback_up, send};
use crate::tree::{LayerWriter, send_upper};

/// The builder's own root, a tmpfs it moves onto at start: a step's pivot_root needs the
/// root it leaves to be a mount of its own, which the initramfs is not
/// (pivot_root(2), EINVAL), and only pivot_root takes the builder's files out of a step's
/// reach; a chroot leaves them there for a step with CAP_SYS_CHROOT.
const B: &str = "/b";
const LAYERS: &str = "/l";
const ROOT: &str = "/root";
const EMPTY: &str = "/empty";

fn err(what: impl std::fmt::Display) -> io::Error {
    io::Error::other(what.to_string())
}

/// What fails before a step's process starts as BuildKit's executor fails, not its
/// runtime: a mount's source its tree lacks. Said as the step's failure, where what fails
/// as runc's container init fails is said in the step's output, which ends with 1
/// (measured against BuildKit, 2026-10-02).
#[derive(Debug)]
struct Executor(String);

impl std::fmt::Display for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Executor {}

impl Executor {
    fn wrap(why: String) -> io::Error {
        io::Error::other(Executor(why))
    }

    fn is(e: &io::Error) -> bool {
        e.get_ref().is_some_and(|r| r.is::<Executor>())
    }
}

/// How the step's process reports a failure before its command runs: the class, then
/// the text.
const EXECUTOR_FAILED: u8 = b'E';
const INIT_FAILED: u8 = b'I';

fn os_err(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

fn cstr(p: impl AsRef<OsStr>) -> io::Result<CString> {
    CString::new(p.as_ref().as_bytes()).map_err(|_| err("a path holds NUL"))
}

fn mount(source: &str, target: &Path, fstype: &str, flags: libc::c_ulong, data: &str) -> io::Result<()> {
    let (s, t, f, d) = (cstr(source)?, cstr(target)?, cstr(fstype)?, cstr(data)?);
    let src = if source.is_empty() {
        std::ptr::null()
    } else {
        s.as_ptr()
    };
    let fty = if fstype.is_empty() {
        std::ptr::null()
    } else {
        f.as_ptr()
    };
    // SAFETY: NUL-terminated strings or null, which mount(2) takes.
    if unsafe { libc::mount(src, t.as_ptr(), fty, flags, d.as_ptr().cast()) } != 0 {
        return Err(os_err(&format!("mounting {fstype} on {}", target.display())));
    }
    Ok(())
}

fn umount(target: &Path) -> io::Result<()> {
    let t = cstr(target)?;
    // SAFETY: a NUL-terminated path.
    if unsafe { libc::umount2(t.as_ptr(), libc::MNT_DETACH) } != 0 {
        return Err(os_err(&format!("unmounting {}", target.display())));
    }
    Ok(())
}

/// A layer's directory name: its id in base 36.
fn layer_name(id: u32) -> String {
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut n = id;
    let mut out = Vec::new();
    loop {
        out.push(digits.get((n % 36) as usize).copied().unwrap_or(b'0'));
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// The builder: its connection, which bases are mounted, and its count of mounts made.
struct Builder {
    conn: File,
    /// The base images mounted so far, by pmem device.
    bases: HashMap<u32, PathBuf>,
    /// The layer whose stream is arriving, and its writer.
    writing: Option<(u32, LayerWriter)>,
    serial: u64,
    /// The step's SSH agent sockets, listened on here and relayed to the host while it
    /// runs: each mount's listener, the agent's id and the host's token for it.
    ssh: Vec<(std::os::unix::net::UnixListener, Vec<u8>, [u8; 16])>,
}

/// Starts the builder: the host's layers and steps, until it hangs up.
pub fn main() -> ! {
    match serve() {
        Ok(()) => {}
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards-init: building: {e}");
        }
    }
    power_off()
}

fn serve() -> io::Result<()> {
    std::fs::create_dir_all(B)?;
    // Layers live in memory, or on swap once memory runs short.
    mount("tmpfs", Path::new(B), "tmpfs", 0, "mode=0755,size=100%")?;
    for d in [LAYERS, EMPTY, ROOT, "/dev", "/proc"] {
        std::fs::create_dir_all(Path::new(B).join(d.trim_start_matches('/')))?;
    }
    mount("devtmpfs", &Path::new(B).join("dev"), "devtmpfs", 0, "")?;
    mount("proc", &Path::new(B).join("proc"), "proc", 0, "")?;
    // Onto it, as switch_root moves off an initramfs.
    std::env::set_current_dir(B)?;
    mount(".", Path::new("/"), "", libc::MS_MOVE, "")?;
    let dot = cstr(".")?;
    // SAFETY: a NUL-terminated path.
    if unsafe { libc::chroot(dot.as_ptr()) } != 0 {
        return Err(os_err("chroot"));
    }
    std::env::set_current_dir("/")?;
    swap_on()?;
    loopback_up()?;
    // A builder with a network: eth0 as the host named it, which a step with the default
    // network shares, as BuildKit's steps share its host's.
    if let Some((addr, prefix, gateway)) = crate::net::from_cmdline() {
        crate::net::configure(addr, prefix, gateway)?;
    }
    let conn = dial(build::PORT, true)?;
    let mut b = Builder {
        conn,
        bases: HashMap::new(),
        writing: None,
        serial: 0,
        ssh: Vec::new(),
    };
    let mut h = [0u8; run::HEADER];
    let mut payload = Vec::new();
    loop {
        match (&b.conn).read_exact(&mut h) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let Some((which, len)) = run::parse_header(h) else {
            return Err(err("a malformed frame"));
        };
        payload.resize(len as usize, 0);
        (&b.conn).read_exact(&mut payload)?;
        match which {
            kind::LAYER => b.layer(&payload)?,
            kind::STEP => {
                let step = Step::decode(&payload).ok_or_else(|| err("a malformed step"))?;
                b.step(&step)?;
            }
            other => return Err(err(format!("an unknown frame kind {other}"))),
        }
    }
}

/// Swaps to `/dev/vda` if the host gave one, its swap header written: tmpfs pages go
/// there, compressed first by zswap, once memory runs short.
fn swap_on() -> io::Result<()> {
    let dev = Path::new("/dev/vda");
    if !dev.exists() {
        return Ok(());
    }
    let d = cstr(dev)?;
    // SAFETY: a NUL-terminated path.
    if unsafe { libc::swapon(d.as_ptr(), 0) } != 0 {
        return Err(os_err("swapon /dev/vda"));
    }
    Ok(())
}

impl Builder {
    /// The next bytes of a layer's stream.
    fn layer(&mut self, payload: &[u8]) -> io::Result<()> {
        let (id, bytes) = payload
            .split_first_chunk::<4>()
            .ok_or_else(|| err("a layer frame without its id"))?;
        let id = u32::from_be_bytes(*id);
        let dir = Path::new(LAYERS).join(format!("{}.new", layer_name(id)));
        match &mut self.writing {
            Some((current, _)) if *current == id => {}
            Some(_) => return Err(err("a layer begun before the last one ended")),
            None => {
                std::fs::create_dir(&dir)?;
                self.writing = Some((id, LayerWriter::new(dir.clone())));
            }
        }
        if let Some((_, w)) = &mut self.writing {
            w.feed(bytes)?;
            if w.ended()
                && let Some((_, w)) = self.writing.take()
            {
                w.finish()?;
                std::fs::rename(&dir, Path::new(LAYERS).join(layer_name(id)))?;
                send(&self.conn, kind::LAYERED, &[])?;
            }
        }
        Ok(())
    }

    /// The base image on pmem device `n`, mounted once.
    fn base(&mut self, n: u32) -> io::Result<PathBuf> {
        if let Some(p) = self.bases.get(&n) {
            return Ok(p.clone());
        }
        let at = PathBuf::from(format!("/base{n}"));
        std::fs::create_dir_all(&at)?;
        mount(
            &format!("/dev/pmem{n}"),
            &at,
            "erofs",
            libc::MS_RDONLY,
            "dax=always",
        )?;
        self.bases.insert(n, at.clone());
        Ok(at)
    }

    /// Mounts `tree` at `at`, read-only, or writable over a fresh upper directory `upper`.
    fn mount_tree(&mut self, tree: &Tree, at: &Path, upper: Option<&Path>) -> io::Result<()> {
        // Relative to /b/l, so that each layer costs its short name.
        let mut lower: Vec<String> = tree.layers.iter().rev().map(|&l| layer_name(l)).collect();
        if let Some(n) = tree.base {
            lower.push(self.base(n)?.to_string_lossy().into_owned());
        }
        // Without an upper directory overlayfs takes two lower ones at least
        // (fs/overlayfs/super.c, ovl_get_lowerstack): an empty one goes under a tree of
        // one, which keeps what overlayfs hides of it (whiteouts) hidden, as a bind
        // mount of its one directory would not.
        if lower.is_empty() || upper.is_none() && lower.len() == 1 {
            lower.push(EMPTY.into());
        }
        let mut data = format!("lowerdir={}", lower.join(":"));
        if let Some(u) = upper {
            let work = u.with_extension("work");
            std::fs::create_dir_all(u)?;
            std::fs::create_dir_all(&work)?;
            data.push_str(&format!(",upperdir={},workdir={}", u.display(), work.display()));
        }
        // What a layer holds as it was written: no index, no redirects, no metacopy, as
        // BuildKit's differ needs of an upper directory (docs/research/buildkit-run.md §6).
        data.push_str(",index=off,redirect_dir=off,metacopy=off");
        if upper.is_some() {
            // A builder's upper directory is never needed after a crash.
            data.push_str(",volatile");
        }
        if data.len() >= 4096 {
            return Err(err("too many layers for one mount"));
        }
        std::fs::create_dir_all(at)?;
        let flags = if upper.is_some() { 0 } else { libc::MS_RDONLY };
        std::env::set_current_dir(LAYERS)?;
        let mounted = mount("overlay", at, "overlay", flags, &data);
        std::env::set_current_dir("/")?;
        mounted
    }

    fn step(&mut self, step: &Step) -> io::Result<()> {
        self.serial += 1;
        let work = PathBuf::from(format!("/s{}", self.serial));
        let upper = work.join("upper");
        let root = Path::new(ROOT);
        // What fails as the step is set up fails the step, as what fails as its command
        // starts does: the builder says why and goes on to the next.
        let mut sources: Vec<Option<PathBuf>> = Vec::with_capacity(step.mounts.len());
        let mut caches: Vec<PathBuf> = Vec::with_capacity(step.caches.len());
        let mut stubs = Vec::new();
        let mut injected = None;
        let status = match self.prepare(
            step,
            root,
            &work,
            &upper,
            &mut caches,
            &mut sources,
            &mut stubs,
            &mut injected,
        ) {
            Ok(files) => self.run(step, root, &files, &sources, &work),
            Err(e) => Err(e),
        };
        // Its agents' sockets go with it, whether or not it ran.
        self.ssh.clear();
        // The proxy's CA goes before the stubs, as BuildKit's deferred cleanups run.
        if let Some(i) = &injected {
            i.clean(root);
        }
        clean_stubs(root, &stubs);
        let _ = umount(root);
        for (i, s) in sources.iter().enumerate() {
            if let (Some(at), Some((_, Mount::Tree { .. }))) = (s, step.mounts.get(i)) {
                let _ = umount(at);
            }
        }
        for at in &caches {
            let _ = umount(at);
        }
        let status = match status {
            Ok(s) => s,
            Err(e) => {
                send(&self.conn, run::kind::SYSTEM_ERR, e.to_string().as_bytes())?;
                // docker run's status for what never ran.
                125
            }
        };
        send(&self.conn, run::kind::EXIT, &status.to_be_bytes())?;
        if status == 0 {
            let conn = &self.conn;
            send_upper(&upper, &mut |chunk| send(conn, kind::CHANGES, chunk))?;
            std::fs::rename(&upper, Path::new(LAYERS).join(layer_name(step.upper)))?;
            // Each output mount's changes, in order, each kept as its layer (D81).
            for &(i, layer) in &step.outputs {
                let up = work.join(format!("m{i}.upper"));
                send_upper(&up, &mut |chunk| send(conn, kind::MOUNT_CHANGES, chunk))?;
                send(conn, kind::MOUNT_END, &[])?;
                std::fs::rename(&up, Path::new(LAYERS).join(layer_name(layer)))?;
            }
        }
        // Each cache's changes, whatever the status, as BuildKit keeps a cache's writes
        // (D114), and the root they left it: none from a step whose caches were never
        // mounted, its root as it was given.
        for (k, c) in step.caches.iter().enumerate().filter(|(_, c)| c.writable) {
            let up = work.join(format!("c{k}.upper"));
            let left = match std::fs::symlink_metadata(&up) {
                Ok(m) => build::Root {
                    mode: m.mode() & 0o7777,
                    uid: m.uid(),
                    gid: m.gid(),
                },
                Err(_) => {
                    std::fs::create_dir_all(&up)?;
                    c.root
                }
            };
            let conn = &self.conn;
            send_upper(&up, &mut |chunk| send(conn, kind::MOUNT_CHANGES, chunk))?;
            send(conn, kind::MOUNT_END, &left.encode())?;
            std::fs::rename(&up, Path::new(LAYERS).join(layer_name(c.layer)))?;
        }
        let _ = std::fs::remove_dir_all(&work);
        Ok(())
    }

    /// Sets step `step` up in `work`: its root at `root` with its upper directory `upper`,
    /// its caches (each put in `caches` as it is mounted) and the trees it mounts (each in
    /// `sources`, so that what was mounted is unmounted whatever fails), what BuildKit
    /// removes after it if it is left empty (`stubs`), the build's proxy's CA in its trust
    /// bundle (`injected`, D110), its working directory, and its hosts and resolv.conf.
    /// Returns the directory of those two files.
    #[allow(clippy::too_many_arguments)]
    fn prepare(
        &mut self,
        step: &Step,
        root: &Path,
        work: &Path,
        upper: &Path,
        caches: &mut Vec<PathBuf>,
        sources: &mut Vec<Option<PathBuf>>,
        stubs: &mut Vec<Vec<u8>>,
        injected: &mut Option<Injected>,
    ) -> io::Result<PathBuf> {
        std::fs::create_dir_all(work)?;
        self.mount_tree(&step.root, root, Some(upper))?;
        for (k, c) in step.caches.iter().enumerate() {
            let at = work.join(format!("c{k}"));
            let up = c.writable.then(|| work.join(format!("c{k}.upper")));
            // Its root is its upper directory's, as overlayfs shows it: the one it was left
            // with, as containerd gives a snapshot its parent's owner.
            if let Some(up) = &up {
                std::fs::create_dir_all(up)?;
                let p = cstr(up)?;
                // SAFETY: a NUL-terminated path.
                unsafe {
                    if libc::chown(p.as_ptr(), c.root.uid, c.root.gid) != 0
                        || libc::chmod(p.as_ptr(), c.root.mode & 0o7777) != 0
                    {
                        return Err(os_err("a cache's root"));
                    }
                }
            }
            self.mount_tree(&c.tree, &at, up.as_deref())?;
            caches.push(at);
        }
        for (i, (_, m)) in step.mounts.iter().enumerate() {
            let source = match m {
                Mount::Tree { tree, writable, .. } => {
                    let at = work.join(format!("m{i}"));
                    let up = writable.then(|| work.join(format!("m{i}.upper")));
                    self.mount_tree(tree, &at, up.as_deref())?;
                    Some(at)
                }
                // The cache as it is mounted for the step.
                Mount::Cache { cache, .. } => Some(
                    caches
                        .get(*cache as usize)
                        .cloned()
                        .ok_or_else(|| err("a mount of a cache the step has not"))?,
                ),
                // A socket here, which the step's mount places at its target and this
                // process relays to the host's agent (`relay_ssh`).
                Mount::Ssh {
                    id,
                    mode,
                    uid,
                    gid,
                    token,
                } => {
                    let dir = work.join(format!("ssh{i}"));
                    std::fs::create_dir_all(&dir)?;
                    let path = dir.join("agent.sock");
                    let listener = std::os::unix::net::UnixListener::bind(&path)?;
                    listener.set_nonblocking(true)?;
                    let c = cstr(&path)?;
                    // SAFETY: a NUL-terminated path.
                    unsafe {
                        if libc::chown(c.as_ptr(), *uid, *gid) != 0
                            || libc::chmod(c.as_ptr(), mode & 0o777) != 0
                        {
                            return Err(os_err("an SSH agent's socket"));
                        }
                    }
                    self.ssh.push((listener, id.clone(), *token));
                    Some(path)
                }
                _ => None,
            };
            sources.push(source);
        }
        let r = Root::open(root)?;
        *stubs = self::stubs(&r, step);
        *injected = inject_ca(&r, &step.proxy_ca)?;
        // A working directory not there yet, made as BuildKit's executor makes it, in the
        // step's root: each new directory 0755, owned by the step's user (§1).
        if !step.cwd.is_empty() && r.open_at(&step.cwd, libc::O_PATH).is_err() {
            let at = r.resolve(&step.cwd, true).map_err(|e| {
                Executor::wrap(format!(
                    "working dir {} points to invalid target: {e}",
                    String::from_utf8_lossy(&step.cwd)
                ))
            })?;
            r.mkdir_all(&at, 0o755, Some((step.uid, step.gid))).map_err(|e| {
                Executor::wrap(format!(
                    "failed to create working directory {}: {e}",
                    String::from_utf8_lossy(&step.cwd)
                ))
            })?;
        }
        let files = work.join("etc");
        std::fs::create_dir_all(&files)?;
        std::fs::write(files.join("hosts"), &step.hosts)?;
        std::fs::write(files.join("resolv.conf"), &step.resolv)?;
        Ok(files)
    }

    /// Runs the step's command, relaying its output; its status as `docker run` reports
    /// one: its code, or 128 and the signal that ended it.
    fn run(
        &mut self,
        step: &Step,
        root: &Path,
        files: &Path,
        sources: &[Option<PathBuf>],
        work: &Path,
    ) -> io::Result<u32> {
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        // The child's setup failure, if any, as text: closed on its exec.
        let (fail_r, fail_w) = pipe()?;
        // The builder's own network for the default, as BuildKit's host network; loopback
        // alone, in a namespace of the step's own, for none.
        let net = match step.network {
            Network::None => libc::CLONE_NEWNET,
            Network::Default => 0,
        };
        let flags = libc::CLONE_NEWNS
            | libc::CLONE_NEWPID
            | libc::CLONE_NEWUTS
            | libc::CLONE_NEWIPC
            | net
            | libc::CLONE_NEWCGROUP;
        // Its limits, where it has any: a cgroup of its own it starts in, whose namespace
        // is rooted there, as a container's is.
        let cgroup = step_cgroup(&step.cgroup)?;
        let mut args = crate::domains::CloneArgs {
            flags: flags as u64,
            exit_signal: libc::SIGCHLD as u64,
            ..Default::default()
        };
        if let Some((fd, _)) = &cgroup {
            args.flags |= crate::domains::CLONE_INTO_CGROUP;
            args.cgroup = fd.as_raw_fd() as u64;
        }
        // SAFETY: clone3(2) as fork(2) with new namespaces: no new stack, so the child runs
        // on a copy of this one, as fork's does. The child calls the kernel for its IDs and
        // limits (`defaults::take_ids`), not musl, whose thread list is init's.
        let pid = unsafe {
            libc::syscall(
                libc::SYS_clone3,
                &raw mut args,
                std::mem::size_of::<crate::domains::CloneArgs>(),
            )
        };
        if pid < 0 {
            return Err(os_err("starting the step"));
        }
        if pid == 0 {
            drop((out_r, err_r, fail_r));
            let e = child(step, root, files, sources, work, &out_w, &err_w);
            // Reached only if the command did not start.
            let class = if Executor::is(&e) {
                EXECUTOR_FAILED
            } else {
                INIT_FAILED
            };
            let msg = [&[class][..], e.to_string().as_bytes()].concat();
            // SAFETY: a descriptor this process owns.
            unsafe {
                libc::write(fail_w.as_raw_fd(), msg.as_ptr().cast(), msg.len());
                libc::_exit(127);
            }
        }
        drop((out_w, err_w, fail_w));
        // Its SSH agents relayed while it runs, on threads of this process's own, begun
        // only now that it has forked, so that none held a lock its child would need.
        let ssh = std::mem::take(&mut self.ssh);
        // The step's end wakes the relays (`wake`), which take no more connections.
        let (wake, woken) = std::os::unix::net::UnixStream::pair()?;
        let mut status = 0;
        let waited = std::thread::scope(|scope| {
            for (listener, id, token) in &ssh {
                let woken = &woken;
                let _ = std::thread::Builder::new()
                    .name("ssh relay".into())
                    .spawn_scoped(scope, move || relay_ssh(scope, listener, id, token, woken));
            }
            let relayed = self.relay(out_r, err_r);
            // SAFETY: waits for our own child.
            let waited = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
            drop(wake);
            relayed.map(|()| waited)
        })?;
        if waited < 0 {
            return Err(os_err("waiting for the step"));
        }
        // Its cgroup, empty once it is reaped.
        if let Some((fd, dir)) = cgroup {
            drop(fd);
            let _ = std::fs::remove_dir(&dir);
        }
        let mut msg = Vec::new();
        File::from(fail_r).read_to_end(&mut msg)?;
        match msg.split_first() {
            Some((&EXECUTOR_FAILED, why)) => return Err(err(String::from_utf8_lossy(why))),
            Some((_, why)) => {
                // As BuildKit shows what runc's container init could not do: in the
                // step's output, then exit code 1.
                let line = [
                    &b"unable to start container process: error during container init: "[..],
                    why,
                    b"\n",
                ]
                .concat();
                send(&self.conn, run::kind::STDERR, &line)?;
                return Ok(1);
            }
            None => {}
        }
        Ok(if libc::WIFSIGNALED(status) {
            128 + libc::WTERMSIG(status) as u32
        } else {
            libc::WEXITSTATUS(status) as u32
        })
    }

    /// Sends what the step writes, as it writes it, until both its outputs close.
    fn relay(&self, out: OwnedFd, err_fd: OwnedFd) -> io::Result<()> {
        let mut open = [Some(out), Some(err_fd)];
        let mut buf = vec![0u8; 64 * 1024];
        while open.iter().any(Option::is_some) {
            let mut fds: Vec<libc::pollfd> = open
                .iter()
                .map(|f| libc::pollfd {
                    fd: f.as_ref().map_or(-1, AsRawFd::as_raw_fd),
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
            // SAFETY: an array of pollfds of the length given.
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            for (i, p) in fds.iter().enumerate() {
                if p.revents == 0 {
                    continue;
                }
                let Some(Some(f)) = open.get(i) else { continue };
                // SAFETY: a buffer of the length given, into a descriptor we own.
                let n = unsafe { libc::read(f.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n > 0 {
                    let which = if i == 0 {
                        run::kind::STDOUT
                    } else {
                        run::kind::STDERR
                    };
                    send(&self.conn, which, buf.get(..n as usize).unwrap_or_default())?;
                } else if (n == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted)
                    && let Some(slot) = open.get_mut(i)
                {
                    *slot = None;
                }
            }
        }
        Ok(())
    }
}

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: an array of two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(os_err("pipe"));
    }
    // SAFETY: two descriptors just made.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// The build's proxy's CA as it went into a step's trust bundle (D110): the bundle, a path
/// of the step's root, and the certificate, to take it out again after the step.
#[derive(Debug)]
struct Injected {
    path: Vec<u8>,
    der: Vec<u8>,
}

impl Injected {
    /// InjectProxyCA's cleanup: the bundle without the CA, if it still holds it, the step's
    /// own changes to it kept; read and written in the root, whatever the step made of
    /// the paths above it. As BuildKit's, its failure fails nothing.
    fn clean(&self, root: &Path) {
        let Ok(r) = Root::open(root) else {
            return;
        };
        let Ok((current, st)) = read_bundle(&r, &self.path) else {
            return;
        };
        let cleaned = proxyca::removed(&current, &self.der);
        if cleaned != current {
            let _ = write_bundle(&r, &self.path, &cleaned, &st);
        }
    }
}

/// InjectProxyCA: the PEM `ca` appended to the first of the root's trust bundles that is a
/// regular file (each found as fs.RootPath finds it, in the root), unless that bundle holds
/// its certificate already. None where `ca` is empty, the root has no bundle, or it holds
/// the CA.
fn inject_ca(r: &Root, ca: &[u8]) -> io::Result<Option<Injected>> {
    if ca.is_empty() {
        return Ok(None);
    }
    let der = proxyca::first_certificate(ca).ok_or_else(|| err(proxyca::NO_CERTIFICATE))?;
    let mut found = None;
    for name in proxyca::BUNDLES {
        let path = r.resolve(name, false).map_err(|e| {
            err(format!(
                "failed to resolve certificate bundle {}: {e}",
                String::from_utf8_lossy(name)
            ))
        })?;
        let regular = r
            .open_at(&path, libc::O_PATH)
            .and_then(|fd| fstat(&fd))
            .is_ok_and(|st| st.st_mode & libc::S_IFMT == libc::S_IFREG);
        if regular {
            found = Some(path);
            break;
        }
    }
    let Some(path) = found else {
        return Ok(None);
    };
    let (original, st) = read_bundle(r, &path)?;
    if proxyca::contains(&original, &der) {
        return Ok(None);
    }
    write_bundle(r, &path, &proxyca::appended(&original, ca), &st)?;
    Ok(Some(Injected { path, der }))
}

fn fstat(fd: &OwnedFd) -> io::Result<libc::stat> {
    // SAFETY: a zeroed stat is valid for fstat to fill.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: fstat(2) of a descriptor of ours.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

/// readCertBundle: the bundle at `path` of the root, a regular file of at most
/// [`proxyca::MOST`] bytes, and its stat. Opened without blocking, so a FIFO there is refused,
/// never waited on.
fn read_bundle(r: &Root, path: &[u8]) -> io::Result<(Vec<u8>, libc::stat)> {
    let shown = String::from_utf8_lossy(path);
    let fd = r.open_at(path, libc::O_RDONLY | libc::O_NONBLOCK)?;
    let st = fstat(&fd)?;
    if st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(err(format!("{shown} is not a regular file")));
    }
    let mut data = Vec::new();
    File::from(fd).take(proxyca::MOST + 1).read_to_end(&mut data)?;
    if data.len() as u64 > proxyca::MOST {
        return Err(err(format!("{shown} exceeds {} bytes", proxyca::MOST)));
    }
    Ok((data, st))
}

/// writeCertBundle: `data` written to a new file beside the bundle (os.CreateTemp's
/// `.buildkit-ca-*`), given the bundle's mode and owner, then renamed over it.
fn write_bundle(r: &Root, path: &[u8], data: &[u8], st: &libc::stat) -> io::Result<()> {
    let (dir, name) = r.parent(path)?;
    // A name of its own: tried again where one is taken, as CreateTemp tries.
    let mut tries = 0u32;
    let (tmp, file) = loop {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let tmp = CString::new(format!(
            ".buildkit-ca-{}",
            nanos ^ tries.wrapping_mul(2_654_435_761)
        ))
        .map_err(|_| err("a temporary name"))?;
        // SAFETY: openat(2) of a new name in a directory of ours, never followed.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                tmp.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd >= 0 {
            // SAFETY: a descriptor just opened, ours alone.
            break (tmp, File::from(unsafe { OwnedFd::from_raw_fd(fd) }));
        }
        let e = io::Error::last_os_error();
        tries += 1;
        if e.raw_os_error() != Some(libc::EEXIST) || tries >= 10_000 {
            return Err(e);
        }
    };
    let written = (|| {
        let mut f = &file;
        f.write_all(data)?;
        // SAFETY: fchmod(2) and fchown(2) of a descriptor of ours.
        unsafe {
            if libc::fchmod(file.as_raw_fd(), st.st_mode & 0o7777) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::fchown(file.as_raw_fd(), st.st_uid, st.st_gid) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    })();
    drop(file);
    let renamed = written.and_then(|()| {
        // SAFETY: renameat(2) of a name in a directory of ours over another there.
        if unsafe { libc::renameat(dir.as_raw_fd(), tmp.as_ptr(), dir.as_raw_fd(), name.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    });
    if renamed.is_err() {
        // SAFETY: unlinkat(2) of the name made above.
        unsafe { libc::unlinkat(dir.as_raw_fd(), tmp.as_ptr(), 0) };
    }
    renamed
}

/// The step's paths that do not exist yet, with each missing parent, leaf first, as paths
/// of its root: what BuildKit removes after the step if the step left it empty
/// (executor/stubs.go).
fn stubs(r: &Root, step: &Step) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let targets = [&b"/etc/resolv.conf"[..], b"/etc/hosts"]
        .into_iter()
        .chain(step.mounts.iter().map(|(t, _)| t.as_slice()));
    for t in targets {
        // The stubs of the path the target resolves to, as BuildKit's cleaner finds them.
        let Ok(t) = r.resolve(t, false) else {
            continue;
        };
        let mut p: Vec<u8> = t.iter().copied().skip_while(|&b| b == b'/').collect();
        while p.last() == Some(&b'/') {
            p.pop();
        }
        // Each parent, until one is there, itself not followed (lstat).
        while !p.is_empty() && r.open_at(&p, libc::O_PATH | libc::O_NOFOLLOW).is_err() {
            out.push(p.clone());
            match p.iter().rposition(|&b| b == b'/') {
                Some(i) => p.truncate(i),
                None => break,
            }
        }
    }
    out
}

/// Removes each stub the step left empty, restoring its parent's times. Each is reached
/// in the step's root, whatever symlinks the step made of the paths above it.
fn clean_stubs(root: &Path, stubs: &[Vec<u8>]) {
    let Ok(r) = Root::open(root) else {
        return;
    };
    for p in stubs {
        let Ok((dir, name)) = r.parent(p) else {
            continue;
        };
        // SAFETY: a zeroed stat is valid for fstatat to fill.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fstatat(2) of a name in a directory of ours, not followed.
        if unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            continue;
        }
        let is_dir = st.st_mode & libc::S_IFMT == libc::S_IFDIR;
        let empty = if is_dir {
            // SAFETY: openat(2) of that directory, never followed.
            let fd = unsafe {
                libc::openat(
                    dir.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                continue;
            }
            // SAFETY: a descriptor just opened, ours alone.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            std::fs::read_dir(inroot::path(&fd))
                .map(|mut d| d.next().is_none())
                .unwrap_or(false)
        } else {
            st.st_size == 0
        };
        if !empty {
            continue;
        }
        // The parent's times, kept across the removal.
        let above = match p.iter().rposition(|&b| b == b'/') {
            Some(i) => p.get(..i).unwrap_or_default(),
            None => &[][..],
        };
        let parent = r.open_at(above, libc::O_RDONLY | libc::O_DIRECTORY).ok();
        let times = parent.as_ref().and_then(|f| {
            // SAFETY: a zeroed stat is valid for fstat to fill.
            let mut pst: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: fstat(2) of a descriptor of ours.
            (unsafe { libc::fstat(f.as_raw_fd(), &mut pst) } == 0).then_some(pst)
        });
        let flag = if is_dir { libc::AT_REMOVEDIR } else { 0 };
        // SAFETY: unlinkat(2) of a name in a directory of ours.
        let removed = unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flag) } == 0;
        if removed && let (Some(f), Some(t)) = (&parent, times) {
            let ts = [
                libc::timespec {
                    tv_sec: t.st_atime,
                    tv_nsec: t.st_atime_nsec as libc::c_long,
                },
                libc::timespec {
                    tv_sec: t.st_mtime,
                    tv_nsec: t.st_mtime_nsec as libc::c_long,
                },
            ];
            // SAFETY: futimens(2) of a descriptor of ours with two timespecs.
            unsafe { libc::futimens(f.as_raw_fd(), ts.as_ptr()) };
        }
    }
}

/// The step's process, PID 1 of its namespaces: sets up its root and becomes the
/// command. Returns only why it could not.
fn child(
    step: &Step,
    root: &Path,
    files: &Path,
    sources: &[Option<PathBuf>],
    work: &Path,
    out: &OwnedFd,
    err_fd: &OwnedFd,
) -> io::Error {
    match setup(step, root, files, sources, work).and_then(|()| exec(step, out, err_fd)) {
        Ok(never) => match never {},
        Err(e) => e,
    }
}

enum Never {}

fn setup(step: &Step, root: &Path, files: &Path, sources: &[Option<PathBuf>], work: &Path) -> io::Result<()> {
    let (nosuid, noexec, nodev) = (libc::MS_NOSUID, libc::MS_NOEXEC, libc::MS_NODEV);
    // What this namespace mounts stays in it.
    mount("", Path::new("/"), "", libc::MS_REC | libc::MS_PRIVATE, "")?;
    // SAFETY: umask(2) always succeeds.
    unsafe { libc::umask(0) };
    // Every path of the step's root is resolved in it (inroot.rs): the tree is the step's
    // own and an earlier step's, which may hold any symlink.
    let r = Root::open(root)?;
    let dir = |p: &str| r.mkdir_all(p.as_bytes(), 0o755, None);
    let proc = dir("/proc")?;
    mount("proc", &inroot::path(&proc), "proc", nosuid | noexec | nodev, "")?;
    let dev = dir("/dev")?;
    mount(
        "tmpfs",
        &inroot::path(&dev),
        "tmpfs",
        nosuid | libc::MS_STRICTATIME,
        "mode=755,size=65536k",
    )?;
    // The new tmpfs, as /dev now resolves.
    let dev = r.open_at(b"/dev", libc::O_PATH | libc::O_DIRECTORY)?;
    for (name, major, minor) in DEVICES {
        inroot::mknodat(&dev, name, libc::S_IFCHR | 0o666, major, minor)?;
    }
    if step.insecure {
        // What BuildKit adds outside a user namespace with security.insecure (§4).
        let more: &[(&str, u32, u32)] = &[
            ("kmsg", 1, 11),
            ("cuse", 10, 203),
            ("fuse", 10, 229),
            ("kvm", 10, 232),
            ("loop-control", 10, 237),
        ];
        for (name, major, minor) in more {
            inroot::mknodat(&dev, name, libc::S_IFCHR | 0o660, *major, *minor)?;
        }
        inroot::mkdirat(&dev, "net", 0o755)?;
        let net = r.open_at(b"/dev/net", libc::O_PATH | libc::O_DIRECTORY)?;
        inroot::mknodat(&net, "tun", libc::S_IFCHR | 0o660, 10, 200)?;
    }
    for (target, link) in LINKS {
        inroot::symlinkat(target, &dev, link)?;
    }
    if Path::new("/proc/kcore").exists() {
        inroot::symlinkat("/proc/kcore", &dev, "core")?;
    }
    for d in ["pts", "shm", "mqueue"] {
        inroot::mkdirat(&dev, d, 0o755)?;
    }
    let under = |p: &[u8]| r.open_at(p, libc::O_PATH | libc::O_DIRECTORY);
    mount(
        "devpts",
        &inroot::path(&under(b"/dev/pts")?),
        "devpts",
        nosuid | noexec,
        "newinstance,ptmxmode=0666,mode=0620,gid=5",
    )?;
    mount(
        "shm",
        &inroot::path(&under(b"/dev/shm")?),
        "tmpfs",
        nosuid | noexec | nodev,
        "mode=1777,size=65536k",
    )?;
    mount(
        "mqueue",
        &inroot::path(&under(b"/dev/mqueue")?),
        "mqueue",
        nosuid | noexec | nodev,
        "",
    )?;
    // The step's CDI devices (D96), each the builder's own device of its path.
    for d in &step.devices {
        cdi_node(&r, d)?;
    }
    let ro = if step.insecure { 0 } else { libc::MS_RDONLY };
    let sys = dir("/sys")?;
    mount(
        "sysfs",
        &inroot::path(&sys),
        "sysfs",
        nosuid | noexec | nodev | ro,
        "",
    )?;
    if let Ok(cgroup) = under(b"/sys/fs/cgroup") {
        mount(
            "cgroup2",
            &inroot::path(&cgroup),
            "cgroup2",
            nosuid | noexec | nodev | ro,
            "",
        )?;
    }
    // /etc/hosts and /etc/resolv.conf over the tree's, read-only, in no layer (§2).
    let bind_ro = nosuid | noexec | nodev;
    for name in ["hosts", "resolv.conf"] {
        let target = r.resolve(format!("/etc/{name}").as_bytes(), false)?;
        let at = r.file(&target, 0o666)?;
        bind(&files.join(name), &at, &r, &target, bind_ro, true)?;
    }
    // The step's own mounts, in order.
    for (i, (target, m)) in step.mounts.iter().enumerate() {
        match m {
            Mount::Tree { subpath, .. } => {
                let Some(Some(tree)) = sources.get(i) else {
                    return Err(err("a tree mount without its tree"));
                };
                // Its source in its own tree, as BuildKit resolves it there; one missing is
                // BuildKit's executor's failure, before any process starts.
                let src = Root::open(tree)?
                    .open_at(subpath, libc::O_PATH)
                    .map_err(|e| Executor::wrap(inroot::path_error("open", subpath, &e).to_string()))?;
                let target = &r.resolve(target, false)?;
                let at = if inroot::is_dir(&src)? {
                    r.mkdir_all(target, 0o755, None)?
                } else {
                    r.file(target, 0o666)?
                };
                // Readonly unless writable; a writable tree's writes go to its own upper.
                bind(&inroot::path(&src), &at, &r, target, 0, false)?;
            }
            Mount::Cache {
                subpath, readonly, ..
            } => {
                let Some(Some(tree)) = sources.get(i) else {
                    return Err(err("a cache mount without its cache"));
                };
                let src = Root::open(tree)?
                    .open_at(subpath, libc::O_PATH)
                    .map_err(|e| Executor::wrap(inroot::path_error("open", subpath, &e).to_string()))?;
                let target = &r.resolve(target, false)?;
                let at = if inroot::is_dir(&src)? {
                    r.mkdir_all(target, 0o755, None)?
                } else {
                    r.file(target, 0o666)?
                };
                bind(&inroot::path(&src), &at, &r, target, 0, *readonly)?;
            }
            Mount::Tmpfs { size, readonly } => {
                let at = r.mkdir_all(&r.resolve(target, false)?, 0o755, None)?;
                let data = if *size > 0 {
                    format!("size={size}")
                } else {
                    String::new()
                };
                mount(
                    "tmpfs",
                    &inroot::path(&at),
                    "tmpfs",
                    nosuid | if *readonly { libc::MS_RDONLY } else { 0 },
                    &data,
                )?;
            }
            Mount::Ssh { mode, .. } => {
                let sock = sources
                    .get(i)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| err("an SSH agent's socket was not made"))?;
                let target = &r.resolve(target, false)?;
                let at = r.file(target, 0o666)?;
                let exec_bit = if mode & 0o111 == 0 { noexec } else { 0 };
                bind(sock, &at, &r, target, nosuid | nodev | exec_bit, true)?;
            }
            Mount::Secret { data, mode, uid, gid } => {
                let holder = work.join(format!("secret{i}"));
                std::fs::create_dir_all(&holder)?;
                mount("tmpfs", &holder, "tmpfs", nosuid | nodev, "mode=0711")?;
                let file = holder.join("secret");
                let mut f = File::options().write(true).create_new(true).open(&file)?;
                f.write_all(data)?;
                drop(f);
                let c = cstr(&file)?;
                // SAFETY: a NUL-terminated path.
                unsafe {
                    if libc::chown(c.as_ptr(), *uid, *gid) != 0 || libc::chmod(c.as_ptr(), mode & 0o777) != 0
                    {
                        return Err(os_err("a secret's file"));
                    }
                }
                let target = &r.resolve(target, false)?;
                let at = r.file(target, 0o666)?;
                let exec_bit = if mode & 0o111 == 0 { noexec } else { 0 };
                bind(&file, &at, &r, target, nosuid | nodev | exec_bit, true)?;
            }
        }
    }
    if !step.insecure {
        for p in MASKED {
            match r.open_at(p.as_bytes(), libc::O_PATH) {
                Ok(at) if inroot::is_dir(&at)? => {
                    mount("tmpfs", &inroot::path(&at), "tmpfs", libc::MS_RDONLY, "")?
                }
                Ok(at) => bind(Path::new("/dev/null"), &at, &r, p.as_bytes(), 0, false)?,
                Err(_) => {}
            }
        }
        for p in READONLY {
            if let Ok(at) = r.open_at(p.as_bytes(), libc::O_PATH) {
                bind(
                    &inroot::path(&at),
                    &at,
                    &r,
                    p.as_bytes(),
                    nosuid | noexec | nodev,
                    true,
                )?;
            }
        }
    }
    // The root becomes the step's own: what was above it is gone from this namespace.
    std::env::set_current_dir(root)?;
    let dot = cstr(".")?;
    // SAFETY: pivot_root(".", ".") stacks the old root under the new, which the detach
    // below then removes (pivot_root(2), NOTES).
    if unsafe { libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), dot.as_ptr()) } != 0 {
        return Err(os_err("pivot_root"));
    }
    umount(Path::new("."))?;
    std::env::set_current_dir("/")?;
    // SAFETY: a buffer of the length given.
    if unsafe { libc::sethostname(step.hostname.as_ptr().cast(), step.hostname.len()) } != 0 {
        return Err(os_err("sethostname"));
    }
    if step.network == Network::None {
        loopback_up()?;
    }
    for &(resource, soft, hard) in &step.rlimits {
        if !defaults::rlimit(resource as _, soft, hard) {
            return Err(os_err("setting a ulimit"));
        }
    }
    // The working directory, as runc's init makes it, in the step's root, now `/`.
    let cwd: &[u8] = if step.cwd.is_empty() { b"/" } else { &step.cwd };
    Root::open(Path::new("/"))?.mkdir_all(cwd, 0o755, None)?;
    confine(step)?;
    identity(step)?;
    // SAFETY: umask(2) always succeeds. runc's default (rootfs_linux.go).
    unsafe { libc::umask(0o022) };
    let c = cstr(OsStr::from_bytes(cwd))?;
    // SAFETY: a NUL-terminated path.
    if unsafe { libc::chdir(c.as_ptr()) } != 0 {
        let e = io::Error::last_os_error();
        let why = e
            .raw_os_error()
            .map_or_else(|| e.to_string(), shards_cmdline::go::linux_error);
        return Err(io::Error::new(
            e.kind(),
            format!(
                "chdir to cwd ({}) failed: {why}",
                shards_cmdline::go::quote(&String::from_utf8_lossy(cwd))
            ),
        ));
    }
    Ok(())
}

/// Binds `src` over `at`, the resolved target `path` of root `r`, then applies `flags`,
/// read-only if `readonly`: remounted at `path` resolved again, which now reaches the
/// bind mount, where `at` still names what it covers.
fn bind(
    src: &Path,
    at: &OwnedFd,
    r: &Root,
    path: &[u8],
    flags: libc::c_ulong,
    readonly: bool,
) -> io::Result<()> {
    mount(
        &src.to_string_lossy(),
        &inroot::path(at),
        "",
        libc::MS_BIND | libc::MS_REC,
        "",
    )?;
    let ro = if readonly { libc::MS_RDONLY } else { 0 };
    if flags != 0 || readonly {
        let mounted = r.open_at(path, libc::O_PATH)?;
        mount(
            "",
            &inroot::path(&mounted),
            "",
            libc::MS_BIND | libc::MS_REMOUNT | flags | ro,
            "",
        )?;
    }
    Ok(())
}

/// Relays each connection to `listener`, an SSH agent socket of the step's, to the host
/// (shards_abi::build::SSH_PORT), opened with the mount's `token` and the agent's `id`,
/// until `woken` says the step has ended (its other end closed); each connection's bytes
/// both ways, on threads of `scope`.
fn relay_ssh<'s, 'e>(
    scope: &'s std::thread::Scope<'s, 'e>,
    listener: &'e std::os::unix::net::UnixListener,
    id: &'e [u8],
    token: &'e [u8; 16],
    woken: &'e std::os::unix::net::UnixStream,
) {
    loop {
        let mut polled = [listener.as_raw_fd(), woken.as_raw_fd()].map(|fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        });
        // SAFETY: poll(2) on two descriptors of ours, until either has something.
        if unsafe { libc::poll(polled.as_mut_ptr(), 2, -1) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if polled[1].revents != 0 {
            return;
        }
        let Ok((conn, _)) = listener.accept() else {
            continue;
        };
        let _ = std::thread::Builder::new()
            .name("ssh conn".into())
            .spawn_scoped(scope, move || {
                let _ = relay_ssh_conn(conn, id, token);
            });
    }
}

/// One agent connection: the host dialled, the mount's token and the agent's id said,
/// then the bytes each way until either side ends.
fn relay_ssh_conn(conn: std::os::unix::net::UnixStream, id: &[u8], token: &[u8; 16]) -> io::Result<()> {
    conn.set_nonblocking(false)?;
    let mut host = crate::run::dial(build::SSH_PORT, true)?;
    let len = u16::try_from(id.len()).map_err(|_| err("an SSH agent id too long"))?;
    host.write_all(&[token.as_slice(), &len.to_be_bytes(), id].concat())?;
    let mut to_host = host.try_clone()?;
    let mut from_step = conn.try_clone()?;
    std::thread::scope(|s| {
        let _ = std::thread::Builder::new()
            .name("ssh up".into())
            .spawn_scoped(s, || {
                let _ = io::copy(&mut from_step, &mut to_host);
                // SAFETY: shutdown(2) of a descriptor this thread owns a clone of.
                unsafe { libc::shutdown(to_host.as_raw_fd(), libc::SHUT_WR) };
            });
        let mut to_step = &conn;
        let _ = io::copy(&mut host, &mut to_step);
        let _ = conn.shutdown(std::net::Shutdown::Write);
    });
    Ok(())
}

/// The step's seccomp filter, loaded while the step can still load one: runc loads a
/// container's before it changes user where no_new_privs is unset (libcontainer
/// standard_init_linux.go), and BuildKit sets none. What init does after it, the change of
/// user and capabilities and the exec, the default profile allows.
fn confine(step: &Step) -> io::Result<()> {
    if step.seccomp.is_empty() {
        return Ok(());
    }
    let entry = [b"seccomp=".as_slice(), &step.seccomp].concat();
    let (flags, program) = crate::setup::filter(std::slice::from_ref(&entry))
        .ok_or_else(|| err("a seccomp filter cut short"))?;
    let prog = libc::sock_fprog {
        len: u16::try_from(program.len()).map_err(|_| err("a seccomp filter too long"))?,
        filter: program.as_ptr().cast_mut(),
    };
    // SAFETY: seccomp(2) with a program that outlives the call.
    if unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            flags,
            &raw const prog,
        )
    } != 0
    {
        return Err(os_err(
            "error loading seccomp filter into kernel: error loading seccomp filter",
        ));
    }
    Ok(())
}

/// Groups, gid and uid, then the capabilities BuildKit gives a step, as runc applies them:
/// the bounding set first, the others kept across the change of user.
fn identity(step: &Step) -> io::Result<()> {
    let last = defaults::last_cap();
    let keep = |c: u32| step.insecure || CAPS.contains(&c);
    if !defaults::bound(last, keep) {
        return Err(os_err("dropping a capability"));
    }
    // SAFETY: prctl(2) with constant arguments.
    if unsafe { libc::prctl(libc::PR_SET_KEEPCAPS, 1, 0, 0, 0) } != 0 {
        return Err(os_err("keeping capabilities"));
    }
    if !defaults::take_ids(&step.groups, step.gid, step.uid) {
        return Err(os_err("taking the step's IDs"));
    }
    // SAFETY: prctl(2) with constant arguments.
    unsafe { libc::prctl(libc::PR_SET_KEEPCAPS, 0, 0, 0, 0) };
    // For any user but root, execve then leaves none but the bounding set, there being no
    // file or ambient capabilities (capabilities(7)): as BuildKit's steps have it (CapPrm
    // and CapEff 0 for USER 1000:1000 and for nobody, Docker Desktop's BuildKit,
    // 2026-10-02).
    if !defaults::set(last, keep) {
        return Err(os_err("capset"));
    }
    Ok(())
}

/// Becomes the command, its stdin /dev/null, its stdout and stderr the pipes; looks it up
/// in the step's `PATH` as runc does when it holds no slash.
fn exec(step: &Step, out: &OwnedFd, err_fd: &OwnedFd) -> io::Result<Never> {
    let null = File::open("/dev/null")?;
    // SAFETY: dup2(2) onto the standard descriptors.
    unsafe {
        if libc::dup2(null.as_raw_fd(), 0) < 0
            || libc::dup2(out.as_raw_fd(), 1) < 0
            || libc::dup2(err_fd.as_raw_fd(), 2) < 0
        {
            return Err(os_err("dup2"));
        }
    }
    let argv: Vec<CString> = step
        .argv
        .iter()
        .map(|a| CString::new(a.clone()).map_err(|_| err("an argument holds NUL")))
        .collect::<io::Result<_>>()?;
    let env: Vec<CString> = step
        .env
        .iter()
        .filter_map(|e| CString::new(e.clone()).ok())
        .collect();
    let first = argv.first().ok_or_else(|| err("no command"))?;
    let path = lookup(first.as_bytes(), &step.env)?;
    let mut av: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    av.push(std::ptr::null());
    let mut ev: Vec<*const libc::c_char> = env.iter().map(|e| e.as_ptr()).collect();
    ev.push(std::ptr::null());
    // SAFETY: NUL-terminated strings in null-terminated arrays.
    unsafe { libc::execve(path.as_ptr(), av.as_ptr(), ev.as_ptr()) };
    // runc's system.Exec: a Go *PathError of the path it found.
    let e = io::Error::last_os_error();
    let why = e
        .raw_os_error()
        .map_or_else(|| e.to_string(), shards_cmdline::go::linux_error);
    Err(io::Error::new(
        e.kind(),
        format!("exec {}: {why}", path.to_string_lossy()),
    ))
}

/// Where a command is, as Go's exec.LookPath finds it for runc (os/exec/lp_unix.go, Go
/// 1.26.1): itself if its name holds a slash, else the first file in `PATH` that
/// findExecutable accepts, refused if that is found through a relative entry (ErrDot);
/// its errors as LookPath's: `exec: "name": why`.
fn lookup(name: &[u8], env: &[Vec<u8>]) -> io::Result<CString> {
    let fail = |why: &str| {
        err(format!(
            "exec: {}: {why}",
            shards_cmdline::go::quote(&String::from_utf8_lossy(name))
        ))
    };
    if name.contains(&b'/') {
        return match find_executable(name) {
            Ok(()) => CString::new(name).map_err(|_| err("a path holds NUL")),
            Err(why) => Err(fail(&why)),
        };
    }
    let path = env
        .iter()
        .rev()
        .find_map(|e| e.strip_prefix(b"PATH="))
        .unwrap_or_default();
    // An empty PATH has no entries (Go's filepath.SplitList): nothing is found in it.
    for dir in path.split(|&b| b == b':').filter(|_| !path.is_empty()) {
        // Unix shell semantics: an empty element means ".".
        let dir: &[u8] = if dir.is_empty() { b"." } else { dir };
        let mut p = dir.to_vec();
        p.push(b'/');
        p.extend_from_slice(name);
        if find_executable(&p).is_ok() {
            if !p.starts_with(b"/") {
                return Err(fail("cannot run executable found relative to current directory"));
            }
            return CString::new(p).map_err(|_| err("a path holds NUL"));
        }
    }
    Err(fail("executable file not found in $PATH"))
}

/// findExecutable: `file` is no directory, and the effective user may execute it
/// (faccessat2(2) AT_EACCESS, its mode bits where that is refused), or why not, in Go's
/// words.
fn find_executable(file: &[u8]) -> Result<(), String> {
    let go = |e: &io::Error| {
        e.raw_os_error()
            .map_or_else(|| e.to_string(), shards_cmdline::go::linux_error)
    };
    let shown = String::from_utf8_lossy(file);
    let meta = std::fs::metadata(OsStr::from_bytes(file)).map_err(|e| format!("stat {shown}: {}", go(&e)))?;
    if meta.is_dir() {
        return Err(shards_cmdline::go::linux_error(libc::EISDIR));
    }
    let c = CString::new(file).map_err(|_| "a path holds NUL".to_string())?;
    // SAFETY: faccessat2(2) of a NUL-terminated path, for the effective IDs.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_faccessat2,
            libc::AT_FDCWD,
            c.as_ptr(),
            libc::X_OK,
            libc::AT_EACCESS,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOSYS | libc::EPERM) if meta.mode() & 0o111 != 0 => Ok(()),
        Some(libc::ENOSYS | libc::EPERM) => Err(shards_cmdline::go::linux_error(libc::EACCES)),
        _ => Err(go(&e)),
    }
}

/// A step's own cgroup, under the builder's `steps`, with its limits written in order
/// (`Step::cgroup`, runc's files): open, and where it is; none for a step without limits.
/// The controllers its files need are enabled on the way down. A swap file absent, as
/// where the kernel has no swap, is skipped for "max" or "0", as runc skips it.
fn step_cgroup(files: &[(Vec<u8>, Vec<u8>)]) -> io::Result<Option<(OwnedFd, PathBuf)>> {
    use std::os::unix::fs::OpenOptionsExt;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if files.is_empty() {
        return Ok(None);
    }
    let root = Path::new("/sys/fs/cgroup");
    // The builder's own cgroup2, mounted by the first step with limits.
    if !root.join("cgroup.controllers").exists() {
        std::fs::create_dir_all(root)?;
        // SAFETY: mount(2) of cgroup2 with NUL-terminated literals.
        let rc = unsafe {
            libc::mount(
                c"cgroup2".as_ptr(),
                c"/sys/fs/cgroup".as_ptr(),
                c"cgroup2".as_ptr(),
                libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
                std::ptr::null(),
            )
        };
        if rc != 0 {
            return Err(err(format!(
                "mounting the builder's cgroup2: {}",
                io::Error::last_os_error()
            )));
        }
    }
    let steps = root.join("steps");
    let enable = |dir: &Path| std::fs::write(dir.join("cgroup.subtree_control"), "+memory +cpu +cpuset");
    enable(root).map_err(|e| err(format!("enabling the builder's cgroup controllers: {e}")))?;
    match std::fs::create_dir(&steps) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(err(format!("{}: {e}", steps.display()))),
    }
    enable(&steps).map_err(|e| err(format!("{}: enabling controllers: {e}", steps.display())))?;
    let dir = steps.join(
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_string(),
    );
    std::fs::create_dir(&dir).map_err(|e| err(format!("{}: {e}", dir.display())))?;
    for (file, value) in files {
        let name = String::from_utf8_lossy(file).into_owned();
        match std::fs::write(dir.join(&name), value) {
            Ok(()) => {}
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    && name == "memory.swap.max"
                    && matches!(value.as_slice(), b"max" | b"0") => {}
            Err(e) => {
                let _ = std::fs::remove_dir(&dir);
                return Err(Executor::wrap(format!(
                    "failed to write {}: {e}",
                    String::from_utf8_lossy(value)
                )));
            }
        }
    }
    let fd = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(&dir)?;
    Ok(Some((OwnedFd::from(fd), dir)))
}

/// A CDI device node in the step's root: the builder's own device at `from` (or its path),
/// of the kind asked for, or a FIFO; its mode the device's unless given, its owner given.
fn cdi_node(r: &Root, d: &shards_abi::build::Device) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
    let shown = String::from_utf8_lossy(&d.path).into_owned();
    let (kind, rdev, mode) = if d.kind == b'p' {
        (libc::S_IFIFO, 0, d.mode.unwrap_or(0o666))
    } else {
        let from = if d.from.is_empty() { &d.path } else { &d.from };
        let at = Path::new(std::ffi::OsStr::from_bytes(from));
        let md = std::fs::metadata(at).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("CDI device {shown}: the microVM has no {}: {e}", at.display()),
            )
        })?;
        let ft = md.file_type();
        let kind = match (d.kind, ft.is_char_device(), ft.is_block_device()) {
            (b'c' | 0, true, _) => libc::S_IFCHR,
            (b'b' | 0, _, true) => libc::S_IFBLK,
            _ => {
                return Err(io::Error::other(format!(
                    "CDI device {shown}: the microVM's {} is no {} device",
                    at.display(),
                    match d.kind {
                        b'b' => "block",
                        b'c' => "character",
                        _ => "block or character",
                    }
                )));
            }
        };
        (kind, md.rdev(), d.mode.unwrap_or(md.mode() & 0o7777))
    };
    if let Some(parent) = d
        .path
        .iter()
        .rposition(|&b| b == b'/')
        .and_then(|i| d.path.get(..i))
        && !parent.is_empty()
    {
        r.mkdir_all(parent, 0o755, None)?;
    }
    let (dir, name) = r.parent(&d.path)?;
    // In place of what the path held, as CDI removes a device before it adds it.
    // SAFETY: unlinkat(2) of a name in a directory of ours.
    unsafe { libc::unlinkat(std::os::fd::AsRawFd::as_raw_fd(&dir), name.as_ptr(), 0) };
    // SAFETY: mknodat(2) and fchownat(2) relative to a descriptor of ours.
    let made = unsafe {
        libc::mknodat(
            std::os::fd::AsRawFd::as_raw_fd(&dir),
            name.as_ptr(),
            kind | mode,
            rdev,
        )
    };
    if made != 0 {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            format!("CDI device {shown}: {}", io::Error::last_os_error()),
        ));
    }
    // SAFETY: as above.
    let owned = unsafe {
        libc::fchownat(
            std::os::fd::AsRawFd::as_raw_fd(&dir),
            name.as_ptr(),
            d.uid,
            d.gid,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if owned != 0 {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            format!("CDI device {shown}: {}", io::Error::last_os_error()),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const CA: &[u8] = b"-----BEGIN CERTIFICATE-----\nAAEC\n-----END CERTIFICATE-----\n";

    /// A step's root of its own, removed as it drops.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("shards-init-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    /// The CA goes into the first bundle that is a regular file, found in the root: a FIFO
    /// first in the list passed over, never waited on; an absolute symlink followed in the
    /// root, not the host's. Its mode kept, it comes out again, the step's own lines kept.
    #[test]
    fn the_proxy_ca_goes_into_the_first_bundle_and_out_again() {
        let s = scratch("ca");
        let at = |p: &str| s.0.join(p.trim_start_matches('/'));
        std::fs::create_dir_all(at("/etc/ssl/certs")).unwrap();
        let fifo = CString::new(at("/etc/ssl/certs/ca-certificates.crt").as_os_str().as_bytes()).unwrap();
        // SAFETY: a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        std::fs::create_dir_all(at("/etc/pki/tls/certs")).unwrap();
        std::os::unix::fs::symlink("/certs/real.pem", at("/etc/pki/tls/certs/ca-bundle.crt")).unwrap();
        std::fs::create_dir_all(at("/certs")).unwrap();
        let bundle = at("/certs/real.pem");
        std::fs::write(&bundle, b"original").unwrap();
        std::fs::set_permissions(&bundle, std::fs::Permissions::from_mode(0o640)).unwrap();
        let r = Root::open(&s.0).unwrap();
        let injected = inject_ca(&r, CA).unwrap().unwrap();
        assert_eq!(injected.path, b"/certs/real.pem");
        assert_eq!(
            std::fs::read(&bundle).unwrap(),
            proxyca::appended(b"original", CA)
        );
        assert_eq!(std::fs::metadata(&bundle).unwrap().mode() & 0o7777, 0o640);
        // Held already: nothing goes in twice.
        assert!(inject_ca(&r, CA).unwrap().is_none());
        // What the step adds stays; the CA goes.
        let mut grown = std::fs::read(&bundle).unwrap();
        grown.extend_from_slice(b"step\n");
        std::fs::write(&bundle, &grown).unwrap();
        injected.clean(&s.0);
        assert_eq!(std::fs::read(&bundle).unwrap(), b"original\nstep\n");
        // No temporary file is left beside it.
        assert_eq!(std::fs::read_dir(at("/certs")).unwrap().count(), 1);
    }

    /// No CA, no bundle: nothing to do. A bundle past 10 MiB fails the step, as BuildKit's.
    #[test]
    fn a_root_without_a_bundle_or_with_a_huge_one() {
        let s = scratch("noca");
        let r = Root::open(&s.0).unwrap();
        assert!(inject_ca(&r, b"").unwrap().is_none());
        assert!(inject_ca(&r, CA).unwrap().is_none());
        std::fs::create_dir_all(s.0.join("etc/ssl")).unwrap();
        let huge = std::fs::File::create(s.0.join("etc/ssl/cert.pem")).unwrap();
        huge.set_len(proxyca::MOST + 1).unwrap();
        let e = inject_ca(&r, CA).unwrap_err().to_string();
        assert_eq!(e, "/etc/ssl/cert.pem exceeds 10485760 bytes");
        assert!(inject_ca(&r, b"no PEM").is_err());
    }

    /// A bundle of 10 MiB to the byte is read, and the CA goes in; past that, it no longer
    /// reads, so the cleanup leaves it, as BuildKit's readCertBundle leaves it.
    #[test]
    fn a_bundle_of_ten_mib_takes_the_ca() {
        let s = scratch("most");
        std::fs::create_dir_all(s.0.join("etc/ssl")).unwrap();
        let bundle = s.0.join("etc/ssl/cert.pem");
        std::fs::File::create(&bundle)
            .unwrap()
            .set_len(proxyca::MOST)
            .unwrap();
        let r = Root::open(&s.0).unwrap();
        let injected = inject_ca(&r, CA).unwrap().unwrap();
        let grown = std::fs::metadata(&bundle).unwrap().len();
        assert!(grown > proxyca::MOST, "{grown}");
        injected.clean(&s.0);
        assert_eq!(std::fs::metadata(&bundle).unwrap().len(), grown);
    }

    /// A bundle is read only where it is a regular file: a FIFO there is refused, never
    /// read as an empty one.
    #[test]
    fn a_bundle_that_is_no_file_is_refused() {
        let s = scratch("nofile");
        std::fs::create_dir_all(s.0.join("etc/ssl")).unwrap();
        let fifo = CString::new(s.0.join("etc/ssl/cert.pem").as_os_str().as_bytes()).unwrap();
        // SAFETY: a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        let r = Root::open(&s.0).unwrap();
        let e = read_bundle(&r, b"/etc/ssl/cert.pem").unwrap_err().to_string();
        assert_eq!(e, "/etc/ssl/cert.pem is not a regular file");
    }
}
