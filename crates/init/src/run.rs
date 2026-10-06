//! Runs a workload in an image (docs/design/architecture.md D16). The image on
//! virtio-pmem becomes the root filesystem under a tmpfs overlay, with the mounts Docker
//! gives a container. Then init dials the host for the workload, runs it as `docker run`
//! would, relays its stdio, and reports its exit status.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::time::{Duration, Instant};

use shards_abi::run::{self, BUFFERED, CHUNK, Size, Spec, kind};
use shards_abi::{control, marker};

use crate::defaults::{self, CAPS, DEVICES, LINKS, MASKED, READONLY};
use crate::frames::{Outbox, each_frame};
use crate::linux::power_off;
use crate::orders::{Orders, cstrings, pointers};
use shards_user::{self as user, ExecUser};

/// `docker run`'s status for a command that never ran, when nothing says more
/// (docker/cli cli/command/container/run.go, toStatusError).
const NOT_RUN: u32 = 125;
/// How long a template waits for the kernel's crypto self-tests. One snapshotted while
/// they run is still correct, only slower to restore.
const SELFTESTS_WAIT: Duration = Duration::from_secs(2);

/// `/etc/hosts` as Docker writes it for every container (moby
/// daemon/libnetwork/etchosts/etchosts.go, `Build`): the guest's IPv6 is enabled, so the
/// variant without it (`BuildNoIPv6`) does not apply. Each run adds its own name
/// ([`set_hostname`]).
const HOSTS: &[u8] = b"127.0.0.1\tlocalhost\n\
::1\tlocalhost ip6-localhost ip6-loopback\n\
fe00::\tip6-localnet\n\
ff00::\tip6-mcastprefix\n\
ff02::1\tip6-allnodes\n\
ff02::2\tip6-allrouters\n";

/// Why the workload did not run, and the status to report.
struct Failure {
    status: u32,
    message: String,
    /// The daemon's refusal, not the runtime's failure: an exec's unknown user, which
    /// `docker exec` reports as "Error response from daemon".
    daemon: bool,
}

fn setup_failed(message: impl Into<String>) -> Failure {
    Failure {
        status: NOT_RUN,
        message: message.into(),
        daemon: false,
    }
}

/// Boots into the image on `device`, runs the host's workload, and powers off. As a
/// template (`template`), it asks for a snapshot once the image is mounted: every VM
/// restored from it continues from there, and dials the host for its own workload.
pub fn main(device: &str, template: bool) -> ! {
    // Docker's limits for what it starts, inherited by the standby and so the workload.
    defaults::limits();
    // Before any snapshot, so that every copy of a template has one.
    let standby = mount_root(device).and_then(|()| Standby::fork(None));
    if template && standby.is_ok() {
        await_crypto_selftests();
        if let Err(e) = crate::linux::control_write(control::SNAPSHOT, control::SNAPSHOT_NOW) {
            let _ = writeln!(io::stderr(), "shards-init: requesting a snapshot: {e}");
            power_off()
        }
        // A restored VM continues here, with its snapshot's wall clock.
        let _ = crate::linux::control_write(control::MARKER, marker::RESUMED);
        if let Err(e) = crate::linux::sync_clock() {
            let _ = writeln!(io::stderr(), "shards-init: setting the clock: {e}");
        }
    }
    let conn = match dial(run::PORT, true) {
        Ok(conn) => conn,
        Err(e) => {
            let _ = writeln!(io::stderr(), "shards-init: dialing the host: {e}");
            power_off()
        }
    };
    let _ = crate::linux::control_write(control::MARKER, marker::CONNECTED);
    // Before the workload exists: the host takes one connection on each of its ports, so
    // a workload that dials one finds it taken (AGENTFILE_ARCH.md §9.7). Without blocking:
    // the relay finishes the connection while the workload runs.
    let signals = dial(run::SIGNAL_PORT, false).ok();
    let started = standby.and_then(|standby| standby.start(&receive(&conn)?));
    let status = match started {
        Ok(workload) => {
            let _ = crate::linux::control_write(control::MARKER, marker::WORKLOAD_STARTED);
            let _ = send(&conn, kind::STARTED, &[]);
            workload.relay(&conn, signals)
        }
        Err(f) => {
            let _ = send(&conn, kind::SYSTEM_ERR, f.message.as_bytes());
            f.status
        }
    };
    if oom_killed() {
        let _ = send(&conn, kind::OOM, &[]);
    }
    let _ = send(&conn, kind::EXIT, &status.to_be_bytes());
    // The host may ask for the container's writable layer, to keep (layer.rs).
    if asked_to_save(&conn)
        && let Err(e) = crate::layer::save(&conn)
    {
        let _ = writeln!(io::stderr(), "shards-init: saving the container's files: {e}");
    }
    // The host closes the connection once it has the status. Powering off before then
    // could lose the frame on its way out.
    let _ = shutdown_and_wait(&conn);
    let _ = crate::linux::control_write(control::MARKER, marker::POWERING_OFF);
    power_off()
}

/// Waits for the crypto self-tests the kernel starts at boot (crypto/algapi.c,
/// `crypto_start_tests`), which run in `cryptomgr_test` threads alongside init. Every copy
/// of a template replays what its guest still had running, and this `PREEMPT_NONE` kernel
/// gives such a thread the CPU for up to a tick at a time (docs/research/
/// platform-measurements.md M21).
fn await_crypto_selftests() {
    let deadline = Instant::now() + SELFTESTS_WAIT;
    loop {
        match crypto_selftests_running() {
            Ok(false) => return,
            Ok(true) if Instant::now() >= deadline => {
                let _ = writeln!(
                    io::stderr(),
                    "shards-init: crypto self-tests still running after {SELFTESTS_WAIT:?}; saving the template anyway"
                );
                return;
            }
            Ok(true) => std::thread::sleep(Duration::from_millis(1)),
            Err(e) => {
                let _ = writeln!(io::stderr(), "shards-init: /proc/crypto: {e}");
                return;
            }
        }
    }
}

/// Whether /proc/crypto lists an algorithm under test (a larval) or not yet tested
/// (crypto/proc.c, `c_show`). A kernel without it runs no tests.
fn crypto_selftests_running() -> io::Result<bool> {
    let text = match std::fs::read_to_string("/proc/crypto") {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    Ok(text.lines().any(|line| {
        let mut field = line.splitn(2, ':').map(str::trim);
        matches!(
            (field.next(), field.next()),
            (Some("selftest"), Some("unknown")) | (Some("type"), Some("larval"))
        )
    }))
}

fn c(s: &str) -> Result<CString, Failure> {
    CString::new(s).map_err(|_| setup_failed(format!("{s:?} contains NUL")))
}

fn mount(source: &str, target: &str, fstype: &str, flags: libc::c_ulong, data: &str) -> Result<(), Failure> {
    let (s, t, f, d) = (c(source)?, c(target)?, c(fstype)?, c(data)?);
    // SAFETY: NUL-terminated strings that outlive the call.
    if unsafe { libc::mount(s.as_ptr(), t.as_ptr(), f.as_ptr(), flags, d.as_ptr().cast()) } != 0 {
        return Err(setup_failed(format!(
            "mounting {fstype} on {target}: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Makes a directory unless it exists.
fn mkdir(path: &str) -> Result<(), Failure> {
    match std::fs::create_dir(path) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => Err(setup_failed(format!("mkdir {path}: {e}"))),
        _ => Ok(()),
    }
}

fn chdir_chroot(path: &str, root: bool) -> Result<(), Failure> {
    let p = c(path)?;
    // SAFETY: a NUL-terminated path.
    let rc = unsafe {
        if root {
            libc::chroot(p.as_ptr())
        } else {
            libc::chdir(p.as_ptr())
        }
    };
    if rc != 0 {
        return Err(setup_failed(format!(
            "entering {path}: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn mount_root(device: &str) -> Result<(), Failure> {
    for dir in ["/lower", "/rw", "/newroot"] {
        mkdir(dir)?;
    }
    mount(device, "/lower", "erofs", libc::MS_RDONLY, "dax=always")?;
    mount("tmpfs", "/rw", "tmpfs", 0, "mode=0755")?;
    for dir in ["/rw/upper", "/rw/work"] {
        mkdir(dir)?;
    }
    mount(
        "overlay",
        "/newroot",
        "overlay",
        0,
        "lowerdir=/lower,upperdir=/rw/upper,workdir=/rw/work,volatile",
    )?;
    // Kept for `diff` (changes.rs), which compares the layers the root hides.
    if let Err(e) = crate::changes::keep("/lower", "/rw/upper") {
        let _ = writeln!(io::stderr(), "shards-init: keeping the layers for diff: {e}");
    }
    // The initramfs cannot be unmounted, so the new root moves over it
    // (Documentation/filesystems/ramfs-rootfs-initramfs.rst).
    chdir_chroot("/newroot", false)?;
    mount(".", "/", "", libc::MS_MOVE, "")?;
    chdir_chroot(".", true)?;
    chdir_chroot("/", false)?;
    // Docker's mounts for a container (moby daemon/pkg/oci/defaults.go, a 64 MiB /dev/shm
    // from daemon/config/config.go): /dev holds a container's devices alone, not the VM's
    // disks, memory and console.
    let (nosuid, noexec, nodev) = (libc::MS_NOSUID, libc::MS_NOEXEC, libc::MS_NODEV);
    for (source, target, fstype, flags, data) in [
        ("proc", "/proc", "proc", nosuid | noexec | nodev, ""),
        (
            "sysfs",
            "/sys",
            "sysfs",
            nosuid | noexec | nodev | libc::MS_RDONLY,
            "",
        ),
        (
            "tmpfs",
            "/dev",
            "tmpfs",
            nosuid | libc::MS_STRICTATIME,
            "mode=755,size=65536k",
        ),
        (
            "devpts",
            "/dev/pts",
            "devpts",
            nosuid | noexec,
            "newinstance,ptmxmode=0666,mode=0620,gid=5",
        ),
        (
            "shm",
            "/dev/shm",
            "tmpfs",
            nosuid | noexec | nodev,
            "mode=1777,size=65536k",
        ),
        ("mqueue", "/dev/mqueue", "mqueue", nosuid | noexec | nodev, ""),
    ] {
        mkdir(target)?;
        mount(source, target, fstype, flags, data)?;
    }
    devices()?;
    container_files()?;
    loopback_up().map_err(|e| setup_failed(e.to_string()))?;
    // A VM with a network: eth0 as the host named it, before any snapshot.
    if let Some((addr, prefix, gateway)) = crate::net::from_cmdline() {
        crate::net::configure(addr, prefix, gateway).map_err(|e| setup_failed(format!("eth0: {e}")))?;
    }
    cgroups()?;
    crate::setup::keep_proc_sys().map_err(|e| setup_failed(format!("/proc/sys: {e}")))?;
    // Last: init writes /proc/sys above, and no more after but through what it kept.
    masked()
}

/// What of the workload's process its execs take too, as runc's exec takes the
/// container's (moby daemon/exec.go: the container's process, its command replaced).
#[derive(Clone, Default)]
struct Inherited {
    /// Its capabilities, a bit for each by number; the defaults' where none were said.
    caps: Option<u64>,
    /// `--group-add`'s groups.
    groups: Vec<Vec<u8>>,
    /// Its setup entries an exec's standby applies too: `ulimit=`, `oom=`, and its
    /// seccomp filter and no_new_privs, as runc's exec takes the container's.
    setup: Vec<Vec<u8>>,
}

/// The workload's [`Inherited`], once it starts.
static WORKLOAD: std::sync::OnceLock<Inherited> = std::sync::OnceLock::new();

/// [`Inherited`]'s, and the standby's own, of a spec's setup entries; sysctls are
/// written here, by init.
fn sort_setup(entries: &[Vec<u8>], into: &mut Inherited) -> Result<Vec<Vec<u8>>, Failure> {
    let mut standby = Vec::new();
    for entry in entries {
        if let Some(kv) = entry.strip_prefix(b"sysctl=") {
            let kv = String::from_utf8_lossy(kv);
            let (k, v) = kv.split_once('=').unwrap_or((&kv, ""));
            crate::setup::write_sysctl(k, v).map_err(setup_failed)?;
        } else if let Some(caps) = entry.strip_prefix(b"caps=") {
            into.caps = std::str::from_utf8(caps).ok().and_then(|c| c.parse().ok());
        } else if let Some(group) = entry.strip_prefix(b"group=") {
            into.groups.push(group.to_vec());
        } else {
            if entry.starts_with(b"ulimit=")
                || entry.starts_with(b"oom=")
                || entry.starts_with(b"seccomp=")
                || entry == b"nnp"
            {
                into.setup.push(entry.clone());
            }
            standby.push(entry.clone());
        }
    }
    Ok(standby)
}

/// The workload's use of its microVM, as dockerd's stats read a container's
/// (moby daemon/stats/collector_unix.go, containerd's cgroup v2 metrics): its cgroup's
/// CPU time in ns (cpu.stat usage_usec), memory in use, its inactive file pages and its
/// limit (memory.current, memory.stat, memory.max, else MemTotal), processes
/// (pids.current), bytes read and written (io.stat's rbytes and wbytes, summed); the
/// VM's CPU time in ns from /proc/stat, as dockerd's system usage, and its online CPUs;
/// and the bytes its interfaces received and sent but loopback's (/proc/net/dev), as
/// libnetwork counts a container's endpoints.
fn stats() -> String {
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let cg = |f: &str| read(&format!("{WORKLOAD_CGROUP}/{f}"));
    let field = |text: &str, key: &str| -> u64 {
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(' ')?.trim().parse().ok())
            .unwrap_or(0)
    };
    let number = |text: String| text.trim().parse::<u64>().unwrap_or(0);
    let cpu = field(&cg("cpu.stat"), "usage_usec").saturating_mul(1000);
    let memory = cg("memory.stat");
    let limit = match cg("memory.max").trim() {
        "max" | "" => read("/proc/meminfo")
            .lines()
            .find_map(|l| {
                l.strip_prefix("MemTotal:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })
            .unwrap_or(0)
            .saturating_mul(1024),
        n => n.parse().unwrap_or(0),
    };
    let (mut rbytes, mut wbytes) = (0u64, 0u64);
    for line in cg("io.stat").lines() {
        for kv in line.split_whitespace() {
            if let Some(n) = kv.strip_prefix("rbytes=").and_then(|n| n.parse::<u64>().ok()) {
                rbytes = rbytes.saturating_add(n);
            } else if let Some(n) = kv.strip_prefix("wbytes=").and_then(|n| n.parse::<u64>().ok()) {
                wbytes = wbytes.saturating_add(n);
            }
        }
    }
    // /proc/stat's "cpu" line, in clock ticks (USER_HZ, 100 on Linux) over every CPU.
    let stat = read("/proc/stat");
    let ticks: u64 = stat
        .lines()
        .find(|l| l.starts_with("cpu "))
        .map(|l| {
            l.split_whitespace()
                .skip(1)
                .filter_map(|t| t.parse::<u64>().ok())
                .sum()
        })
        .unwrap_or(0);
    let online = stat
        .lines()
        .filter(|l| l.starts_with("cpu") && !l.starts_with("cpu "))
        .count();
    let (mut rx, mut tx) = (0u64, 0u64);
    for line in read("/proc/net/dev").lines().skip(2) {
        let Some((name, counters)) = line.split_once(':') else {
            continue;
        };
        if name.trim() == "lo" {
            continue;
        }
        let c: Vec<u64> = counters
            .split_whitespace()
            .filter_map(|t| t.parse().ok())
            .collect();
        rx = rx.saturating_add(c.first().copied().unwrap_or(0));
        tx = tx.saturating_add(c.get(8).copied().unwrap_or(0));
    }
    format!(
        "cpu {cpu}\nsystem {}\nonline {online}\nmemory {}\ninactive_file {}\nlimit {limit}\npids {}\nread {rbytes}\nwrite {wbytes}\nrx {rx}\ntx {tx}\n",
        ticks.saturating_mul(10_000_000),
        number(cg("memory.current")),
        field(&memory, "inactive_file"),
        number(cg("pids.current")),
    )
}

/// Where the workload's cgroup is, as init sees the hierarchy.
const WORKLOAD_CGROUP: &str = "/sys/fs/cgroup/workload";

/// The cgroup v2 hierarchy (Linux Documentation/admin-guide/cgroup-v2.rst), as runc gives
/// a container its own: mounted with `nsdelegate`, so that a cgroup namespace bounds what
/// its processes may change; the controllers runc sets limits through enabled below the
/// root; and the cgroup every workload process joins ([`isolate`]). The root is then
/// shared, so that what init mounts later reaches the workload's mount namespace.
fn cgroups() -> Result<(), Failure> {
    let (nosuid, noexec, nodev) = (libc::MS_NOSUID, libc::MS_NOEXEC, libc::MS_NODEV);
    mount(
        "cgroup2",
        "/sys/fs/cgroup",
        "cgroup2",
        nosuid | noexec | nodev,
        "nsdelegate",
    )?;
    // One at a time: a controller the kernel lacks stays off, and a limit of it then
    // fails where it is asked for.
    for controller in ["cpu", "cpuset", "io", "memory", "pids"] {
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .open("/sys/fs/cgroup/cgroup.subtree_control")
            .and_then(|mut f| f.write_all(format!("+{controller}").as_bytes()));
    }
    mkdir(WORKLOAD_CGROUP)?;
    mount("", "/", "", libc::MS_REC | libc::MS_SHARED, "")
}

/// Puts this process, a standby, in the workload's cgroup and in namespaces of its own,
/// as runc puts a container's: a cgroup namespace rooted there, and a mount namespace,
/// a slave of init's, in which `/sys/fs/cgroup` is that cgroup, read-only, as Docker's
/// containers see theirs (moby daemon/pkg/oci/defaults.go). An exec's joins the
/// workload's (`join`, its process), as runc's exec enters a container's. Returns the
/// errno of the step that failed.
fn isolate(join: Option<libc::pid_t>) -> Result<(), i32> {
    let errno = |e: io::Error| e.raw_os_error().unwrap_or(libc::EIO);
    std::fs::OpenOptions::new()
        .write(true)
        .open(format!("{WORKLOAD_CGROUP}/cgroup.procs"))
        .and_then(|mut f| f.write_all(b"0"))
        .map_err(errno)?;
    let last = || io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
    if let Some(pid) = join {
        for (ns, kind) in [("cgroup", libc::CLONE_NEWCGROUP), ("mnt", libc::CLONE_NEWNS)] {
            let path = CString::new(format!("/proc/{pid}/ns/{ns}")).map_err(|_| libc::EINVAL)?;
            // SAFETY: open(2) of a NUL-terminated path, and setns(2) on the descriptor,
            // closed after; this process is a single-threaded fork of init.
            unsafe {
                let fd = libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
                if fd < 0 {
                    return Err(last());
                }
                let joined = libc::setns(fd, kind);
                let e = last();
                libc::close(fd);
                if joined != 0 {
                    return Err(e);
                }
            }
        }
        return Ok(());
    }
    let (nosuid, noexec, nodev) = (libc::MS_NOSUID, libc::MS_NOEXEC, libc::MS_NODEV);
    // SAFETY: unshare(2), mount(2) and umount2(2) on NUL-terminated literals; this
    // process is a single-threaded fork of init.
    unsafe {
        if libc::unshare(libc::CLONE_NEWCGROUP | libc::CLONE_NEWNS) != 0 {
            return Err(last());
        }
        if libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_SLAVE,
            std::ptr::null(),
        ) != 0
            || libc::umount2(c"/sys/fs/cgroup".as_ptr(), libc::MNT_DETACH) != 0
            || libc::mount(
                c"cgroup2".as_ptr(),
                c"/sys/fs/cgroup".as_ptr(),
                c"cgroup2".as_ptr(),
                libc::MS_RDONLY | nosuid | noexec | nodev,
                std::ptr::null(),
            ) != 0
        {
            return Err(last());
        }
    }
    Ok(())
}

/// Whether the kernel killed a process of the workload's cgroup for want of memory: its
/// `memory.events` counts an `oom_kill` (Documentation/admin-guide/cgroup-v2.rst), whether
/// its own limit or the VM's ran out.
fn oom_killed() -> bool {
    std::fs::read_to_string(format!("{WORKLOAD_CGROUP}/memory.events")).is_ok_and(|events| {
        events.lines().any(|l| {
            l.strip_prefix("oom_kill ")
                .and_then(|n| n.trim().parse::<u64>().ok())
                .is_some_and(|n| n > 0)
        })
    })
}

/// Sets the workload's limits, each `FILE=VALUE` of [`Spec::cgroup`], as runc's cgroups
/// WriteFile does, and in its words when the kernel refuses one.
fn limit(cgroup: &[Vec<u8>]) -> Result<(), Failure> {
    write_cgroup(cgroup).map_err(setup_failed)
}

/// Writes each `FILE=VALUE` of `cgroup` to the workload's cgroup, as runc's fs2 writes
/// them; what failed, in runc's words.
pub fn write_cgroup(cgroup: &[Vec<u8>]) -> Result<(), String> {
    for entry in cgroup {
        let text = String::from_utf8_lossy(entry);
        let (file, value) = text
            .split_once('=')
            .filter(|(f, _)| {
                !f.is_empty()
                    && f.bytes()
                        .all(|b| b.is_ascii_lowercase() || b == b'.' || b == b'_')
            })
            .ok_or_else(|| format!("a cgroup setting of no file: {text:?}"))?;
        // runc's setIo: BFQ's weight where the kernel has it, else io.weight's scale
        // (ConvertBlkIOToIOWeightValue).
        let converted;
        let (file, value) = if file == "io.bfq.weight"
            && std::fs::metadata(format!("{WORKLOAD_CGROUP}/io.bfq.weight")).is_err()
        {
            let weight: u64 = value.parse().unwrap_or(0);
            converted = (1 + weight.saturating_sub(10) * 9999 / 990).to_string();
            ("io.weight", converted.as_str())
        } else {
            (file, value)
        };
        let path = format!("{WORKLOAD_CGROUP}/{file}");
        let shown = format!("/sys/fs/cgroup/{file}");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .map_err(|e| format!("open {shown}: {}", go_error(&e)))?;
        f.write_all(value.as_bytes()).map_err(|e| {
            format!(
                "failed to write {}: write {shown}: {}",
                shards_cmdline::go::quote(value),
                go_error(&e)
            )
        })?;
    }
    Ok(())
}

/// An I/O error as Go's syscall.Errno words it.
fn go_error(e: &io::Error) -> String {
    shards_cmdline::go::linux_error(e.raw_os_error().unwrap_or(libc::EIO))
}

/// A container's devices in /dev, and runc's links (crate::defaults).
fn devices() -> Result<(), Failure> {
    for (name, major, minor) in DEVICES {
        let path = c(&format!("/dev/{name}"))?;
        // SAFETY: a NUL-terminated path; mknod(2) and chmod(2), the mode whatever the
        // umask.
        let made = unsafe {
            libc::mknod(path.as_ptr(), libc::S_IFCHR | 0o666, libc::makedev(major, minor)) == 0
                && libc::chmod(path.as_ptr(), 0o666) == 0
        };
        if !made {
            return Err(setup_failed(format!(
                "/dev/{name}: {}",
                io::Error::last_os_error()
            )));
        }
    }
    let link = |target: &str, name: &str| {
        std::os::unix::fs::symlink(target, format!("/dev/{name}"))
            .map_err(|e| setup_failed(format!("/dev/{name}: {e}")))
    };
    for (target, name) in LINKS {
        link(target, name)?;
    }
    if std::path::Path::new("/proc/kcore").exists() {
        link("/proc/kcore", "core")?;
    }
    Ok(())
}

/// The paths a container has masked and read-only (crate::defaults), as runc makes
/// them: a file under /dev/null, a directory under an empty read-only tmpfs, a
/// read-only path bound onto itself and remounted so.
fn masked() -> Result<(), Failure> {
    let (nosuid, noexec, nodev) = (libc::MS_NOSUID, libc::MS_NOEXEC, libc::MS_NODEV);
    for p in MASKED {
        match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => mount("tmpfs", p, "tmpfs", libc::MS_RDONLY, "")?,
            Ok(_) => mount("/dev/null", p, "", libc::MS_BIND, "")?,
            Err(_) => {}
        }
    }
    for p in READONLY {
        if std::fs::symlink_metadata(p).is_ok() {
            mount(p, p, "", libc::MS_BIND | libc::MS_REC, "")?;
            mount(
                "",
                p,
                "",
                libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | nosuid | noexec | nodev,
                "",
            )?;
        }
    }
    Ok(())
}

/// What Docker gives every container beside its image (moby daemon/initlayer/setup_unix.go),
/// alike for every run, so made before any snapshot: `/etc/mtab` as a link to `/proc/mounts`,
/// `/etc/hosts` ([`HOSTS`]), and `/etc/hostname`, which each run fills ([`set_hostname`]).
/// Each replaces whatever the image has there, as Docker's do: 89 of 99 official images
/// ship an `/etc/hostname` left from their build, and amazonlinux an empty `/etc/mtab`.
/// They are files of the run's own, in the overlay's upper layer: unlike Docker's bind
/// mounts, a workload may replace or rename them as it may any other file.
fn container_files() -> Result<(), Failure> {
    use std::os::unix::fs::DirBuilderExt;
    let etc = std::fs::symlink_metadata("/etc");
    if etc.as_ref().is_ok_and(|m| !m.is_dir()) {
        // A directory over whatever else the image has there, as Docker's init layer
        // above the image is.
        std::fs::remove_file("/etc").map_err(|e| setup_failed(format!("replacing /etc: {e}")))?;
    }
    if etc.as_ref().map_or(true, |m| !m.is_dir()) {
        std::fs::DirBuilder::new()
            .mode(0o755)
            .create("/etc")
            .map_err(|e| setup_failed(format!("mkdir /etc: {e}")))?;
    }
    replace("/etc/mtab", |path| {
        std::os::unix::fs::symlink("/proc/mounts", path)
    })?;
    write_file("/etc/hosts", HOSTS)?;
    write_file("/etc/hostname", b"")
}

/// Puts a new file at `path` with `make`, in place of whatever is there but a directory,
/// never through a link.
fn replace(path: &str, make: impl FnOnce(&str) -> io::Result<()>) -> Result<(), Failure> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            return Err(setup_failed(format!("replacing {path}: {e}")));
        }
        _ => {}
    }
    make(path).map_err(|e| setup_failed(format!("writing {path}: {e}")))
}

/// Puts a new root-owned file of mode 0644 holding `bytes` at `path` ([`replace`]).
fn write_file(path: &str, bytes: &[u8]) -> Result<(), Failure> {
    use std::os::unix::fs::OpenOptionsExt;
    replace(path, |path| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(path)?
            .write_all(bytes)
    })
}

/// Where a run's own name points until the VM has a network of its own: an address of
/// the loopback, so that the name reaches the VM itself, as Debian names a host without a
/// permanent IP (Debian Reference, 5.1.1 "The hostname resolution"). Docker maps it to
/// the container's address (moby daemon/libnetwork/sandbox_dns_unix.go, makeHostsRecs);
/// without that, a program that looks itself up (Java's `InetAddress.getLocalHost`)
/// fails. With guest networking (D31), the VM's address takes its place.
const OWN_ADDRESS: &[u8] = b"127.0.1.1";

/// Names the run in `/etc/hostname` and `/etc/hosts`, as Docker does: the name and a
/// newline; then [`HOSTS`]'s lines, `--add-host`'s (`extra`), and the run's own, its full
/// name with `domain` and its first label after (moby
/// daemon/libnetwork/sandbox_dns_unix.go, buildHostsFile and makeHostsRecs), all of mode
/// 0644. A domain is the NIS domain name too, as runc sets it.
fn set_hostname(name: &[u8], domain: &[u8], extra: &[Vec<u8>]) -> Result<(), Failure> {
    if !domain.is_empty() {
        // SAFETY: a buffer of the given length.
        if unsafe { libc::setdomainname(domain.as_ptr().cast(), domain.len()) } != 0 {
            return Err(setup_failed(format!(
                "setdomainname: {}",
                io::Error::last_os_error()
            )));
        }
    }
    let mut bytes = Vec::with_capacity(name.len() + 1);
    bytes.extend_from_slice(name);
    bytes.push(b'\n');
    write_file("/etc/hostname", &bytes)?;
    let mut hosts = Vec::with_capacity(HOSTS.len() + OWN_ADDRESS.len() + name.len() + 2);
    hosts.extend_from_slice(HOSTS);
    for line in extra {
        hosts.extend_from_slice(line);
        hosts.push(b'\n');
    }
    // The guest's own address on a network, as Docker names a container on its bridge;
    // the loopback's otherwise.
    let own = crate::net::from_cmdline().map(|(addr, _, _)| addr.to_string());
    hosts.extend_from_slice(own.as_deref().map_or(OWN_ADDRESS, str::as_bytes));
    hosts.push(b'\t');
    let mut full = name.to_vec();
    if !domain.is_empty() {
        full.push(b'.');
        full.extend_from_slice(domain);
    }
    hosts.extend_from_slice(&full);
    if let Some(dot) = full.iter().position(|&b| b == b'.') {
        hosts.push(b' ');
        hosts.extend_from_slice(full.get(..dot).unwrap_or_default());
    }
    hosts.push(b'\n');
    write_file("/etc/hosts", &hosts)
}

/// Brings the loopback interface up, as every container's network namespace has it,
/// `--network none` included: the kernel then gives it 127.0.0.1/8 and ::1
/// (netdevice(7), SIOCSIFFLAGS).
pub(crate) fn loopback_up() -> io::Result<()> {
    let failed = |what: &str| {
        let e = io::Error::last_os_error();
        io::Error::new(e.kind(), format!("bringing up lo: {what}: {e}"))
    };
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(failed("socket"));
    }
    // SAFETY: a descriptor just opened, owned from here on.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: ifreq is plain data, for which all zeroes is valid.
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in req.ifr_name.iter_mut().zip(b"lo") {
        *dst = *src as libc::c_char;
    }
    // SAFETY: an ifreq naming an interface, which the kernel fills in.
    if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCGIFFLAGS as libc::Ioctl, &mut req) } != 0 {
        return Err(failed("SIOCGIFFLAGS"));
    }
    // SAFETY: SIOCGIFFLAGS filled the flags member of the union.
    unsafe {
        req.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
    }
    // SAFETY: the ifreq as read, with IFF_UP added.
    if unsafe { libc::ioctl(sock.as_raw_fd(), libc::SIOCSIFFLAGS as libc::Ioctl, &req) } != 0 {
        return Err(failed("SIOCSIFFLAGS"));
    }
    Ok(())
}

/// Connects to a host port. Without blocking, the connection may still be in progress:
/// the socket turns writable when it completes.
pub(crate) fn dial(port: u32, blocking: bool) -> io::Result<File> {
    let flags = libc::SOCK_STREAM | libc::SOCK_CLOEXEC | if blocking { 0 } else { libc::SOCK_NONBLOCK };
    // SAFETY: socket(2) with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_VSOCK, flags, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let sock = unsafe { File::from_raw_fd(fd) };
    // SAFETY: an all-zero sockaddr_vm is a valid value.
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_cid = libc::VMADDR_CID_HOST;
    addr.svm_port = port;
    let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
    // SAFETY: `addr` is a valid sockaddr_vm of `len` bytes.
    if unsafe { libc::connect(fd, (&raw const addr).cast(), len) } != 0 {
        let e = io::Error::last_os_error();
        if blocking || e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(e);
        }
    }
    Ok(sock)
}

/// A nonblocking connect's result, once its socket is writable.
fn connect_result(fd: RawFd) -> io::Result<()> {
    let mut err: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt(2) into a c_int of `len` bytes.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&raw mut err).cast(),
            &mut len,
        )
    };
    match (rc, err) {
        (0, 0) => Ok(()),
        (0, e) => Err(io::Error::from_raw_os_error(e)),
        _ => Err(io::Error::last_os_error()),
    }
}

/// Writes one frame, blocking.
pub(crate) fn send(conn: &File, kind: u8, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("frame too long"))?;
    let mut w = conn;
    w.write_all(&run::header(kind, len))?;
    w.write_all(payload)
}

/// Half-closes the connection and waits, at most two seconds, for the host to close it.
/// Whether the host, after the exit status, asks for the writable layer
/// ([`kind::SAVE`]) rather than closing the connection. What it still sends of the
/// workload's stdin is passed over.
fn asked_to_save(conn: &File) -> bool {
    let mut r = conn;
    let mut h = [0u8; run::HEADER];
    let mut skip = vec![0u8; CHUNK];
    loop {
        if r.read_exact(&mut h).is_err() {
            return false;
        }
        match run::parse_header(h) {
            Some((kind::SAVE, _)) => return true,
            Some((_, len)) => {
                let mut left = len as usize;
                while left > 0 {
                    let n = left.min(skip.len());
                    if r.read_exact(skip.get_mut(..n).unwrap_or_default()).is_err() {
                        return false;
                    }
                    left -= n;
                }
            }
            None => return false,
        }
    }
}

fn shutdown_and_wait(conn: &File) -> io::Result<()> {
    // SAFETY: shutdown(2) on our own socket.
    unsafe { libc::shutdown(conn.as_raw_fd(), libc::SHUT_WR) };
    let mut pfd = libc::pollfd {
        fd: conn.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let mut buf = [0u8; 256];
    loop {
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut pfd, 1, 2000) } <= 0 {
            return Ok(());
        }
        if (&*conn).read(&mut buf)? == 0 {
            return Ok(());
        }
    }
}

/// The workload the host sends; first, if it sends one, the container's writable layer
/// from before, put over the root (layer.rs).
fn receive(conn: &File) -> Result<Spec, Failure> {
    let mut r = conn;
    let mut h = [0u8; run::HEADER];
    r.read_exact(&mut h)
        .map_err(|e| setup_failed(format!("reading the workload: {e}")))?;
    if let Some((kind::LAYER, first)) = run::parse_header(h) {
        crate::layer::apply(conn, first)
            .map_err(|e| setup_failed(format!("putting back the container's files: {e}")))?;
        r.read_exact(&mut h)
            .map_err(|e| setup_failed(format!("reading the workload: {e}")))?;
    }
    let len = match run::parse_header(h) {
        Some((kind::SPEC, len)) => len,
        _ => return Err(setup_failed("the host sent no workload")),
    };
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)
        .map_err(|e| setup_failed(format!("reading the workload: {e}")))?;
    Spec::decode(&payload).ok_or_else(|| setup_failed("malformed workload"))
}

fn pipe() -> Result<(OwnedFd, OwnedFd), Failure> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: pipe2(2) fills two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(setup_failed(format!("pipe: {}", io::Error::last_os_error())));
    }
    let [r, w] = fds;
    // SAFETY: fresh descriptors nothing else owns.
    Ok(unsafe { (OwnedFd::from_raw_fd(r), OwnedFd::from_raw_fd(w)) })
}

/// What the child reports through its error pipe when it cannot exec: the step that
/// failed and errno.
mod step {
    pub const CHDIR: u8 = 0;
    pub const USER: u8 = 1;
    /// execve(2) itself, or setting up the stdio before it.
    pub const EXEC: u8 = 2;
    /// No candidate on PATH was an executable file.
    pub const NOT_IN_PATH: u8 = 3;
    /// The command, named by a path, is not there: its stat(2) failed.
    pub const STAT: u8 = 4;
    /// The command, named by a path, is a directory or may not be executed.
    pub const ACCESS: u8 = 5;
    /// Opening its terminal, or making it the session's.
    pub const TTY: u8 = 6;
    /// Joining the workload's cgroup, or its namespaces ([`super::isolate`]).
    pub const CGROUP: u8 = 7;
    /// An entry of `Spec::setup`, its index in the candidate's place
    /// (crate::setup::apply).
    pub const SETUP: u8 = 8;
    /// Loading its seccomp filter.
    pub const SECCOMP: u8 = 9;
    /// Setting no_new_privs.
    pub const NNP: u8 = 10;
}

/// A running workload and init's ends of its stdio. With a terminal, `stdout` is its
/// pty's master and `stdin` a duplicate of it, and there is no `stderr`.
struct Workload {
    pid: libc::pid_t,
    stdin: Option<OwnedFd>,
    stdout: Option<OwnedFd>,
    stderr: Option<OwnedFd>,
    sigchld: OwnedFd,
    tty: bool,
    /// A held standby's orders, never sent: it waits on them until it is killed
    /// ([`run::builtin::HOLD`]).
    _held: Option<OwnedFd>,
}

/// A pty's master, and the path of its peer, which the standby opens as the workload's
/// terminal. runc's steps (libcontainer/console_linux.go, safeAllocPty and setupConsole;
/// docs/research/tty-and-interactive-runs.md §2.2): a new master, unlocked; the size
/// given, if both dimensions are; the peer owned by the workload's user
/// (libcontainer/init_linux.go, fixStdioPermissions). The pty keeps the kernel's termios,
/// as on Docker's path, which changes none.
struct Pty {
    master: OwnedFd,
    peer: Vec<u8>,
}

impl Pty {
    fn open(size: Size, uid: u32) -> Result<Pty, Failure> {
        let failed = |what: &str| setup_failed(format!("{what}: {}", io::Error::last_os_error()));
        // SAFETY: posix_openpt(3) returns a new descriptor, or -1.
        let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(failed("opening /dev/ptmx"));
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let master = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: unlockpt(3) on our master.
        if unsafe { libc::unlockpt(fd) } != 0 {
            return Err(failed("unlocking the pty"));
        }
        let mut name = [0u8; 64];
        // SAFETY: ptsname_r(3) writes a NUL-terminated name within the buffer's length.
        if unsafe { libc::ptsname_r(fd, name.as_mut_ptr().cast(), name.len()) } != 0 {
            return Err(failed("naming the pty"));
        }
        let peer = std::ffi::CStr::from_bytes_until_nul(&name)
            .map_err(|_| setup_failed("the pty's name is not terminated"))?
            .to_bytes()
            .to_vec();
        if size.rows != 0 && size.cols != 0 {
            let ws = libc::winsize {
                ws_row: size.rows,
                ws_col: size.cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            // SAFETY: TIOCSWINSZ reads one winsize.
            if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } != 0 {
                return Err(failed("sizing the pty"));
            }
        }
        let path = CString::new(peer.clone()).map_err(|_| setup_failed("the pty's name holds a NUL"))?;
        // SAFETY: chown(2) of the peer by path; gid -1 keeps devpts's.
        if unsafe { libc::chown(path.as_ptr(), uid, u32::MAX) } != 0 {
            return Err(failed("handing the pty to the workload's user"));
        }
        Ok(Pty { master, peer })
    }
}

/// A process forked ahead of its workload's request, waiting to exec it, with the pipes
/// that become its stdio. init forks it before the template's snapshot, so that no run
/// waits for a fork (docs/research/platform-measurements.md M27).
struct Standby {
    pid: libc::pid_t,
    /// Where init writes the standby's orders: what to exec, and as whom.
    orders: OwnedFd,
    /// Closes when the standby execs: bytes on it mean it could not.
    err: OwnedFd,
    stdin: OwnedFd,
    stdout: OwnedFd,
    stderr: OwnedFd,
    sigchld: OwnedFd,
    /// The image's user database, read once: a template's image cannot change.
    passwd: Option<Vec<u8>>,
    group: Option<Vec<u8>>,
}

/// The standby's ends of its pipes, and init's, which it closes.
struct Ends {
    orders: OwnedFd,
    stdio: [OwnedFd; 3],
    err: OwnedFd,
    inits: [OwnedFd; 6],
}

impl Standby {
    /// A standby in namespaces of its own, or, with `join`, in those of that process, the
    /// workload an exec runs beside.
    fn fork(join: Option<libc::pid_t>) -> Result<Standby, Failure> {
        let passwd = std::fs::read("/etc/passwd").ok();
        let group = std::fs::read("/etc/group").ok();
        let (stdin_r, stdin_w) = pipe()?;
        let (stdout_r, stdout_w) = pipe()?;
        let (stderr_r, stderr_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        let (orders_r, orders_w) = pipe()?;
        // SIGCHLD arrives through a signalfd, so the relay can poll for it.
        // SAFETY: plain sigset operations on a local set.
        let sigchld = unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGCHLD);
            libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK)
        };
        if sigchld < 0 {
            return Err(setup_failed(format!("signalfd: {}", io::Error::last_os_error())));
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let sigchld = unsafe { OwnedFd::from_raw_fd(sigchld) };
        // SAFETY: init is single-threaded, so its child may run anything until it execs.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(setup_failed(format!("fork: {}", io::Error::last_os_error())));
        }
        if pid == 0 {
            standby(
                Ends {
                    orders: orders_r,
                    stdio: [stdin_r, stdout_w, stderr_w],
                    err: err_w,
                    inits: [stdin_w, stdout_r, stderr_r, err_r, orders_w, sigchld],
                },
                join,
            )
        }
        drop((stdin_r, stdout_w, stderr_w, err_w, orders_r));
        Ok(Standby {
            pid,
            orders: orders_w,
            err: err_r,
            stdin: stdin_w,
            stdout: stdout_r,
            stderr: stderr_r,
            sigchld,
            passwd,
            group,
        })
    }

    /// Sets the container up as the spec says, then has the standby exec the workload. A
    /// standby that has ended is replaced first.
    fn start(self, spec: &Spec) -> Result<Workload, Failure> {
        let mut status = 0;
        // SAFETY: waitpid(2) for our own child, without blocking.
        let ended = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) } == self.pid;
        let standby = if ended { Standby::fork(None)? } else { self };
        if !spec.hostname.is_empty() {
            // SAFETY: a buffer of the given length.
            if unsafe { libc::sethostname(spec.hostname.as_ptr().cast(), spec.hostname.len()) } != 0 {
                return Err(setup_failed(format!(
                    "sethostname: {}",
                    io::Error::last_os_error()
                )));
            }
            set_hostname(&spec.hostname, &spec.domainname, &spec.hosts)?;
        }
        if let Some(r) = &spec.resolv {
            write_file("/etc/resolv.conf", r)?;
        }
        // A visit to a stopped container's files (`shards cp`, `diff`, `export`): the
        // standby is the workload, run nothing of the image's, and holds until killed.
        if spec.builtin == run::builtin::HOLD {
            let Standby {
                pid,
                orders,
                stdout,
                stderr,
                sigchld,
                ..
            } = standby;
            return Ok(Workload {
                pid,
                stdin: None,
                stdout: Some(stdout),
                stderr: Some(stderr),
                sigchld,
                tty: false,
                _held: Some(orders),
            });
        }
        limit(&spec.cgroup)?;
        // Sysctls, by init; the rest by the standby, in its namespaces.
        let mut inherited = Inherited::default();
        let setup = sort_setup(&spec.setup, &mut inherited)?;
        let _ = WORKLOAD.set(inherited.clone());
        standby.launch(spec, false, setup, &inherited)
    }

    /// Resolves the spec as Docker and runc do, then has the standby exec it. The
    /// workload's working directory is made if missing, as `docker run` makes it; an
    /// exec's (`exec`) must be there, as runc's exec finds it, and its unknown user is the
    /// daemon's refusal.
    fn launch(
        self,
        spec: &Spec,
        exec: bool,
        setup: Vec<Vec<u8>>,
        process: &Inherited,
    ) -> Result<Workload, Failure> {
        let standby = self;
        let argv0 = spec
            .argv
            .first()
            .ok_or_else(|| setup_failed("no command given"))?;
        let (passwd, group) = (standby.passwd.as_deref(), standby.group.as_deref());
        let ExecUser { uid, gid, mut groups } =
            user::resolve(&spec.user, passwd, group).map_err(|m| Failure {
                daemon: exec,
                ..setup_failed(m)
            })?;
        // `--group-add`, as moby's getUser adds them (GetAdditionalGroupsPath).
        if !process.groups.is_empty() {
            let added = user::additional_groups(&process.groups, group).map_err(|m| Failure {
                daemon: exec,
                ..setup_failed(m)
            })?;
            groups.extend(added);
        }
        let env = user::prepare_env(&spec.env, uid, passwd).map_err(setup_failed)?;
        let cwd = if exec {
            exec_cwd(&spec.cwd)?
        } else {
            workdir(&spec.cwd)?
        };
        let path_env = env
            .iter()
            .rev()
            .find_map(|kv| kv.strip_prefix(b"PATH="))
            .unwrap_or_default();
        let explicit = argv0.contains(&b'/');
        // An empty PATH has no entries (Go's filepath.SplitList), so nothing is found in
        // it, as runc finds nothing.
        let candidates: Vec<Vec<u8>> = if explicit {
            vec![argv0.clone()]
        } else if path_env.is_empty() {
            Vec::new()
        } else {
            path_env
                .split(|&b| b == b':')
                .map(|dir| {
                    // An empty PATH entry is the working directory.
                    let dir: &[u8] = if dir.is_empty() { b"." } else { dir };
                    [dir, b"/", argv0].concat()
                })
                .collect()
        };
        for (list, what) in [
            (&candidates, "PATH"),
            (&spec.argv, "an argument"),
            (&env, "the environment"),
        ] {
            if list.iter().any(|b| b.contains(&0)) {
                return Err(setup_failed(format!("{what} contains a NUL byte")));
            }
        }
        if cwd.contains(&0) {
            return Err(setup_failed("the working directory contains a NUL byte"));
        }
        // The workload owns its stdio, so it can reopen it through /proc/self/fd, as runc's
        // fixStdioPermissions arranges (libcontainer/init_linux.go). A pipe's two ends are
        // one inode.
        let pty = match spec.tty {
            Some(size) => Some(Pty::open(size, uid)?),
            None => {
                for fd in [&standby.stdin, &standby.stdout, &standby.stderr] {
                    // SAFETY: fchown(2) on our own pipe; gid -1 leaves the group.
                    unsafe { libc::fchown(fd.as_raw_fd(), uid, u32::MAX) };
                }
                None
            }
        };
        let tried = candidates.clone();
        // Without `-i` or a terminal, its stdin is /dev/null, as Docker's is.
        let null_stdin = pty.is_none() && !spec.stdin;
        let orders = Orders {
            uid,
            gid,
            groups,
            cwd,
            explicit,
            candidates,
            argv: spec.argv.clone(),
            env,
            tty: pty.as_ref().map(|p| p.peer.clone()).unwrap_or_default(),
            null_stdin,
            setup: setup.clone(),
            caps: process
                .caps
                .unwrap_or_else(|| CAPS.iter().fold(0, |m, &c| m | 1 << c)),
        }
        .encode();
        let Standby {
            pid,
            orders: to_standby,
            err,
            stdin,
            stdout,
            stderr,
            sigchld,
            ..
        } = standby;
        File::from(to_standby)
            .write_all(&orders)
            .map_err(|e| setup_failed(format!("starting the workload: {e}")))?;
        // The error pipe closes on exec: bytes on it mean the workload never started.
        let mut report = Vec::new();
        let _ = File::from(err).read_to_end(&mut report);
        if let [which, e0, e1, e2, e3, c0, c1, c2, c3] = report[..] {
            // The standby exits right after reporting.
            // SAFETY: waits for our own child.
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
            let errno = i32::from_be_bytes([e0, e1, e2, e3]);
            if which == step::SETUP {
                let entry = usize::try_from(u32::from_be_bytes([c0, c1, c2, c3]))
                    .ok()
                    .and_then(|i| setup.get(i))
                    .map_or(&[][..], Vec::as_slice);
                let message = crate::setup::failed(entry, errno);
                return Err(Failure {
                    status: u32::from(shards_cmdline::commands::run_status(&message)),
                    message,
                    daemon: false,
                });
            }
            let tried = match (which, &pty) {
                (step::TTY, Some(p)) => &p.peer[..],
                _ => usize::try_from(u32::from_be_bytes([c0, c1, c2, c3]))
                    .ok()
                    .and_then(|i| tried.get(i))
                    .map_or(&argv0[..], Vec::as_slice),
            };
            return Err(exec_failure(which, errno, argv0, tried, &spec.cwd, &spec.user));
        }
        let Some(Pty { master, .. }) = pty else {
            return Ok(Workload {
                pid,
                stdin: (!null_stdin).then_some(stdin),
                stdout: Some(stdout),
                stderr: Some(stderr),
                sigchld,
                tty: false,
                _held: None,
            });
        };
        // The terminal carries everything; the pipes go unused.
        drop((stdin, stdout, stderr));
        let input = master
            .try_clone()
            .map_err(|e| setup_failed(format!("the pty's master: {e}")))?;
        Ok(Workload {
            pid,
            stdin: Some(input),
            stdout: Some(master),
            stderr: None,
            sigchld,
            tty: true,
            _held: None,
        })
    }
}

/// The standby's side of the fork: it closes init's ends, waits for its orders, and runs
/// them as `child` does. The standby is single-threaded, as init was when it forked, so
/// it may allocate.
fn standby(ends: Ends, join: Option<libc::pid_t>) -> ! {
    let Ends {
        orders,
        stdio,
        err,
        inits,
    } = ends;
    drop(inits);
    // Before the orders: the standby the template keeps is isolated before its snapshot.
    let isolated = isolate(join);
    let mut bytes = Vec::new();
    let got = File::from(orders).read_to_end(&mut bytes);
    let decoded = got.ok().and_then(|_| Orders::decode(&bytes));
    let built = decoded.and_then(|mut o| {
        let tty = std::mem::take(&mut o.tty);
        let tty = if tty.is_empty() {
            None
        } else {
            Some(CString::new(tty).ok()?)
        };
        Some((
            cstrings(std::mem::take(&mut o.candidates))?,
            cstrings(std::mem::take(&mut o.argv))?,
            cstrings(std::mem::take(&mut o.env))?,
            CString::new(std::mem::take(&mut o.cwd)).ok()?,
            tty,
            o,
        ))
    });
    let Some((candidates, argv, envp, cwd, tty, o)) = built else {
        // No orders: init has gone, or the VM is powering off. Nothing to run.
        // SAFETY: ends this process without running atexit handlers inherited from init.
        unsafe { libc::_exit(NOT_RUN as libc::c_int) }
    };
    let fail = |which: u8, errno: i32, index: usize| -> ! {
        let [a, b, c, d] = errno.to_be_bytes();
        let [e, f, g, h] = u32::try_from(index).unwrap_or(u32::MAX).to_be_bytes();
        let report = [which, a, b, c, d, e, f, g, h];
        // SAFETY: write(2) of a local buffer to our error pipe, then _exit(2).
        unsafe {
            libc::write(err.as_raw_fd(), report.as_ptr().cast(), report.len());
            libc::_exit(127)
        }
    };
    if let Err(errno) = isolated {
        fail(step::CGROUP, errno, 0);
    }
    for (i, entry) in o.setup.iter().enumerate() {
        if let Err(errno) = crate::setup::apply(entry) {
            fail(step::SETUP, errno, i);
        }
    }
    let (argv_ptrs, envp_ptrs) = (pointers(&argv), pointers(&envp));
    let [stdin, stdout, stderr] = &stdio;
    // Opened here, in this single-threaded fork of init: a workload without `-i` or a
    // terminal reads /dev/null, as Docker's does; the pipe, at its end already, where
    // that cannot be opened.
    let null = o.null_stdin.then(|| File::open("/dev/null").ok()).flatten();
    let stdin = null.as_ref().map_or(stdin.as_raw_fd(), AsRawFd::as_raw_fd);
    // Read now, in this single-threaded fork of init, and not in `child`.
    let last_cap = defaults::last_cap();
    let filter = crate::setup::filter(&o.setup);
    let fprog = filter.as_ref().map(|(_, p)| libc::sock_fprog {
        len: u16::try_from(p.len()).unwrap_or(u16::MAX),
        filter: p.as_ptr().cast_mut(),
    });
    let seccomp = filter.as_ref().map(|(f, _)| *f).zip(fprog.as_ref());
    let nnp = o.setup.iter().any(|e| e == b"nnp");
    // SAFETY: this process is the child of a fork of single-threaded init, and `child`
    // runs on data built above.
    unsafe {
        child(&Child {
            stdio: [stdin, stdout.as_raw_fd(), stderr.as_raw_fd()],
            tty: tty.as_ref(),
            err: err.as_raw_fd(),
            cwd: &cwd,
            caps: o.caps,
            uid: o.uid,
            gid: o.gid,
            groups: &o.groups,
            candidates: &candidates,
            explicit: o.explicit,
            last_cap,
            seccomp,
            nnp,
            argv: argv_ptrs.as_ptr(),
            envp: envp_ptrs.as_ptr(),
        })
    }
}

/// A command run beside the workload (`docker exec`): its process, init's ends of its
/// stdio, and its own connection to the host, on which it reports as the workload
/// reports on the run connection.
struct Exec {
    id: u32,
    conn: Option<File>,
    /// Its nonblocking connect has completed.
    connected: bool,
    /// Its process, or 0 if it never started.
    pid: libc::pid_t,
    stdin: Option<OwnedFd>,
    stdout: Option<OwnedFd>,
    stderr: Option<OwnedFd>,
    tty: bool,
    from_conn: Vec<u8>,
    to_stdin: Outbox,
    stdin_eof: bool,
    to_conn: Outbox,
    status: Option<u32>,
    /// Its EXIT is queued.
    ended: bool,
}

/// One of init's own for an exec ([`Spec::builtin`]), done in a child of init's, so that
/// the relay goes on, its output on a pipe as a command's: what is asked for on stdout,
/// status 0; why it could not be had on stderr, status 1.
fn builtin(kind: u8, args: &[Vec<u8>]) -> Result<Started, Failure> {
    let (stdout_r, stdout_w) = pipe()?;
    let (stderr_r, stderr_w) = pipe()?;
    // What reads the host's stdin: unpacking an archive.
    let input = if kind == run::builtin::EXTRACT {
        Some(pipe()?)
    } else {
        None
    };
    // SAFETY: init is single-threaded, so its child may run anything.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(setup_failed(format!("fork: {}", io::Error::last_os_error())));
    }
    if pid == 0 {
        drop((stdout_r, stderr_r));
        let mut out = io::BufWriter::new(File::from(stdout_w));
        // `cp`'s work, which says how it failed by its status (copy.rs).
        if matches!(
            kind,
            run::builtin::STAT | run::builtin::ARCHIVE | run::builtin::EXTRACT
        ) {
            let path = args.first().map_or(&[][..], Vec::as_slice);
            let done = match kind {
                run::builtin::STAT => crate::copy::stat(path, &mut out),
                run::builtin::ARCHIVE => crate::copy::archive(path, &mut out),
                _ => match input {
                    Some((r, w)) => {
                        drop(w);
                        let user = args.get(1).map(Vec::as_slice);
                        let overwrite = args.get(2).is_some_and(|a| a == b"1");
                        crate::copy::extract(path, user, overwrite, &mut File::from(r))
                    }
                    None => Err(crate::copy::Failed(1, "no input".into())),
                },
            }
            .and_then(|()| out.flush().map_err(crate::copy::Failed::from));
            let code = match done {
                Ok(()) => 0,
                Err(crate::copy::Failed(code, said)) => {
                    let _ = writeln!(File::from(stderr_w), "{said}");
                    code
                }
            };
            // SAFETY: _exit(2) ends the child without running init's exit paths.
            unsafe { libc::_exit(code) }
        }
        let done = match kind {
            run::builtin::PROCESSES => out.write_all(&crate::procs::dump()),
            run::builtin::CHANGES => crate::changes::write(&mut out),
            run::builtin::EXPORT => export(&mut out),
            run::builtin::CGROUP => write_cgroup(args).map_err(io::Error::other),
            run::builtin::STATS => out.write_all(stats().as_bytes()),
            run::builtin::SIZE => crate::changes::upper()
                .ok_or_else(|| io::Error::other("the writable layer was not kept"))
                .and_then(|u| crate::layer::usage(std::path::Path::new(&u)))
                .and_then(|n| write!(out, "{n}")),
            run::builtin::LAYER => {
                // Paused as dockerd pauses a container it commits (moby daemon/commit.go):
                // every process but init and this one stopped, then let go on.
                let pause = args.first().is_some_and(|a| a == b"pause");
                if pause {
                    // SAFETY: kill(2) of every process this one may signal.
                    unsafe { libc::kill(-1, libc::SIGSTOP) };
                }
                let packed = crate::layer::pack(&mut out);
                if pause {
                    // SAFETY: as above.
                    unsafe { libc::kill(-1, libc::SIGCONT) };
                }
                packed
            }
            other => Err(io::Error::other(format!("no built-in {other}"))),
        }
        .and_then(|()| out.flush());
        let code = match done {
            Ok(()) => 0,
            Err(e) => {
                let _ = writeln!(File::from(stderr_w), "{e}");
                1
            }
        };
        // SAFETY: _exit(2) ends the child without running init's exit paths.
        unsafe { libc::_exit(code) }
    }
    drop((stdout_w, stderr_w));
    let stdin = input.map(|(r, w)| {
        drop(r);
        w
    });
    Ok(Started {
        pid,
        tty: false,
        stdin,
        stdout: Some(stdout_r),
        stderr: Some(stderr_r),
    })
}

/// The container's files as a tar archive, as dockerd exports its root (moby
/// daemon/export.go: go-archive's Tar of the container's mounted root): what is
/// mounted over the root is not its files, and the files init writes in `/etc` are
/// left out, where Docker's root holds them empty for its mounts.
fn export(out: &mut impl Write) -> io::Result<()> {
    let opts = shards_archive::PackOptions {
        exclude_patterns: [
            "proc/*",
            "sys/*",
            "dev/*",
            "etc/hosts",
            "etc/hostname",
            "etc/resolv.conf",
        ]
        .iter()
        .map(|p| p.as_bytes().to_vec())
        .collect(),
        ..Default::default()
    };
    shards_archive::pack(std::path::Path::new("/"), &opts, out)
        .map(drop)
        .map_err(|e| io::Error::other(e.to_string()))
}

/// An exec started: its process and init's ends of its stdio.
struct Started {
    pid: libc::pid_t,
    tty: bool,
    stdin: Option<OwnedFd>,
    stdout: Option<OwnedFd>,
    stderr: Option<OwnedFd>,
}

impl Exec {
    /// Starts the command a host's [`kind::EXEC`] frame asks for, unless the workload
    /// has ended (`running`), and dials its connection. Without a connection, the exec's
    /// id and why, for the host to hear on the workload's ([`kind::EXEC_FAILED`]); none if
    /// the frame does not say which exec it is.
    fn start(payload: &[u8], running: bool, workload: libc::pid_t) -> Result<Exec, Option<(u32, String)>> {
        let (token, rest) = payload.split_at_checked(run::TOKEN).ok_or(None)?;
        let (id, spec) = rest.split_at_checked(4).ok_or(None)?;
        let id = u32::from_be_bytes(id.try_into().map_err(|_| None)?);
        let spec = Spec::decode(spec).ok_or_else(|| Some((id, "a malformed command".to_string())))?;
        let conn = dial(run::EXEC_PORT, false)
            .map_err(|e| Some((id, format!("connecting to the host for the command: {e}"))))?;
        let mut exec = Exec {
            id,
            conn: Some(conn),
            connected: false,
            pid: 0,
            stdin: None,
            stdout: None,
            stderr: None,
            tty: false,
            from_conn: Vec::new(),
            to_stdin: Outbox::default(),
            stdin_eof: false,
            to_conn: Outbox::default(),
            status: None,
            ended: false,
        };
        exec.to_conn
            .extend(&[&run::header(kind::HELLO, run::TOKEN as u32), token]);
        let started = if spec.builtin != 0 && running {
            builtin(spec.builtin, &spec.argv)
        } else if running {
            // The workload's process, but for what the exec says (`--privileged`).
            let mut process = WORKLOAD.get().cloned().unwrap_or_default();
            if let Some(caps) = spec.setup.iter().find_map(|e| e.strip_prefix(b"caps=")) {
                process.caps = std::str::from_utf8(caps).ok().and_then(|c| c.parse().ok());
            }
            Standby::fork(Some(workload))
                .and_then(|standby| standby.launch(&spec, true, process.setup.clone(), &process))
                .map(|w| Started {
                    pid: w.pid,
                    tty: w.tty,
                    stdin: w.stdin,
                    stdout: w.stdout,
                    stderr: w.stderr,
                })
        } else {
            Err(setup_failed("the container's main process has exited"))
        };
        match started {
            Ok(w) => {
                exec.pid = w.pid;
                exec.tty = w.tty;
                for fd in [&w.stdin, &w.stdout, &w.stderr].into_iter().flatten() {
                    set_nonblocking(fd.as_raw_fd(), true);
                }
                (exec.stdin, exec.stdout, exec.stderr) = (w.stdin, w.stdout, w.stderr);
                exec.to_conn.extend(&[&run::header(kind::STARTED, 0)]);
            }
            Err(f) => {
                let class = if f.daemon {
                    run::exec_failed::DAEMON
                } else {
                    run::exec_failed::RUNTIME
                };
                let len = u32::try_from(f.message.len() + 1).unwrap_or(u32::MAX);
                exec.to_conn.extend(&[
                    &run::header(kind::SYSTEM_ERR, len),
                    &[class],
                    f.message.as_bytes(),
                ]);
                exec.status = Some(f.status);
            }
        }
        Ok(exec)
    }

    /// Whether everything of it is said: its status known, its output drained, its EXIT
    /// sent, or its connection gone.
    fn finished(&self) -> bool {
        self.status.is_some()
            && self.stdout.is_none()
            && self.stderr.is_none()
            && (self.conn.is_none() || self.ended && self.to_conn.is_empty())
    }
}

/// Whose a polled descriptor is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    Sigchld,
    Host,
    Stdin,
    Signals,
    Stdout,
    Stderr,
    /// An exec's, by its index.
    Exec(usize, Part),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Conn,
    Stdin,
    Stdout,
    Stderr,
}

/// Reads what a pipe or pty holds into `to` as `which` frames; drops `slot` at its end.
fn drain_into(slot: &mut Option<OwnedFd>, which: u8, buf: &mut [u8], to: &mut Outbox) {
    let Some(fd) = slot.as_ref().map(AsRawFd::as_raw_fd) else {
        return;
    };
    match read(fd, buf) {
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
        Ok(0) | Err(_) => *slot = None,
        Ok(n) => to.extend(&[&run::header(which, n as u32), buf.get(..n).unwrap_or_default()]),
    }
}

/// Writes what `to` holds into `slot`; drops the slot, and what it held, if it is closed.
fn feed(slot: &mut Option<OwnedFd>, to: &mut Outbox) {
    if let Some(fd) = slot.as_ref().map(AsRawFd::as_raw_fd) {
        match write(fd, to.pending()) {
            Ok(n) => to.written(n),
            Err(_) => {
                *slot = None;
                to.clear();
            }
        }
    }
}

impl Workload {
    /// Relays stdio until the workload has exited and its output is drained, and every
    /// exec has said all it has, and returns the workload's status. Execs start, are
    /// signalled and resized through the signal connection, each relayed on its own.
    fn relay(mut self, conn: &File, mut signals: Option<File>) -> u32 {
        let mut host = Some(conn.as_raw_fd());
        for fd in [host, self.stdin.as_ref().map(AsRawFd::as_raw_fd)]
            .into_iter()
            .chain([&self.stdout, &self.stderr].map(|f| f.as_ref().map(AsRawFd::as_raw_fd)))
            .flatten()
        {
            set_nonblocking(fd, true);
        }
        let mut from_host: Vec<u8> = Vec::new();
        let (mut signals_connected, mut from_signals) = (false, Vec::new());
        let mut to_stdin = Outbox::default();
        let mut stdin_eof = false;
        let mut to_host = Outbox::default();
        let mut status: Option<u32> = None;
        let mut execs: Vec<Exec> = Vec::new();
        let mut buf = vec![0u8; CHUNK];
        // Reused each turn: six for the workload, four for each exec.
        let (mut set, mut owners) = (Vec::<libc::pollfd>::new(), Vec::<Owner>::new());
        loop {
            let exited = status.is_some();
            execs.retain(|e| !e.finished());
            if exited
                && self.stdout.is_none()
                && self.stderr.is_none()
                && (to_host.is_empty() || host.is_none())
                && execs.is_empty()
            {
                break;
            }
            set.clear();
            owners.clear();
            let mut poll = |fd: Option<RawFd>, events: libc::c_short, owner: Owner| {
                if let (Some(fd), true) = (fd, events != 0) {
                    set.push(libc::pollfd {
                        fd,
                        events,
                        revents: 0,
                    });
                    owners.push(owner);
                }
            };
            poll(Some(self.sigchld.as_raw_fd()), libc::POLLIN, Owner::Sigchld);
            let host_events = if !exited && !stdin_eof && to_stdin.len() < BUFFERED {
                libc::POLLIN
            } else {
                0
            } | if to_host.is_empty() { 0 } else { libc::POLLOUT };
            poll(host, host_events, Owner::Host);
            let stdin_events = if to_stdin.is_empty() { 0 } else { libc::POLLOUT };
            poll(
                self.stdin.as_ref().map(AsRawFd::as_raw_fd),
                stdin_events,
                Owner::Stdin,
            );
            let signal_events = if signals_connected {
                libc::POLLIN
            } else {
                libc::POLLOUT
            };
            poll(
                signals.as_ref().map(AsRawFd::as_raw_fd),
                signal_events,
                Owner::Signals,
            );
            let out_events = if to_host.len() < BUFFERED { libc::POLLIN } else { 0 };
            poll(
                self.stdout.as_ref().map(AsRawFd::as_raw_fd),
                out_events,
                Owner::Stdout,
            );
            poll(
                self.stderr.as_ref().map(AsRawFd::as_raw_fd),
                out_events,
                Owner::Stderr,
            );
            for (i, e) in execs.iter().enumerate() {
                let conn_events = if !e.connected {
                    libc::POLLOUT
                } else {
                    (if e.to_conn.is_empty() { 0 } else { libc::POLLOUT })
                        | if !e.stdin_eof && e.to_stdin.len() < BUFFERED {
                            libc::POLLIN
                        } else {
                            0
                        }
                };
                poll(
                    e.conn.as_ref().map(AsRawFd::as_raw_fd),
                    conn_events,
                    Owner::Exec(i, Part::Conn),
                );
                let stdin_events = if e.to_stdin.is_empty() { 0 } else { libc::POLLOUT };
                poll(
                    e.stdin.as_ref().map(AsRawFd::as_raw_fd),
                    stdin_events,
                    Owner::Exec(i, Part::Stdin),
                );
                let out_events = if e.to_conn.len() < BUFFERED {
                    libc::POLLIN
                } else {
                    0
                };
                poll(
                    e.stdout.as_ref().map(AsRawFd::as_raw_fd),
                    out_events,
                    Owner::Exec(i, Part::Stdout),
                );
                poll(
                    e.stderr.as_ref().map(AsRawFd::as_raw_fd),
                    out_events,
                    Owner::Exec(i, Part::Stderr),
                );
            }
            // SAFETY: valid pollfds, as many as given.
            if unsafe { libc::poll(set.as_mut_ptr(), set.len() as libc::nfds_t, -1) } < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            for (p, &owner) in set.iter().zip(&owners).filter(|(p, _)| p.revents != 0) {
                let fd = p.fd;
                match owner {
                    Owner::Sigchld => {
                        let main = self.pid;
                        reap(&self.sigchld, |pid, code| {
                            if pid == main {
                                let _ = crate::linux::control_write(control::MARKER, marker::WORKLOAD_EXITED);
                                status = Some(code);
                                // SAFETY: kill(2) of every process but init: a container
                                // ends with its main process.
                                unsafe { libc::kill(-1, libc::SIGKILL) };
                            } else if let Some(e) = execs.iter_mut().find(|e| e.pid == pid) {
                                e.status = Some(code);
                                // Its stdin goes with it.
                                e.stdin = None;
                                e.to_stdin.clear();
                            }
                        });
                        if status.is_some() {
                            // The workload's stdin goes with it.
                            self.stdin = None;
                            to_stdin.clear();
                        }
                    }
                    Owner::Host => {
                        if p.revents & libc::POLLOUT != 0 {
                            match write(fd, to_host.pending()) {
                                Ok(n) => to_host.written(n),
                                Err(_) => host = None,
                            }
                        }
                        if p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 && host.is_some() {
                            match read(fd, &mut buf) {
                                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                                // The host is gone: nothing more for stdin.
                                Ok(0) | Err(_) => stdin_eof = true,
                                Ok(n) => {
                                    from_host.extend_from_slice(buf.get(..n).unwrap_or_default());
                                    let mut closed = false;
                                    let whole = each_frame(&mut from_host, |which, payload| {
                                        if which == kind::STDIN {
                                            closed |= payload.is_empty();
                                            to_stdin.extend(&[payload]);
                                        }
                                    });
                                    stdin_eof |= closed || !whole;
                                }
                            }
                        }
                    }
                    Owner::Signals => {
                        if !signals_connected {
                            match connect_result(fd) {
                                Ok(()) => signals_connected = true,
                                Err(_) => signals = None,
                            }
                            continue;
                        }
                        match read(fd, &mut buf) {
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                            Ok(0) | Err(_) => signals = None,
                            Ok(n) => {
                                from_signals.extend_from_slice(buf.get(..n).unwrap_or_default());
                                let pid = self.pid;
                                let running = status.is_none();
                                let pty = self.stdout.as_ref().filter(|_| self.tty).map(AsRawFd::as_raw_fd);
                                let whole = each_frame(&mut from_signals, |which, payload| {
                                    let id_and = |p: &[u8]| {
                                        let (id, rest) = p.split_at_checked(4)?;
                                        Some((u32::from_be_bytes(id.try_into().ok()?), rest.to_vec()))
                                    };
                                    let sig = |p: &[u8]| <[u8; 4]>::try_from(p).map(u32::from_be_bytes).ok();
                                    match which {
                                        kind::RESIZE => {
                                            if let (Some(fd), Some(size)) = (pty, Size::decode(payload)) {
                                                resize(fd, size);
                                            }
                                        }
                                        kind::SIGNAL => {
                                            if let Some(sig) = sig(payload)
                                                && running
                                                && (1..=64).contains(&sig)
                                            {
                                                // SAFETY: kill(2) of our own child, not yet reaped.
                                                unsafe { libc::kill(pid, sig as libc::c_int) };
                                            }
                                        }
                                        kind::EXEC => match Exec::start(payload, running, pid) {
                                            Ok(e) => execs.push(e),
                                            // Said where the host hears it, which otherwise
                                            // waits for the exec's connection (review 8.8).
                                            Err(Some((id, why))) => {
                                                let len = u32::try_from(4 + why.len()).unwrap_or(u32::MAX);
                                                to_host.extend(&[
                                                    &run::header(kind::EXEC_FAILED, len),
                                                    &id.to_be_bytes(),
                                                    why.as_bytes(),
                                                ]);
                                            }
                                            Err(None) => {}
                                        },
                                        kind::EXEC_SIGNAL => {
                                            if let Some((id, rest)) = id_and(payload)
                                                && let Some(sig) = sig(&rest)
                                                && (1..=64).contains(&sig)
                                                && let Some(e) = execs
                                                    .iter()
                                                    .find(|e| e.id == id && e.pid > 0 && e.status.is_none())
                                            {
                                                // SAFETY: kill(2) of our own child, not yet reaped.
                                                unsafe { libc::kill(e.pid, sig as libc::c_int) };
                                            }
                                        }
                                        kind::EXEC_RESIZE => {
                                            if let Some((id, rest)) = id_and(payload)
                                                && let Some(size) = Size::decode(&rest)
                                                && let Some(e) = execs.iter().find(|e| e.id == id && e.tty)
                                                && let Some(fd) = e.stdout.as_ref()
                                            {
                                                resize(fd.as_raw_fd(), size);
                                            }
                                        }
                                        _ => {}
                                    }
                                });
                                if !whole {
                                    signals = None;
                                }
                            }
                        }
                    }
                    Owner::Stdin => feed(&mut self.stdin, &mut to_stdin),
                    Owner::Stdout => drain_into(&mut self.stdout, kind::STDOUT, &mut buf, &mut to_host),
                    Owner::Stderr => drain_into(&mut self.stderr, kind::STDERR, &mut buf, &mut to_host),
                    Owner::Exec(i, part) => {
                        let Some(e) = execs.get_mut(i) else {
                            continue;
                        };
                        match part {
                            Part::Conn => {
                                if !e.connected {
                                    match connect_result(fd) {
                                        Ok(()) => e.connected = true,
                                        Err(_) => e.conn = None,
                                    }
                                    continue;
                                }
                                if p.revents & libc::POLLOUT != 0 {
                                    match write(fd, e.to_conn.pending()) {
                                        Ok(n) => e.to_conn.written(n),
                                        Err(_) => e.conn = None,
                                    }
                                }
                                if p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
                                    && e.conn.is_some()
                                {
                                    match read(fd, &mut buf) {
                                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {}
                                        Ok(0) | Err(_) => e.stdin_eof = true,
                                        Ok(n) => {
                                            e.from_conn.extend_from_slice(buf.get(..n).unwrap_or_default());
                                            let (mut closed, to) = (false, &mut e.to_stdin);
                                            let whole = each_frame(&mut e.from_conn, |which, payload| {
                                                if which == kind::STDIN {
                                                    closed |= payload.is_empty();
                                                    to.extend(&[payload]);
                                                }
                                            });
                                            e.stdin_eof |= closed || !whole;
                                        }
                                    }
                                }
                            }
                            Part::Stdin => feed(&mut e.stdin, &mut e.to_stdin),
                            Part::Stdout => drain_into(&mut e.stdout, kind::STDOUT, &mut buf, &mut e.to_conn),
                            Part::Stderr => drain_into(&mut e.stderr, kind::STDERR, &mut buf, &mut e.to_conn),
                        }
                    }
                }
            }
            if stdin_eof && to_stdin.is_empty() {
                self.stdin = None;
            }
            for e in &mut execs {
                if e.stdin_eof && e.to_stdin.is_empty() {
                    e.stdin = None;
                }
                // Its output drained, its status follows it.
                if let Some(code) = e.status
                    && !e.ended
                    && e.stdout.is_none()
                    && e.stderr.is_none()
                {
                    e.to_conn
                        .extend(&[&run::header(kind::EXIT, 4), &code.to_be_bytes()]);
                    e.ended = true;
                }
            }
        }
        if let Some(fd) = host {
            set_nonblocking(fd, false);
        }
        status.unwrap_or(NOT_RUN)
    }
}

/// Reaps every exited child, telling `exited` each one's pid and status: its code, or
/// 128 and the signal that ended it.
fn reap(sigchld: &OwnedFd, mut exited: impl FnMut(libc::pid_t, u32)) {
    let mut info = [0u8; std::mem::size_of::<libc::signalfd_siginfo>()];
    while read(sigchld.as_raw_fd(), &mut info).is_ok_and(|n| n > 0) {}
    loop {
        let mut st = 0;
        // SAFETY: waitpid(2) with a valid status pointer.
        let pid = unsafe { libc::waitpid(-1, &mut st, libc::WNOHANG) };
        if pid <= 0 {
            return;
        }
        exited(
            pid,
            if libc::WIFSIGNALED(st) {
                128 + libc::WTERMSIG(st) as u32
            } else {
                libc::WEXITSTATUS(st) as u32
            },
        );
    }
}

/// Sizes the pty whose master is `fd`, as the shim resizes a TTY container's
/// (containerd console, tc_unix.go): the kernel signals the terminal's foreground process
/// group only if the size changed (tty_io.c, tty_do_resize). A zero dimension leaves the
/// size alone, as the Docker CLI never sends one (cli/command/container/tty.go).
fn resize(fd: RawFd, size: Size) {
    if size.rows == 0 || size.cols == 0 {
        return;
    }
    let ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads one winsize.
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
}

/// The working directory, made as Docker makes it (moby daemon/container/container.go,
/// SetupWorkingDirectory): missing directories are created 0755, owned by root.
fn workdir(cwd: &[u8]) -> Result<Vec<u8>, Failure> {
    if cwd.is_empty() {
        return Ok(b"/".to_vec());
    }
    if cwd.first() != Some(&b'/') {
        return Err(setup_failed(format!(
            "the working directory {:?} is not absolute",
            String::from_utf8_lossy(cwd)
        )));
    }
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(cwd));
    match std::fs::create_dir_all(path) {
        Ok(()) => Ok(cwd.to_vec()),
        Err(_) if path.exists() && !path.is_dir() => Err(setup_failed(format!(
            "Cannot mkdir: {} is not a directory",
            path.display()
        ))),
        Err(e) => Err(setup_failed(format!("mkdir {}: {e}", path.display()))),
    }
}

/// An exec's working directory as runc's exec takes it: `/` if none, refused unless
/// absolute, never made.
fn exec_cwd(cwd: &[u8]) -> Result<Vec<u8>, Failure> {
    match cwd.first() {
        None => Ok(b"/".to_vec()),
        Some(b'/') => Ok(cwd.to_vec()),
        Some(_) => Err(Failure {
            status: 128,
            message: "Cwd must be an absolute path".into(),
            daemon: false,
        }),
    }
}

/// Why the command did not start, in the words of runc's Go (exec.LookPath, and
/// os.PathError for the rest), which dockerd passes on: `tried` is the file it was
/// executing. The status is `docker run`'s for those words (shards_cmdline).
fn exec_failure(which: u8, errno: i32, argv0: &[u8], tried: &[u8], cwd: &[u8], user: &[u8]) -> Failure {
    use shards_cmdline::go::{linux_error, quote};
    let cmd = String::from_utf8_lossy(argv0);
    let err = linux_error(errno);
    let message = match which {
        step::NOT_IN_PATH => format!("exec: {}: executable file not found in $PATH", quote(&cmd)),
        step::STAT => format!("exec: {}: stat {cmd}: {err}", quote(&cmd)),
        step::ACCESS => format!("exec: {}: {err}", quote(&cmd)),
        step::CHDIR => format!(
            "chdir to cwd ({}) failed: {err}",
            quote(&String::from_utf8_lossy(cwd))
        ),
        step::USER => format!("setting user {}: {err}", quote(&String::from_utf8_lossy(user))),
        step::TTY => format!("open {}: {err}", String::from_utf8_lossy(tried)),
        step::CGROUP => {
            return Failure {
                status: NOT_RUN,
                message: format!("joining the container's cgroup: {err}"),
                daemon: false,
            };
        }
        // runc's words (libcontainer/seccomp, init_linux.go).
        step::SECCOMP | step::NNP => {
            let message = if which == step::SECCOMP {
                format!("error loading seccomp filter into kernel: error loading seccomp filter: {err}")
            } else {
                format!("prctl(SET_NO_NEW_PRIVS): {err}")
            };
            return Failure {
                status: NOT_RUN,
                message,
                daemon: false,
            };
        }
        _ => format!("exec {}: {err}", String::from_utf8_lossy(tried)),
    };
    Failure {
        status: u32::from(shards_cmdline::commands::run_status(&message)),
        message,
        daemon: false,
    }
}

/// What the child needs, built before fork.
struct Child<'a> {
    stdio: [RawFd; 3],
    /// The terminal to open as stdio instead, with the session it controls.
    tty: Option<&'a CString>,
    err: RawFd,
    cwd: &'a CString,
    /// Its capabilities, a bit for each by number.
    caps: u64,
    uid: u32,
    gid: u32,
    groups: &'a [u32],
    candidates: &'a [CString],
    explicit: bool,
    /// The kernel's last capability, for the bounding set's.
    last_cap: u32,
    /// Its seccomp filter and its seccomp(2) flags, and whether no_new_privs is set.
    seccomp: Option<(u32, &'a libc::sock_fprog)>,
    nnp: bool,
    argv: *const *const libc::c_char,
    envp: *const *const libc::c_char,
}

/// The workload's side of the fork, as runc's init runs it (libcontainer/init_linux.go,
/// finalizeNamespace and setupUser; standard_init_linux.go): stdio, session, working
/// directory, user, then the command, found as Go's exec.LookPath finds it.
///
/// # Safety
/// Only in the child of a fork of a single-threaded process; it never returns.
unsafe fn child(c: &Child<'_>) -> ! {
    /// Reports the failed step, errno and the candidate it was trying to the parent, and
    /// exits.
    ///
    /// # Safety
    /// As for `child`.
    unsafe fn report(err: RawFd, which: u8, candidate: usize) -> ! {
        // SAFETY: async-signal-safe calls on a local buffer.
        unsafe {
            let [a, b, c, d] = (*libc::__errno_location()).to_be_bytes();
            let [e, f, g, h] = u32::try_from(candidate).unwrap_or(u32::MAX).to_be_bytes();
            let report = [which, a, b, c, d, e, f, g, h];
            libc::write(err, report.as_ptr().cast(), report.len());
            libc::_exit(127)
        }
    }
    // SAFETY: async-signal-safe calls, on memory prepared before the fork.
    unsafe {
        let fail = |which: u8| report(c.err, which, 0);
        // Signals as a new process has them: none blocked, none ignored. Rust ignores
        // SIGPIPE in init, and execve keeps ignored signals ignored.
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
        for sig in 1..libc::SIGRTMAX() {
            libc::signal(sig, libc::SIG_DFL);
        }
        libc::setsid();
        let stdio = match c.tty {
            // The session's controlling terminal, as runc makes it (TIOCSCTTY after
            // setsid, libcontainer/init_linux.go).
            Some(path) => {
                let fd = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
                if fd < 0 || libc::ioctl(fd, libc::TIOCSCTTY, 0) != 0 {
                    fail(step::TTY);
                }
                [fd; 3]
            }
            None => c.stdio,
        };
        for (i, &fd) in stdio.iter().enumerate() {
            if libc::dup2(fd, i as libc::c_int) < 0 {
                fail(step::EXEC);
            }
        }
        if c.tty.is_some() && stdio[0] > 2 {
            libc::close(stdio[0]);
        }
        // As root first; if root may not, again as the user (runc does the same).
        let mut chdir_ok = libc::chdir(c.cwd.as_ptr()) == 0;
        // seccomp(2) with the filter: SECCOMP_SET_MODE_FILTER, its flags, its program.
        let load = |(flags, prog): (u32, &libc::sock_fprog)| {
            libc::syscall(
                libc::SYS_seccomp,
                1 as libc::c_long,
                libc::c_long::from(flags),
                prog as *const libc::sock_fprog,
            ) == 0
        };
        // Without no_new_privs, loading a filter needs CAP_SYS_ADMIN: before the
        // capabilities go (runc, standard_init_linux.go).
        if !c.nnp
            && let Some(f) = c.seccomp
            && !load(f)
        {
            fail(step::SECCOMP);
        }
        // A container's capabilities (crate::defaults), as runc applies them
        // (finalizeNamespace): the bounding set first, the rest kept across the change of
        // user, then set. For any user but root, execve then leaves none but the
        // bounding set, there being no file or ambient capabilities (capabilities(7)),
        // as Docker's are.
        let keep = |cap: u32| cap < 64 && c.caps & (1 << cap) != 0;
        if !defaults::bound(c.last_cap, keep) || libc::prctl(libc::PR_SET_KEEPCAPS, 1, 0, 0, 0) != 0 {
            fail(step::USER);
        }
        if libc::setgroups(c.groups.len(), c.groups.as_ptr()) != 0
            || libc::setgid(c.gid) != 0
            || libc::setuid(c.uid) != 0
        {
            fail(step::USER);
        }
        libc::prctl(libc::PR_SET_KEEPCAPS, 0, 0, 0, 0);
        if !defaults::set(c.last_cap, keep) {
            fail(step::USER);
        }
        if !chdir_ok {
            chdir_ok = libc::chdir(c.cwd.as_ptr()) == 0;
        }
        if !chdir_ok {
            fail(step::CHDIR);
        }
        // With it, as late as can be: just before the command (runc).
        if c.nnp {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                fail(step::NNP);
            }
            if let Some(f) = c.seccomp
                && !load(f)
            {
                fail(step::SECCOMP);
            }
        }
        for (i, path) in c.candidates.iter().enumerate() {
            // Go's findExecutable: a file that is not a directory, executable by us.
            let mut st: libc::stat = std::mem::zeroed();
            if libc::stat(path.as_ptr(), &mut st) != 0 {
                if c.explicit {
                    fail(step::STAT);
                }
                continue;
            }
            if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
                if c.explicit {
                    *libc::__errno_location() = libc::EISDIR;
                    fail(step::ACCESS);
                }
                continue;
            }
            if libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::X_OK, libc::AT_EACCESS) != 0 {
                if c.explicit {
                    fail(step::ACCESS);
                }
                continue;
            }
            libc::execve(path.as_ptr(), c.argv, c.envp);
            report(c.err, step::EXEC, i);
        }
        fail(step::NOT_IN_PATH)
    }
}

fn set_nonblocking(fd: RawFd, on: bool) {
    // SAFETY: fcntl(2) on a descriptor we own.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            let flags = if on {
                flags | libc::O_NONBLOCK
            } else {
                flags & !libc::O_NONBLOCK
            };
            libc::fcntl(fd, libc::F_SETFL, flags);
        }
    }
}

fn read(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buf` is valid for its length.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    usize::try_from(n).map_err(|_| io::Error::last_os_error())
}

fn write(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: `buf` is valid for its length.
    let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    match usize::try_from(n) {
        Ok(n) => Ok(n),
        Err(_) => {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::WouldBlock {
                Ok(0)
            } else {
                Err(e)
            }
        }
    }
}
