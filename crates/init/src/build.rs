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

use crate::linux::power_off;
use crate::run::{dial, loopback_up, send};
use crate::tree::{LayerWriter, send_upper};

/// The builder's own root, a tmpfs it moves onto at start: a step's pivot_root needs the
/// root it leaves to be a mount of its own, which the initramfs is not
/// (pivot_root(2), EINVAL), and only pivot_root takes the builder's files out of a step's
/// reach; a chroot leaves them there for a step with CAP_SYS_CHROOT.
const B: &str = "/b";
const LAYERS: &str = "/l";
const ROOT: &str = "/root";
const CACHES: &str = "/c";
const EMPTY: &str = "/empty";

/// BuildKit's capabilities in its sandbox (containerd's defaults, docs/research/buildkit-
/// run.md §4), by number (linux/capability.h).
const SANDBOX_CAPS: [u32; 14] = [0, 1, 3, 4, 5, 6, 7, 8, 10, 13, 18, 27, 29, 31];

/// Paths BuildKit masks and makes read-only in its sandbox (§3).
const MASKED: [&str; 12] = [
    "/proc/acpi",
    "/proc/asound",
    "/proc/interrupts",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/proc/sched_debug",
    "/sys/firmware",
    "/sys/devices/virtual/powercap",
    "/proc/scsi",
];
const READONLY: [&str; 5] = [
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
];

fn err(what: impl std::fmt::Display) -> io::Error {
    io::Error::other(what.to_string())
}

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
    for d in [LAYERS, CACHES, EMPTY, ROOT, "/dev", "/proc"] {
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
    let conn = dial(build::PORT, true)?;
    let mut b = Builder {
        conn,
        bases: HashMap::new(),
        writing: None,
        serial: 0,
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
        match tree.base {
            Some(n) => lower.push(self.base(n)?.to_string_lossy().into_owned()),
            None if lower.is_empty() => lower.push(EMPTY.into()),
            None => {}
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
        std::fs::create_dir_all(&work)?;
        let root = Path::new(ROOT);
        self.mount_tree(&step.root, root, Some(&upper))?;
        // Trees the step mounts, each over its own mount point.
        let mut sources: Vec<Option<PathBuf>> = Vec::with_capacity(step.mounts.len());
        for (i, (_, m)) in step.mounts.iter().enumerate() {
            sources.push(match m {
                Mount::Tree { tree, writable, .. } => {
                    let at = work.join(format!("m{i}"));
                    let up = writable.then(|| work.join(format!("m{i}.upper")));
                    self.mount_tree(tree, &at, up.as_deref())?;
                    Some(at)
                }
                Mount::Cache {
                    id, mode, uid, gid, ..
                } => Some(cache(id, *mode, *uid, *gid)?),
                _ => None,
            });
        }
        // What BuildKit removes after the step if the step left it empty (§5).
        let stubs = stubs(root, step);
        mkdir_all_owned(root, &step.cwd, step.uid, step.gid)?;
        let files = work.join("etc");
        std::fs::create_dir_all(&files)?;
        std::fs::write(files.join("hosts"), &step.hosts)?;
        std::fs::write(files.join("resolv.conf"), &step.resolv)?;
        let status = self.run(step, root, &files, &sources, &work);
        clean_stubs(root, &stubs);
        let _ = umount(root);
        for (i, s) in sources.iter().enumerate() {
            if let (Some(at), Some((_, Mount::Tree { .. }))) = (s, step.mounts.get(i)) {
                let _ = umount(at);
            }
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
        }
        let _ = std::fs::remove_dir_all(&work);
        Ok(())
    }

    /// Runs the step's command, relaying its output; its status as `docker run` reports
    /// one: its code, or 128 and the signal that ended it.
    fn run(
        &self,
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
        let flags = libc::CLONE_NEWNS
            | libc::CLONE_NEWPID
            | libc::CLONE_NEWUTS
            | libc::CLONE_NEWIPC
            | libc::CLONE_NEWNET
            | libc::CLONE_NEWCGROUP;
        // SAFETY: clone(2) as fork(2) with new namespaces: no new stack, so the child runs
        // on a copy of this one, as fork's does. init is single-threaded.
        let pid = unsafe {
            libc::syscall(
                libc::SYS_clone,
                (flags | libc::SIGCHLD) as libc::c_ulong,
                0,
                0,
                0,
                0,
            )
        };
        if pid < 0 {
            return Err(os_err("starting the step"));
        }
        if pid == 0 {
            drop((out_r, err_r, fail_r));
            let e = child(step, root, files, sources, work, &out_w, &err_w);
            // Reached only if the command did not start.
            let msg = format!("{e}");
            // SAFETY: a descriptor this process owns.
            unsafe {
                libc::write(fail_w.as_raw_fd(), msg.as_ptr().cast(), msg.len());
                libc::_exit(127);
            }
        }
        drop((out_w, err_w, fail_w));
        self.relay(out_r, err_r)?;
        let mut status = 0;
        // SAFETY: waits for our own child.
        if unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) } < 0 {
            return Err(os_err("waiting for the step"));
        }
        let mut msg = Vec::new();
        File::from(fail_r).read_to_end(&mut msg)?;
        if !msg.is_empty() {
            return Err(err(String::from_utf8_lossy(&msg)));
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

/// A cache directory, kept for the build's life by `id`: made the first time with `mode`
/// (`0755` at least, as BuildKit's mkdir makes it) and `uid:gid`.
fn cache(id: &[u8], mode: u32, uid: u32, gid: u32) -> io::Result<PathBuf> {
    // The id as a name: its bytes in hex, so any id is one name of its own.
    let name: String = id.iter().map(|b| format!("{b:02x}")).collect();
    let at = Path::new(CACHES).join(if name.is_empty() { "-".into() } else { name });
    if !at.exists() {
        std::fs::create_dir(&at)?;
        let c = cstr(&at)?;
        // SAFETY: a NUL-terminated path.
        unsafe {
            if libc::chown(c.as_ptr(), uid, gid) != 0 || libc::chmod(c.as_ptr(), mode | 0o755) != 0 {
                return Err(os_err("making a cache"));
            }
        }
    }
    Ok(at)
}

/// The step's paths that do not exist yet, with each missing parent, leaf first: what
/// BuildKit removes after the step if the step left it empty (executor/stubs.go).
fn stubs(root: &Path, step: &Step) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let targets = [&b"/etc/resolv.conf"[..], b"/etc/hosts"]
        .into_iter()
        .chain(step.mounts.iter().map(|(t, _)| t.as_slice()));
    for t in targets {
        let mut p = PathBuf::from(OsStr::from_bytes(t.strip_prefix(b"/").unwrap_or(t)));
        loop {
            if p.as_os_str().is_empty() || std::fs::symlink_metadata(root.join(&p)).is_ok() {
                break;
            }
            out.push(root.join(&p));
            if !p.pop() {
                break;
            }
        }
    }
    out
}

/// Removes each stub the step left empty, restoring its parent's times.
fn clean_stubs(root: &Path, stubs: &[PathBuf]) {
    for p in stubs {
        let Ok(m) = std::fs::symlink_metadata(p) else {
            continue;
        };
        let empty = if m.is_dir() {
            std::fs::read_dir(p)
                .map(|mut d| d.next().is_none())
                .unwrap_or(false)
        } else {
            m.len() == 0
        };
        let Some(parent) = p.parent().filter(|q| q.starts_with(root)) else {
            continue;
        };
        if !empty {
            continue;
        }
        let times = std::fs::metadata(parent).ok();
        let removed = if m.is_dir() {
            std::fs::remove_dir(p)
        } else {
            std::fs::remove_file(p)
        };
        if removed.is_ok()
            && let Some(t) = times
            && let Ok(c) = cstr(parent)
        {
            let ts = [
                libc::timespec {
                    tv_sec: t.atime(),
                    tv_nsec: t.atime_nsec() as libc::c_long,
                },
                libc::timespec {
                    tv_sec: t.mtime(),
                    tv_nsec: t.mtime_nsec() as libc::c_long,
                },
            ];
            // SAFETY: a NUL-terminated path and two timespecs.
            unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ts.as_ptr(), 0) };
        }
    }
}

/// The working directory, made as BuildKit's executor makes a missing one: each new
/// directory mode 0755, whatever the umask, owned by the step's user (§1).
fn mkdir_all_owned(root: &Path, dir: &[u8], uid: u32, gid: u32) -> io::Result<()> {
    let mut at = root.to_path_buf();
    for name in dir.split(|&b| b == b'/').filter(|n| !n.is_empty() && *n != b".") {
        at.push(OsStr::from_bytes(name));
        match std::fs::symlink_metadata(&at) {
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::fs::create_dir(&at)?;
        let c = cstr(&at)?;
        // SAFETY: a NUL-terminated path.
        unsafe {
            if libc::chmod(c.as_ptr(), 0o755) != 0 || libc::lchown(c.as_ptr(), uid, gid) != 0 {
                return Err(os_err("making the working directory"));
            }
        }
    }
    Ok(())
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
    let at = |p: &str| root.join(p.trim_start_matches('/'));
    for d in ["proc", "dev", "sys"] {
        std::fs::create_dir_all(root.join(d))?;
    }
    mount("proc", &at("/proc"), "proc", nosuid | noexec | nodev, "")?;
    mount(
        "tmpfs",
        &at("/dev"),
        "tmpfs",
        nosuid | libc::MS_STRICTATIME,
        "mode=755,size=65536k",
    )?;
    let devs: &[(&str, u32, u32)] = &[
        ("null", 1, 3),
        ("zero", 1, 5),
        ("full", 1, 7),
        ("random", 1, 8),
        ("urandom", 1, 9),
        ("tty", 5, 0),
    ];
    for (name, major, minor) in devs {
        mknod(&at("/dev").join(name), libc::S_IFCHR | 0o666, *major, *minor)?;
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
            mknod(&at("/dev").join(name), libc::S_IFCHR | 0o660, *major, *minor)?;
        }
        std::fs::create_dir_all(at("/dev/net"))?;
        mknod(&at("/dev/net/tun"), libc::S_IFCHR | 0o660, 10, 200)?;
    }
    for (target, link) in [
        ("/proc/self/fd", "fd"),
        ("/proc/self/fd/0", "stdin"),
        ("/proc/self/fd/1", "stdout"),
        ("/proc/self/fd/2", "stderr"),
        ("pts/ptmx", "ptmx"),
    ] {
        std::os::unix::fs::symlink(target, at("/dev").join(link))?;
    }
    if Path::new("/proc/kcore").exists() {
        std::os::unix::fs::symlink("/proc/kcore", at("/dev/core"))?;
    }
    for d in ["pts", "shm", "mqueue"] {
        std::fs::create_dir(at("/dev").join(d))?;
    }
    mount(
        "devpts",
        &at("/dev/pts"),
        "devpts",
        nosuid | noexec,
        "newinstance,ptmxmode=0666,mode=0620,gid=5",
    )?;
    mount(
        "shm",
        &at("/dev/shm"),
        "tmpfs",
        nosuid | noexec | nodev,
        "mode=1777,size=65536k",
    )?;
    mount(
        "mqueue",
        &at("/dev/mqueue"),
        "mqueue",
        nosuid | noexec | nodev,
        "",
    )?;
    let ro = if step.insecure { 0 } else { libc::MS_RDONLY };
    mount("sysfs", &at("/sys"), "sysfs", nosuid | noexec | nodev | ro, "")?;
    std::fs::create_dir_all(at("/sys/fs/cgroup")).ok();
    mount(
        "cgroup2",
        &at("/sys/fs/cgroup"),
        "cgroup2",
        nosuid | noexec | nodev | ro,
        "",
    )?;
    // /etc/hosts and /etc/resolv.conf over the tree's, read-only, in no layer (§2).
    let bind_ro = nosuid | noexec | nodev;
    for name in ["hosts", "resolv.conf"] {
        let target = at("/etc").join(name);
        file_target(&target)?;
        bind(&files.join(name), &target, bind_ro, true)?;
    }
    // The step's own mounts, in order.
    for (i, (target, m)) in step.mounts.iter().enumerate() {
        let target = root.join(OsStr::from_bytes(target.strip_prefix(b"/").unwrap_or(target)));
        match m {
            Mount::Tree { subpath, .. } => {
                let Some(Some(src)) = sources.get(i) else {
                    return Err(err("a tree mount without its tree"));
                };
                let src = src.join(OsStr::from_bytes(subpath.strip_prefix(b"/").unwrap_or(subpath)));
                if std::fs::metadata(&src)?.is_dir() {
                    std::fs::create_dir_all(&target)?;
                } else {
                    file_target(&target)?;
                }
                // Readonly unless writable; a writable tree's writes go to its own upper.
                bind(&src, &target, 0, false)?;
            }
            Mount::Cache { readonly, .. } => {
                let Some(Some(src)) = sources.get(i) else {
                    return Err(err("a cache mount without its cache"));
                };
                std::fs::create_dir_all(&target)?;
                bind(src, &target, 0, *readonly)?;
            }
            Mount::Tmpfs { size, readonly } => {
                std::fs::create_dir_all(&target)?;
                let data = if *size > 0 {
                    format!("size={size}")
                } else {
                    String::new()
                };
                mount(
                    "tmpfs",
                    &target,
                    "tmpfs",
                    nosuid | if *readonly { libc::MS_RDONLY } else { 0 },
                    &data,
                )?;
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
                file_target(&target)?;
                let exec_bit = if mode & 0o111 == 0 { noexec } else { 0 };
                bind(&file, &target, nosuid | nodev | exec_bit, true)?;
            }
        }
    }
    if !step.insecure {
        for p in MASKED {
            let target = at(p);
            match std::fs::symlink_metadata(&target) {
                Ok(m) if m.is_dir() => mount("tmpfs", &target, "tmpfs", libc::MS_RDONLY, "")?,
                Ok(_) => bind(Path::new("/dev/null"), &target, 0, false)?,
                Err(_) => {}
            }
        }
        for p in READONLY {
            let target = at(p);
            if target.exists() {
                bind(&target, &target, nosuid | noexec | nodev, true)?;
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
    match step.network {
        Network::None => loopback_up()?,
    }
    for &(resource, soft, hard) in &step.rlimits {
        let limit = libc::rlimit {
            rlim_cur: soft,
            rlim_max: hard,
        };
        // SAFETY: setrlimit(2) with a resource number the host gave and a limit struct.
        if unsafe { libc::setrlimit(resource as _, &limit) } != 0 {
            return Err(os_err("setting a ulimit"));
        }
    }
    identity(step)?;
    // SAFETY: umask(2) always succeeds. runc's default (rootfs_linux.go).
    unsafe { libc::umask(0o022) };
    let cwd = cstr(OsStr::from_bytes(if step.cwd.is_empty() {
        b"/"
    } else {
        &step.cwd
    }))?;
    // SAFETY: a NUL-terminated path.
    if unsafe { libc::chdir(cwd.as_ptr()) } != 0 {
        return Err(os_err("entering the working directory"));
    }
    Ok(())
}

fn mknod(path: &Path, mode: u32, major: u32, minor: u32) -> io::Result<()> {
    let c = cstr(path)?;
    // SAFETY: a NUL-terminated path.
    if unsafe { libc::mknod(c.as_ptr(), mode, libc::makedev(major, minor)) } != 0 {
        return Err(os_err(&format!("making {}", path.display())));
    }
    Ok(())
}

/// An empty file to mount over, with its parents, unless something is there.
fn file_target(target: &Path) -> io::Result<()> {
    if std::fs::symlink_metadata(target).is_ok() {
        return Ok(());
    }
    if let Some(p) = target.parent() {
        std::fs::create_dir_all(p)?;
    }
    File::options()
        .write(true)
        .create_new(true)
        .open(target)
        .map(drop)
}

/// Binds `src` over `target`, then applies `flags`, read-only if `readonly`.
fn bind(src: &Path, target: &Path, flags: libc::c_ulong, readonly: bool) -> io::Result<()> {
    mount(
        &src.to_string_lossy(),
        target,
        "",
        libc::MS_BIND | libc::MS_REC,
        "",
    )?;
    let ro = if readonly { libc::MS_RDONLY } else { 0 };
    if flags != 0 || readonly {
        mount("", target, "", libc::MS_BIND | libc::MS_REMOUNT | flags | ro, "")?;
    }
    Ok(())
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Groups, gid and uid, then the capabilities BuildKit gives a step, as runc applies them:
/// the bounding set first, the others kept across the change of user.
fn identity(step: &Step) -> io::Result<()> {
    let last = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(40);
    let keep = |c: u32| step.insecure || SANDBOX_CAPS.contains(&c);
    for c in 0..=last {
        if !keep(c) {
            // SAFETY: prctl(2) with constant arguments.
            if unsafe { libc::prctl(libc::PR_CAPBSET_DROP, c as libc::c_ulong, 0, 0, 0) } != 0 {
                return Err(os_err("dropping a capability"));
            }
        }
    }
    // SAFETY: prctl(2) with constant arguments.
    if unsafe { libc::prctl(libc::PR_SET_KEEPCAPS, 1, 0, 0, 0) } != 0 {
        return Err(os_err("keeping capabilities"));
    }
    // SAFETY: a list of the length given.
    if unsafe { libc::setgroups(step.groups.len(), step.groups.as_ptr()) } != 0 {
        return Err(os_err("setgroups"));
    }
    // SAFETY: setresgid(2) and setresuid(2) with values.
    unsafe {
        if libc::setresgid(step.gid, step.gid, step.gid) != 0 {
            return Err(os_err("setgid"));
        }
        if libc::setresuid(step.uid, step.uid, step.uid) != 0 {
            return Err(os_err("setuid"));
        }
        libc::prctl(libc::PR_SET_KEEPCAPS, 0, 0, 0, 0);
    }
    // For any user but root, execve then leaves none but the bounding set, there being no
    // file or ambient capabilities (capabilities(7)): as BuildKit's steps have it (CapPrm
    // and CapEff 0 for USER 1000:1000 and for nobody, Docker Desktop's BuildKit,
    // 2026-10-02).
    let mut data = [CapData::default(); 2];
    for c in (0..=last).filter(|&c| keep(c)) {
        if let Some(d) = data.get_mut((c / 32) as usize) {
            d.effective |= 1 << (c % 32);
            d.permitted |= 1 << (c % 32);
        }
    }
    let mut header = CapHeader {
        // _LINUX_CAPABILITY_VERSION_3
        version: 0x2008_0522,
        pid: 0,
    };
    // SAFETY: capset(2) with a version 3 header and two data structs.
    if unsafe { libc::syscall(libc::SYS_capset, &raw mut header, data.as_ptr()) } != 0 {
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
    Err(os_err(&format!(
        "exec {}",
        String::from_utf8_lossy(first.as_bytes())
    )))
}

/// Where a command is: itself if it holds a slash, else the first executable regular file
/// of that name in `PATH`, as Go's exec.LookPath finds it for runc.
fn lookup(name: &[u8], env: &[Vec<u8>]) -> io::Result<CString> {
    if name.contains(&b'/') {
        return CString::new(name).map_err(|_| err("a path holds NUL"));
    }
    let path = env
        .iter()
        .rev()
        .find_map(|e| e.strip_prefix(b"PATH="))
        .unwrap_or_default();
    for dir in path.split(|&b| b == b':') {
        let dir: &[u8] = if dir.is_empty() { b"." } else { dir };
        let mut p = dir.to_vec();
        p.push(b'/');
        p.extend_from_slice(name);
        if let Ok(m) = std::fs::metadata(OsStr::from_bytes(&p))
            && m.is_file()
            && m.mode() & 0o111 != 0
        {
            return CString::new(p).map_err(|_| err("a path holds NUL"));
        }
    }
    Err(err(format!(
        "exec: \"{}\": executable file not found in $PATH",
        String::from_utf8_lossy(name)
    )))
}
