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

/// `/etc/hosts`'s first lines ([`crate::netplan::HOSTS`]); each run adds its own name
/// ([`set_hostname`]).
use crate::netplan::HOSTS;

/// Why the workload did not run, and the status to report.
pub(crate) struct Failure {
    status: u32,
    pub(crate) message: String,
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
    let standby = mount_root(device).and_then(|()| Standby::fork(Born::Own));
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
    let started = standby.and_then(|standby| {
        let spec = receive(&conn)?;
        let (mut workload, declared) = standby.start(&spec)?;
        // A visit to a stopped container's files (`HOLD`, D37) runs nothing of the image's:
        // no command, and no agent or harness.
        if spec.builtin == run::builtin::HOLD {
            return Ok(workload);
        }
        // The image's agents and harnesses, each in its domain (D59), once the run's own
        // files are written: the run replaces /etc/hosts, and replacing a file another
        // mount namespace mounts over detaches that mount (fs/namespace.c,
        // __detach_mounts), which would take a domain's own /etc/hosts away.
        let filters = [
            crate::setup::filter_named(&spec.setup, b"domains-seccomp="),
            crate::setup::filter_named(&spec.setup, b"domains-seccomp-none="),
        ];
        let declared = declared.map_or_else(crate::domains::read, Ok);
        match declared.and_then(|(all, domains, pairs)| {
            crate::domains::start(&all, &domains, &pairs, &filters, &spec.setup)
        }) {
            Ok(domains) if domains.is_empty() => {}
            Ok(domains) => match crate::domains::Memory::watch() {
                Ok(memory) => {
                    workload.domains = domains;
                    workload.memory = Some(memory);
                }
                Err(e) => {
                    // SAFETY: kill(2) of the workload, our own child: the run fails whole.
                    unsafe { libc::kill(workload.pid, libc::SIGKILL) };
                    return Err(setup_failed(e));
                }
            },
            Err(e) => {
                // SAFETY: kill(2) of the workload, our own child: the run fails whole.
                unsafe { libc::kill(workload.pid, libc::SIGKILL) };
                return Err(setup_failed(e));
            }
        }
        Ok(workload)
    });
    let (status, reported) = match started {
        Ok(workload) => {
            let _ = crate::linux::control_write(control::MARKER, marker::WORKLOAD_STARTED);
            let _ = send(&conn, kind::STARTED, &[]);
            workload.relay(&conn, signals)
        }
        Err(f) => {
            let _ = send(&conn, kind::SYSTEM_ERR, f.message.as_bytes());
            (f.status, false)
        }
    };
    // Said by the relay already where containers joined to its network outlived it (D119).
    if !reported {
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
    }
    // The host closes the connection once it has the status. Powering off before then
    // could lose the frame on its way out.
    let _ = shutdown_and_wait(&conn);
    let _ = crate::linux::control_write(control::MARKER, marker::POWERING_OFF);
    power_off()
}

/// Kills every process but init, the kernel's threads, those of the containers joined to
/// the workload's network (D119), each in a cgroup of its own (`join-N`), and `spared`,
/// init's own children working for them: the workload's end ends the rest of its
/// microVM, and its joiners go on. Looked at again until none is left to kill, as one may
/// have forked meanwhile.
fn kill_all_but_joiners(spared: &[libc::pid_t]) {
    // A process forks at most once between looks; each look kills what it finds.
    for _ in 0..64 {
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return;
        };
        let mut found = 0;
        for entry in dir.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<libc::pid_t>().ok())
            else {
                continue;
            };
            if pid <= 2 || spared.contains(&pid) {
                continue;
            }
            // The kernel's threads are kthreadd's (pid 2) children: the fourth field of
            // /proc/PID/stat, after the command in parentheses.
            // And a zombie, already ended, is init's to reap, not to kill: its state, the
            // third field.
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            let mut fields = stat.rsplit_once(')').map(|(_, rest)| rest.split_whitespace());
            let state = fields.as_mut().and_then(Iterator::next);
            let ppid = fields
                .as_mut()
                .and_then(Iterator::next)
                .and_then(|p| p.parse::<libc::pid_t>().ok());
            if ppid == Some(2) || state == Some("Z") {
                continue;
            }
            // Relative to init's cgroup namespace, the root (cgroup_namespaces(7)).
            let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
            if cgroup
                .lines()
                .any(|l| l.split_once("::").is_some_and(|(_, p)| p.starts_with("/join-")))
            {
                continue;
            }
            // SAFETY: kill(2) of a process of the microVM's, neither init nor a joiner's.
            if unsafe { libc::kill(pid, libc::SIGKILL) } == 0 {
                found += 1;
            }
        }
        if found == 0 {
            return;
        }
    }
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
    container_mounts()?;
    container_files()?;
    loopback_up().map_err(|e| setup_failed(e.to_string()))?;
    // A VM with a network: eth0 as the host named it, before any snapshot.
    if let Some((addr, prefix, gateway)) = crate::net::from_cmdline() {
        crate::net::configure(addr, prefix, gateway).map_err(|e| setup_failed(format!("eth0: {e}")))?;
    }
    cgroups()?;
    // Docker's device rules, before any snapshot: a run with them does nothing more.
    crate::devices::confine_by_default(WORKLOAD_CGROUP).map_err(setup_failed)?;
    crate::setup::keep_proc_sys().map_err(|e| setup_failed(format!("/proc/sys: {e}")))?;
    // Docker's own sysctls for a container with a network namespace of its own, where the
    // kernel has them, before any snapshot; a run's `--sysctl` is written after, as
    // dockerd merges the run's over them (moby daemon/oci_linux.go): ICMP echo sockets for
    // every group, without CAP_NET_RAW, and every port bindable without
    // CAP_NET_BIND_SERVICE.
    for (key, value) in [
        ("net.ipv4.ping_group_range", "0 2147483647"),
        ("net.ipv4.ip_unprivileged_port_start", "0"),
    ] {
        if std::path::Path::new(&format!("/proc/sys/{}", key.replace('.', "/"))).exists() {
            crate::setup::write_sysctl(key, value).map_err(setup_failed)?;
        }
    }
    // Last: init writes /proc/sys above, and no more after but through what it kept.
    masked()
}

/// Docker's mounts for a container, in the root this process is in (moby
/// daemon/pkg/oci/defaults.go, a 64 MiB /dev/shm from daemon/config/config.go), and its
/// devices: /dev holds a container's devices alone, not the VM's disks, memory and
/// console. The run's root has them as it boots; a joiner's (D119) as it builds its own.
pub(crate) fn container_mounts() -> Result<(), Failure> {
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
    devices()
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

/// Each joiner's layers (D119), by its exec's id, kept from before its root moved over
/// them, for its diff, size and commit; let go as it ends.
static JOIN_LAYERS: std::sync::Mutex<Vec<(u32, crate::join::Kept)>> = std::sync::Mutex::new(Vec::new());

fn join_layers() -> std::sync::MutexGuard<'static, Vec<(u32, crate::join::Kept)>> {
    JOIN_LAYERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Each joiner's [`Inherited`] (D119), by its exec's id, which its own execs take as the
/// workload's take the workload's; let go as it ends.
static JOINED: std::sync::Mutex<Vec<(u32, Inherited)>> = std::sync::Mutex::new(Vec::new());

fn joined() -> std::sync::MutexGuard<'static, Vec<(u32, Inherited)>> {
    JOINED.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What the image's Agentfile declares that the run keeps from its command and its execs
/// (D115), as the daemon does (`shards_abi::run::beside`): how many agents and harnesses,
/// whose IDs no other process takes, and whether their flows past the microVM cross the
/// command's network namespace.
#[derive(Clone, Copy)]
struct Beside {
    domains: u32,
    uplink: bool,
}

/// The run's [`Beside`], where its image's Agentfile declares any domain.
static BESIDE: std::sync::OnceLock<Beside> = std::sync::OnceLock::new();

impl Beside {
    /// Refuses what of `setup` reaches the domains: a privileged command (`what`, `run`
    /// or `exec`), `/proc` unmasked, the microVM's PID namespace, a capability of
    /// [`shards_abi::run::beside`]'s, a net.* sysctl where their flows cross the
    /// command's network namespace.
    fn refuse(&self, setup: &[Vec<u8>], what: &str) -> Result<(), Failure> {
        use shards_abi::run::beside;
        for entry in setup {
            if entry == b"privileged" {
                return Err(setup_failed(beside::privileged(what)));
            }
            if entry == b"unmasked" {
                return Err(setup_failed(beside::unmasked()));
            }
            if entry == b"pid=host" {
                return Err(setup_failed(beside::pid_host()));
            }
            if let Some(caps) = entry.strip_prefix(b"caps=") {
                let caps = std::str::from_utf8(caps)
                    .ok()
                    .and_then(|c| c.parse::<u64>().ok())
                    .ok_or_else(|| setup_failed("a malformed caps entry"))?;
                if let Some((cap, reach)) = beside::crossing(caps, self.uplink) {
                    let name = run::CAP_NAMES
                        .get(cap as usize)
                        .copied()
                        .unwrap_or("a capability");
                    return Err(setup_failed(beside::capability(name, reach)));
                }
            }
            if let Some(kv) = entry
                .strip_prefix(b"sysctl=")
                .or_else(|| entry.strip_prefix(b"endpoint-sysctl="))
                && self.uplink
                && kv.starts_with(b"net.")
            {
                let kv = String::from_utf8_lossy(kv);
                return Err(setup_failed(beside::sysctl(
                    kv.split_once('=').map_or(&*kv, |(k, _)| k),
                )));
            }
        }
        Ok(())
    }
}

/// Refuses, beside an Agentfile's domains, a process of the run's as any of their IDs
/// (D115): the kernel counts a user's processes, inotify instances, pipe buffers and
/// queued signals as one, and their files are theirs.
fn none_of_theirs(uid: libc::uid_t, gid: libc::gid_t, groups: &[libc::gid_t]) -> Result<(), Failure> {
    if let Some(b) = BESIDE.get() {
        let taken = |id: u32| shards_abi::run::beside::taken(id, b.domains);
        if taken(uid) {
            return Err(setup_failed(shards_abi::run::beside::id("uid", uid, b.domains)));
        }
        if let Some(&g) = std::iter::once(&gid).chain(groups).find(|&&g| taken(g)) {
            return Err(setup_failed(shards_abi::run::beside::id("gid", g, b.domains)));
        }
    }
    Ok(())
}

/// The capabilities a command has where none are said: Docker's defaults, less those the
/// run keeps from its command beside its domains, `CAP_NET_RAW` where their flows past
/// the microVM cross its network namespace (D115).
fn default_caps() -> u64 {
    let all = CAPS.iter().fold(0, |m, &c| m | 1 << c);
    match BESIDE.get() {
        Some(b) if b.uplink => shards_abi::run::beside::UPLINK
            .iter()
            .fold(all, |m, &(c, _)| m & !(1 << c)),
        _ => all,
    }
}

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
        } else if let Some(gateway) = entry.strip_prefix(b"dns=") {
            // Docker's embedded DNS address, relayed to the network's resolver (D46).
            let gateway = std::str::from_utf8(gateway)
                .ok()
                .and_then(|g| g.parse().ok())
                .ok_or_else(|| setup_failed("a malformed dns entry"))?;
            crate::dnsrelay::start(gateway).map_err(setup_failed)?;
        } else if entry.starts_with(b"address=")
            || entry.starts_with(b"address6=")
            || entry == b"confine-eth0"
            || entry == b"init"
            || entry == b"pid=host"
            || entry == b"pid=workload"
            || entry.starts_with(b"pid=joiner=")
            || entry.starts_with(b"endpoint-sysctl=")
        {
            // Init's, as the run starts (`Standby::start`).
        } else if entry.starts_with(b"devices=") {
            // Init's, done as the workload's limits are (`limit`).
        } else {
            if entry.starts_with(b"ulimit=")
                || entry.starts_with(b"oom=")
                || entry.starts_with(b"seccomp=")
                || entry.starts_with(b"seccomp-mounts=")
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
fn stats(cgroup: &str) -> String {
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let cg = |f: &str| read(&format!("{cgroup}/{f}"));
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

/// How long a freeze is waited for, as runc's cgroup v2 freezer waits (libcontainer
/// cgroups/fs2/freezer.go, waitFrozen: 1,000 looks 10 ms apart).
const FREEZE_WAIT: Duration = Duration::from_secs(10);

/// Freezes the cgroup at `dir`, or thaws it, as runc's fs2 freezer does: `cgroup.freeze`
/// written, then, for a freeze, `cgroup.events` read until it says `frozen 1`. Where runc
/// looks every 10 ms, this waits for the kernel's word that the file changed
/// (cgroup-v2.rst: a change of `cgroup.events` is a poll(2) event), within runc's bound
/// and in its words past it.
fn freeze(dir: &str, frozen: bool) -> io::Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(format!("{dir}/cgroup.freeze"))?
        .write_all(if frozen { b"1" } else { b"0" })?;
    if !frozen {
        return Ok(());
    }
    use std::os::unix::fs::FileExt as _;
    let events = File::open(format!("{dir}/cgroup.events"))?;
    let deadline = Instant::now() + FREEZE_WAIT;
    let mut buf = [0u8; 256];
    loop {
        let n = events.read_at(&mut buf, 0)?;
        let text = String::from_utf8_lossy(buf.get(..n).unwrap_or_default()).into_owned();
        if text.lines().any(|l| l == "frozen 1") {
            return Ok(());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::other(format!(
                "timeout of {}s reached waiting for the cgroup to freeze",
                FREEZE_WAIT.as_secs()
            )));
        }
        let mut polled = libc::pollfd {
            fd: events.as_raw_fd(),
            events: libc::POLLPRI,
            revents: 0,
        };
        let ms = libc::c_int::try_from(left.as_millis())
            .unwrap_or(libc::c_int::MAX)
            .max(1);
        // SAFETY: poll(2) on one pollfd of a descriptor this process holds.
        if unsafe { libc::poll(&mut polled, 1, ms) } < 0
            && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
        {
            return Err(io::Error::last_os_error());
        }
    }
}

/// Where the workload's cgroup is, as init sees the hierarchy.
const WORKLOAD_CGROUP: &str = "/sys/fs/cgroup/workload";

/// What the workload's memory limit allows it beyond what it holds, in bytes: none where
/// it has no limit.
pub fn workload_headroom() -> u64 {
    let read = |f: &str| {
        std::fs::read_to_string(format!("{WORKLOAD_CGROUP}/{f}"))
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
    };
    match (read("memory.max"), read("memory.current")) {
        (Some(max), Some(current)) => max.saturating_sub(current),
        _ => 0,
    }
}

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
fn isolate(join: Option<libc::pid_t>, joiner: bool) -> Result<(), i32> {
    let errno = |e: io::Error| e.raw_os_error().unwrap_or(libc::EIO);
    // A joiner's exec joins the joiner's own cgroup (D119), which /proc names.
    let cgroup = match join.filter(|_| joiner) {
        Some(pid) => joiner_cgroup(pid)?,
        None => WORKLOAD_CGROUP.to_string(),
    };
    std::fs::OpenOptions::new()
        .write(true)
        .open(format!("{cgroup}/cgroup.procs"))
        .and_then(|mut f| f.write_all(b"0"))
        .map_err(errno)?;
    let last = || io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
    if let Some(pid) = join {
        // A joiner's UTS, IPC and network namespaces too, which it has of its own (D119).
        let all: &[(&str, libc::c_int)] = if joiner {
            &[
                ("cgroup", libc::CLONE_NEWCGROUP),
                ("uts", libc::CLONE_NEWUTS),
                ("ipc", libc::CLONE_NEWIPC),
                ("net", libc::CLONE_NEWNET),
                ("mnt", libc::CLONE_NEWNS),
            ]
        } else {
            &[("cgroup", libc::CLONE_NEWCGROUP), ("mnt", libc::CLONE_NEWNS)]
        };
        // Every one opened before any is entered: once the mount namespace is, `/proc` may
        // be one whose PIDs are not init's.
        let mut fds = Vec::with_capacity(all.len());
        for (ns, kind) in all {
            let file = File::open(format!("/proc/{pid}/ns/{ns}")).map_err(errno)?;
            fds.push((file, *kind));
        }
        for (file, kind) in &fds {
            // SAFETY: setns(2) on a descriptor this process holds; a single-threaded fork
            // of init.
            if unsafe { libc::setns(file.as_raw_fd(), *kind) } != 0 {
                return Err(last());
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
    own_proc()
}

/// The cgroup joiner `pid`'s process is in (D119), as `/proc/PID/cgroup` names it from
/// init's cgroup namespace, the root: `0::/join-N`. Returns the errno of what failed.
fn joiner_cgroup(pid: libc::pid_t) -> Result<String, i32> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .map_err(|e| e.raw_os_error().unwrap_or(libc::EIO))?;
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .filter(|p| p.starts_with("/join-") && !p.contains(".."))
        .map(|p| format!("/sys/fs/cgroup{p}"))
        .ok_or(libc::ENOENT)
}

/// A joiner's working directory `cwd`, made where it is missing, as [`workdir`] makes a
/// run's. Returns the errno of what failed: ENOTDIR where something else is there.
fn make_workdir(cwd: &CString) -> Result<(), i32> {
    let path = std::path::Path::new(std::ffi::OsStr::from_bytes(cwd.as_bytes()));
    match std::fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(_) if path.exists() && !path.is_dir() => Err(libc::ENOTDIR),
        Err(e) => Err(e.raw_os_error().unwrap_or(libc::EIO)),
    }
}

/// A `/proc` of the PID namespace this process was born in, the workload's own (D115), in
/// place of init's, which shows every process of the microVM, its agents' among them;
/// masked and read-only where init's is (`masked`), as runc mounts a container's.
fn own_proc() -> Result<(), i32> {
    let last = || io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
    let (nosuid, noexec, nodev) = (libc::MS_NOSUID, libc::MS_NOEXEC, libc::MS_NODEV);
    // SAFETY: umount2(2) and mount(2) of NUL-terminated literals.
    unsafe {
        if libc::umount2(c"/proc".as_ptr(), libc::MNT_DETACH) != 0
            || libc::mount(
                c"proc".as_ptr(),
                c"/proc".as_ptr(),
                c"proc".as_ptr(),
                nosuid | noexec | nodev,
                std::ptr::null(),
            ) != 0
        {
            return Err(last());
        }
    }
    let mount = |src: &str, dst: &str, fstype: &str, flags: libc::c_ulong| -> Result<(), i32> {
        let (src, dst, fstype) = (
            CString::new(src).map_err(|_| libc::EINVAL)?,
            CString::new(dst).map_err(|_| libc::EINVAL)?,
            CString::new(fstype).map_err(|_| libc::EINVAL)?,
        );
        // SAFETY: mount(2) of NUL-terminated strings that outlive the call.
        if unsafe {
            libc::mount(
                src.as_ptr(),
                dst.as_ptr(),
                fstype.as_ptr(),
                flags,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(last());
        }
        Ok(())
    };
    for p in MASKED.iter().filter(|p| p.starts_with("/proc/")) {
        match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => mount("tmpfs", p, "tmpfs", libc::MS_RDONLY)?,
            Ok(_) => mount("/dev/null", p, "", libc::MS_BIND)?,
            Err(_) => {}
        }
    }
    for p in READONLY.iter().filter(|p| p.starts_with("/proc/")) {
        if std::fs::symlink_metadata(p).is_ok() {
            mount(p, p, "", libc::MS_BIND | libc::MS_REC)?;
            let flags = libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | nosuid | noexec | nodev;
            mount("", p, "", flags)?;
        }
    }
    Ok(())
}

/// Init's own PID namespace, which it takes back after each child it makes in the
/// workload's ([`fork_in`]).
static OWN_PID_NS: std::sync::OnceLock<OwnedFd> = std::sync::OnceLock::new();

/// The PID namespace `pid` runs in.
fn pid_ns_of(pid: libc::pid_t) -> Result<OwnedFd, Failure> {
    File::open(format!("/proc/{pid}/ns/pid"))
        .map(OwnedFd::from)
        .map_err(|e| setup_failed(format!("the PID namespace it joins: {e}")))
}

/// fork(2), the child born in the PID namespace `ns`, or, with none, in a new one whose
/// first process it is: setns(2) and unshare(2) give the children made after them
/// (pid_namespaces(7)), and init takes its own back before anything else. Init's later
/// children, its agents' domains among them, must not be born there, where the workload
/// would see them: past a failure to take it back, init goes no further.
fn fork_in(ns: Option<&OwnedFd>) -> Result<libc::pid_t, Failure> {
    let own = match OWN_PID_NS.get() {
        Some(own) => own,
        None => {
            let own = File::open("/proc/self/ns/pid")
                .map(OwnedFd::from)
                .map_err(|e| setup_failed(format!("init's PID namespace: {e}")))?;
            OWN_PID_NS.get_or_init(|| own)
        }
    };
    let entered = match ns {
        // SAFETY: setns(2) on a descriptor this process holds.
        Some(ns) => unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWPID) },
        // SAFETY: unshare(2) of the namespace init's next child is born in.
        None => unsafe { libc::unshare(libc::CLONE_NEWPID) },
    };
    if entered != 0 {
        return Err(setup_failed(format!(
            "the workload's PID namespace: {}",
            io::Error::last_os_error()
        )));
    }
    // SAFETY: init is single-threaded, so its child may run anything until it execs.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        return Ok(0);
    }
    let forked = io::Error::last_os_error();
    // SAFETY: setns(2) back into init's own namespace, which it holds.
    if unsafe { libc::setns(own.as_raw_fd(), libc::CLONE_NEWPID) } != 0 {
        let _ = writeln!(
            io::stderr(),
            "shards-init: taking back its own PID namespace: {}",
            io::Error::last_os_error()
        );
        power_off()
    }
    if pid < 0 {
        return Err(setup_failed(format!("fork: {forked}")));
    }
    Ok(pid)
}

/// A new PID namespace for the workload under `--init` (D115), its first process
/// [`reaper`]'s, its command the second; like the namespace a command is PID 1 of
/// ([`Born::Own`]), it shows the workload and its execs none of the microVM's other
/// processes, an agent's or init's, as a container sees none of its host's.
fn clone_reaper(uid: libc::uid_t, gid: libc::gid_t) -> Result<libc::pid_t, Failure> {
    let (pid, ready) = spawn_reaper(WORKLOAD_CGROUP, Ids::Known(uid, gid), "the workload's")?;
    await_reaper(pid, ready, "the workload's")
}

/// The IDs a [`reaper`] takes: as it is born, or sent on a pipe, `uid` then `gid`, four
/// bytes each, big-endian, before its namespace's command may start: a joiner's (D119),
/// whose user its own image names, which its standby, born in the reaper's namespace,
/// reads as it builds its root.
#[derive(Clone, Copy)]
enum Ids {
    Known(libc::uid_t, libc::gid_t),
    Sent(RawFd),
}

/// A [`reaper`] born in `cgroup`, in a PID namespace of its own; with the read end of the
/// pipe it closes once it is ready, for [`await_reaper`]. `whose` namespace it is, as
/// its failures say.
fn spawn_reaper(cgroup: &str, ids: Ids, whose: &str) -> Result<(libc::pid_t, OwnedFd), Failure> {
    let failed = |e: io::Error| setup_failed(format!("{whose} PID namespace: {e}"));
    let cgroup = File::open(cgroup).map_err(failed)?;
    let last_cap = defaults::last_cap();
    let mut args = crate::domains::CloneArgs {
        flags: (libc::CLONE_NEWPID | libc::CLONE_NEWNS) as u64 | crate::domains::CLONE_INTO_CGROUP,
        exit_signal: libc::SIGCHLD as u64,
        cgroup: cgroup.as_raw_fd() as u64,
        ..Default::default()
    };
    // Closed by the reaper once it is ready, with every descriptor of init's it holds.
    let (ready_r, ready_w) = pipe()?;
    // SAFETY: clone3(2) as fork(2), with a clone_args of the size given; the child calls
    // only the kernel ([`reaper`]), never musl's thread list, which is init's.
    let pid = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &raw mut args,
            std::mem::size_of::<crate::domains::CloneArgs>(),
        )
    };
    if pid == 0 {
        reaper(last_cap, ids, ready_w.as_raw_fd())
    }
    if pid < 0 {
        return Err(failed(io::Error::last_os_error()));
    }
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| setup_failed(format!("{whose} PID namespace: a pid out of range")))?;
    drop(ready_w);
    Ok((pid, ready_r))
}

/// Waits for reaper `pid` to say it is ready on `ready`: its PID, or why it is not, the
/// reaper then ended.
fn await_reaper(pid: libc::pid_t, ready: OwnedFd, whose: &str) -> Result<libc::pid_t, Failure> {
    let failed = |e: io::Error| setup_failed(format!("{whose} PID namespace: {e}"));
    // No process of the container's is born in its namespace before it is ready, as none
    // is in docker-init's before runc has made it the command's user: one would find it
    // root, with init's privileges and descriptors, be refused signalling it as the
    // command's user (kill(2)), and lose a signal it sent before the reaper blocked its
    // own, which a namespace's first process discards where it neither handles nor
    // blocks it (kernel/signal.c, sig_task_ignored). Its end closes as it is ready, or as
    // it ends.
    let mut ready = File::from(ready);
    let mut byte = [0u8; 1];
    let waited = loop {
        match ready.read(&mut byte) {
            Ok(0) => break Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => break Err(failed(e)),
        }
    };
    let mut status = 0;
    // SAFETY: waitpid(2) for our own child, without blocking.
    let gone = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } == pid;
    match waited {
        Ok(()) if !gone => Ok(pid),
        Ok(()) => Err(setup_failed(format!(
            "{whose} PID namespace: its reaper could not start"
        ))),
        Err(e) => {
            // SAFETY: kill(2) and waitpid(2) of our own child, not yet waited for.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
            Err(e)
        }
    }
}

/// The command's PID in the namespace of a [`reaper`], the second process born there.
const REAPED: libc::pid_t = 2;

/// The run's [`reaper`], under `--init`: the container's process `top` lists, as `docker
/// top` lists docker-init.
static REAPER: std::sync::OnceLock<libc::pid_t> = std::sync::OnceLock::new();

/// The signals a [`reaper`] leaves unblocked, as docker-init (tini) leaves them: those
/// the kernel raises for a fault, and job control's (tini.c, configure_signals).
const UNFORWARDED: [libc::c_int; 9] = [
    libc::SIGFPE,
    libc::SIGILL,
    libc::SIGSEGV,
    libc::SIGBUS,
    libc::SIGABRT,
    libc::SIGTRAP,
    libc::SIGSYS,
    libc::SIGTTIN,
    libc::SIGTTOU,
];

/// The first process of the workload's PID namespace under `--init`, docker-init's part,
/// as tini does it: it reaps what the workload leaves orphaned there, and forwards to the
/// command each signal sent to it, PID 1, such as a process of the namespace sends with
/// `kill 1`. The command stays init's child, signalled and waited for as without it. As
/// tini runs as the command's user, so does it, `uid` and `gid`: each may signal the
/// other (kill(2)). In the workload's cgroup, it holds nothing a process of the namespace
/// could take from it: no descriptor, no capability, `no_new_privs`, and an empty
/// read-only root in a mount namespace of its own, so that its `/proc/1/root` leads to
/// no file of init's (D115); and it is not dumpable, so that only a tracer with
/// CAP_SYS_PTRACE, which the run refuses beside an Agentfile's domains, could trace it
/// (ptrace(2)). No process is born in its namespace before it is so ([`clone_reaper`]).
/// A copy of init's memory before any run's. Ended with the run (`relay`).
fn reaper(last_cap: u32, ids: Ids, ready: RawFd) -> ! {
    // SAFETY: system calls on local values and literals alone: mount(2), chroot(2) and
    // chdir(2) to an empty root; for IDs sent, dup2(2) of the two pipes it holds,
    // close_range(2) of the rest and read(2) into a local buffer; the command's IDs, then
    // prctl(2) and capset(2) to none of init's privileges; its signals blocked;
    // close_range(2) of every descriptor past stdio, which says to init that it is ready
    // ([`await_reaper`]); then waitpid(2), sigwaitinfo(2) and kill(2) for ever. Its
    // signals stay blocked, so that one sent between the waits waits for sigwaitinfo.
    unsafe {
        let shut = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
        let alone = libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        ) == 0
            && libc::mount(
                c"tmpfs".as_ptr(),
                c"/proc".as_ptr(),
                c"tmpfs".as_ptr(),
                shut,
                std::ptr::null(),
            ) == 0
            && libc::chroot(c"/proc".as_ptr()) == 0
            && libc::chdir(c"/".as_ptr()) == 0;
        if !alone {
            // Its namespace ends with it: no standby is born there, and the run fails.
            libc::_exit(1);
        }
        let (uid, gid) = match ids {
            Ids::Known(uid, gid) => (uid, gid),
            // A joiner's (D119): init's descriptors let go of at once but the pipe it is
            // ready on and the one its IDs come on, as 3 and 4; then its IDs, as init sends
            // them once the joiner's standby has read its image's users. Their writer's end
            // closing first is its end.
            Ids::Sent(sent) => {
                if libc::dup2(ready, 3) < 0 || libc::dup2(sent, 4) < 0 {
                    libc::_exit(1);
                }
                libc::syscall(
                    libc::SYS_close_range,
                    5 as libc::c_long,
                    libc::c_long::from(u32::MAX),
                    0 as libc::c_long,
                );
                let mut got = [0u8; 8];
                let mut have = 0usize;
                while have < got.len() {
                    let n = libc::read(4, got.as_mut_ptr().add(have).cast(), got.len() - have);
                    if n > 0 {
                        have += n as usize;
                    } else if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    } else {
                        libc::_exit(1);
                    }
                }
                let [u0, u1, u2, u3, g0, g1, g2, g3] = got;
                (
                    u32::from_be_bytes([u0, u1, u2, u3]),
                    u32::from_be_bytes([g0, g1, g2, g3]),
                )
            }
        };
        let dropped = defaults::bound(last_cap, |_| false)
            && defaults::take_ids(&[], gid, uid)
            && defaults::set(last_cap, |_| false)
            && libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) == 0
            && libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0;
        if !dropped {
            libc::_exit(1);
        }
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut set);
        for s in UNFORWARDED {
            libc::sigdelset(&mut set, s);
        }
        libc::sigprocmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
        libc::syscall(
            libc::SYS_close_range,
            3 as libc::c_long,
            libc::c_long::from(u32::MAX),
            0 as libc::c_long,
        );
        loop {
            while libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) > 0 {}
            let signal = libc::sigwaitinfo(&set, std::ptr::null_mut());
            if signal > 0 && signal != libc::SIGCHLD {
                libc::kill(REAPED, signal);
            }
        }
    }
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
/// WriteFile does, and in its words when the kernel refuses one; with its devices' I/O
/// limits and filter, in the order runc's fs2 Set writes them all: pids, memory and the
/// weight (the list's head), the devices' I/O, CPU, the filter, then cpusets.
fn limit(cgroup: &[Vec<u8>], devices: Option<&crate::devices::Prepared>) -> Result<(), Failure> {
    limit_in(WORKLOAD_CGROUP, cgroup, devices, None)
}

/// [`limit`] on the cgroup at `dir`: the workload's, or joiner `joiner`'s (D119), whose
/// devices take the place of its own default filter.
fn limit_in(
    dir: &str,
    cgroup: &[Vec<u8>],
    devices: Option<&crate::devices::Prepared>,
    joiner: Option<u32>,
) -> Result<(), Failure> {
    let hooks = |e: String| setup_failed(format!("error setting cgroup config for procHooks process: {e}"));
    let at = |p: &dyn Fn(&[u8]) -> bool| cgroup.iter().position(|e| p(e)).unwrap_or(cgroup.len());
    let cpu = at(&|e| e.starts_with(b"cpu.") || e.starts_with(b"cpuset."));
    let cpuset = at(&|e| e.starts_with(b"cpuset.")).max(cpu);
    let (head, rest) = cgroup
        .split_at_checked(cpu)
        .ok_or_else(|| hooks("a misplaced limit".into()))?;
    let (cpus, cpusets) = rest
        .split_at_checked(cpuset - cpu)
        .ok_or_else(|| hooks("a misplaced limit".into()))?;
    write_cgroup_in(dir, head).map_err(hooks)?;
    if let Some(d) = devices {
        d.write_io(dir).map_err(hooks)?;
    }
    write_cgroup_in(dir, cpus).map_err(hooks)?;
    if let Some(d) = devices {
        match joiner {
            Some(id) => crate::join::attach_devices(id, d),
            None => d.attach(dir),
        }
        .map_err(hooks)?;
    }
    write_cgroup_in(dir, cpusets).map_err(hooks)
}

/// Writes each `FILE=VALUE` of `cgroup` to the cgroup at `dir`, the workload's or a
/// joiner's (D119), as runc's fs2 writes them; what failed, in runc's words.
pub(crate) fn write_cgroup_at(dir: &str, cgroup: &[Vec<u8>]) -> Result<(), String> {
    write_cgroup_in(dir, cgroup)
}

fn write_cgroup_in(dir: &str, cgroup: &[Vec<u8>]) -> Result<(), String> {
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
        let (file, value) =
            if file == "io.bfq.weight" && std::fs::metadata(format!("{dir}/io.bfq.weight")).is_err() {
                let weight: u64 = value.parse().unwrap_or(0);
                converted = (1 + weight.saturating_sub(10) * 9999 / 990).to_string();
                ("io.weight", converted.as_str())
            } else {
                (file, value)
            };
        let path = format!("{dir}/{file}");
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
pub(crate) fn masked() -> Result<(), Failure> {
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
    let own = crate::net::current().map(|(addr, _, _)| addr.to_string());
    let mut full = name.to_vec();
    if !domain.is_empty() {
        full.push(b'.');
        full.extend_from_slice(domain);
    }
    let mut names = full.clone();
    if let Some(dot) = full.iter().position(|&b| b == b'.') {
        names.push(b' ');
        names.extend_from_slice(full.get(..dot).unwrap_or_default());
    }
    // Its IPv6 address after its IPv4 one, on a network with IPv6 (D99), each line the
    // same names, as makeHostsRecs writes one per address of the container's.
    let own6 = crate::net::current6().map(|(addr, _, _)| addr.to_string());
    for addr in [
        own.as_deref().map(str::as_bytes).or(Some(OWN_ADDRESS)),
        own6.as_deref().map(str::as_bytes),
    ]
    .into_iter()
    .flatten()
    {
        hosts.extend_from_slice(addr);
        hosts.push(b'\t');
        hosts.extend_from_slice(&names);
        hosts.push(b'\n');
    }
    write_file("/etc/hosts", &hosts)
}

/// Brings the loopback interface up, as every container's network namespace has it,
/// `--network none` included: the kernel then gives it 127.0.0.1/8 and ::1
/// (netdevice(7), SIOCSIFFLAGS).
pub(crate) fn loopback_up() -> io::Result<()> {
    lo_up().map_err(|what| {
        let e = io::Error::last_os_error();
        io::Error::new(e.kind(), format!("bringing up lo: {what}: {e}"))
    })
}

/// [`loopback_up`] without allocating, for a process just cloned (D59): the step that
/// failed, its errno left as it was.
pub(crate) fn loopback_up_raw() -> bool {
    lo_up().is_ok()
}

fn lo_up() -> Result<(), &'static str> {
    let failed = |what: &'static str| what;
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
    /// Making a joiner's working directory (D119), as [`super::workdir`] makes a run's.
    pub const MKDIR: u8 = 11;
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
    /// The image's agents and harnesses, started before it (D59): their output is relayed
    /// on its stderr, each line prefixed with the domain.
    domains: Vec<crate::domains::Started>,
    /// Their memory, watched, where there are any.
    memory: Option<crate::domains::Memory>,
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
    born: Born,
    /// The first process of the PID namespace it was born in, its reaper, under `--init`
    /// (D115).
    reaper: Option<libc::pid_t>,
    /// A joiner's reaper's (D119): where init sends its IDs, and where it says it is ready
    /// ([`reaper_ready`]).
    reaper_ids: Option<(OwnedFd, OwnedFd)>,
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

/// Where a standby is born, and so its command (D115).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Born {
    /// The first process of a PID namespace of its own, its command that namespace's PID 1
    /// as a container's is: the kernel ends the namespace, and its execs, with it.
    Own,
    /// The second of one whose first is a [`reaper`], docker-init's part (`--init`), as
    /// the command's user and group, these.
    Reaped(libc::uid_t, libc::gid_t),
    /// In the microVM's own, init's (`--pid host`).
    Host,
    /// In the namespaces of this process, the workload an exec runs beside.
    Beside(libc::pid_t),
    /// In the namespaces of this joiner's process (D119), an exec of its: its UTS, IPC and
    /// network namespaces too, which a joiner has of its own.
    InJoiner(libc::pid_t),
    /// A joiner's (D119): the first process of a PID namespace of its own, which builds
    /// its root and namespaces before its orders; init's exec `u32`, its cgroup's name.
    Joined(crate::join::Join, u32),
}

/// Where a standby is isolated before its orders.
enum Isolation {
    /// The workload's: its cgroup, and cgroup and mount namespaces of its own ([`isolate`]).
    Workload,
    /// An exec's: the namespaces of this process ([`isolate`]); of a joiner's (`whole`),
    /// its UTS, IPC and network namespaces too.
    Beside(libc::pid_t, bool),
    /// A joiner's (D119): its root and namespaces built (crate::join::build), and its
    /// image's users told to init on `report`.
    Joined {
        join: crate::join::Join,
        cgroup: String,
        report: OwnedFd,
    },
}

/// The standby's ends of its pipes, and init's, which it closes.
struct Ends {
    orders: OwnedFd,
    stdio: [OwnedFd; 3],
    err: OwnedFd,
    inits: [OwnedFd; 6],
}

impl Standby {
    /// A standby born as `born`, with the image's user database.
    fn fork(born: Born) -> Result<Standby, Failure> {
        let users = (
            std::fs::read("/etc/passwd").ok(),
            std::fs::read("/etc/group").ok(),
        );
        Standby::forked(born, users)
    }

    /// Its place taken by a standby born as `born`, with the user database it read: it
    /// is ended first, killed with its reaper and waited for, but where it has `ended`
    /// already, waited for by [`Standby::start`].
    fn reborn(mut self, born: Born, ended: bool) -> Result<Standby, Failure> {
        let users = (self.passwd.take(), self.group.take());
        let own = (!ended).then_some(self.pid);
        for pid in [own, self.reaper].into_iter().flatten() {
            // SAFETY: kill(2) and waitpid(2) of init's own child, not yet waited for.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
        }
        drop(self);
        Standby::forked(born, users)
    }

    /// Under `--init`, its reaper first, before any pipe of the standby's, so that the
    /// reaper never holds one: init waits for the last writer of the standby's error pipe
    /// to close it to know the command started (`launch`). A reaper whose standby was not
    /// born is ended.
    fn forked(born: Born, users: (Option<Vec<u8>>, Option<Vec<u8>>)) -> Result<Standby, Failure> {
        let reaper = match born {
            Born::Reaped(uid, gid) => Some(clone_reaper(uid, gid)?),
            _ => None,
        };
        Standby::reaped(born, reaper, users).inspect_err(|_| {
            if let Some(reaper) = reaper {
                // SAFETY: kill(2) and waitpid(2) of init's own child, not yet waited for.
                unsafe {
                    libc::kill(reaper, libc::SIGKILL);
                    libc::waitpid(reaper, std::ptr::null_mut(), 0);
                }
            }
        })
    }

    fn reaped(
        born: Born,
        reaper: Option<libc::pid_t>,
        (passwd, group): (Option<Vec<u8>>, Option<Vec<u8>>),
    ) -> Result<Standby, Failure> {
        let mut reaper = reaper;
        let mut reaper_ids = None;
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
        // A joiner's cgroup and its report, made before it is born (D119).
        let joined = match born {
            Born::Joined(join, id) => Some((
                join,
                crate::join::make_cgroup(id).map_err(setup_failed)?,
                crate::join::report_channel().map_err(setup_failed)?,
            )),
            _ => None,
        };
        // The PID namespace it is born in (D115).
        let (pid, isolation) = match born {
            Born::Own => (fork_in(None)?, Isolation::Workload),
            Born::Reaped(..) => {
                let reaper = reaper.ok_or_else(|| setup_failed("the workload's PID namespace: no reaper"))?;
                (fork_in(Some(&pid_ns_of(reaper)?))?, Isolation::Workload)
            }
            Born::Host => {
                // SAFETY: init is single-threaded, so its child may run anything until it
                // execs.
                let pid = unsafe { libc::fork() };
                if pid < 0 {
                    return Err(setup_failed(format!("fork: {}", io::Error::last_os_error())));
                }
                (pid, Isolation::Workload)
            }
            Born::Beside(workload) => (
                fork_in(Some(&pid_ns_of(workload)?))?,
                Isolation::Beside(workload, false),
            ),
            Born::InJoiner(joiner) => (
                fork_in(Some(&pid_ns_of(joiner)?))?,
                Isolation::Beside(joiner, true),
            ),
            Born::Joined(..) => {
                let (join, cgroup, report) = match &joined {
                    Some((join, cgroup, (_, w))) => (
                        *join,
                        cgroup.clone(),
                        w.try_clone()
                            .map_err(|e| setup_failed(format!("a pipe for joining: {e}")))?,
                    ),
                    None => return Err(setup_failed("a joiner without its cgroup")),
                };
                let pid = match join.pid {
                    // docker-init's part first under `--init`, as the workload's (D115), in
                    // its cgroup: its IDs, its command's, sent once its standby, born in
                    // its namespace, has read its image's users (`reaper_ready`).
                    crate::join::Pid::Own if join.init => {
                        let (sent, send) = pipe()?;
                        let (first, ready) =
                            spawn_reaper(&cgroup, Ids::Sent(sent.as_raw_fd()), "the joiner's")?;
                        drop(sent);
                        let born = pid_ns_of(first).and_then(|ns| fork_in(Some(&ns)));
                        let pid = born.inspect_err(|_| {
                            // SAFETY: kill(2) and waitpid(2) of init's own child, not yet
                            // waited for.
                            unsafe {
                                libc::kill(first, libc::SIGKILL);
                                libc::waitpid(first, std::ptr::null_mut(), 0);
                            }
                        })?;
                        reaper = Some(first);
                        reaper_ids = Some((send, ready));
                        pid
                    }
                    crate::join::Pid::Own => fork_in(None)?,
                    crate::join::Pid::Workload => fork_in(Some(&pid_ns_of(join.workload)?))?,
                    crate::join::Pid::Joiner(other) => fork_in(Some(&pid_ns_of(other)?))?,
                    crate::join::Pid::Host => {
                        // SAFETY: init is single-threaded, so its child may run anything
                        // until it execs.
                        let pid = unsafe { libc::fork() };
                        if pid < 0 {
                            return Err(setup_failed(format!("fork: {}", io::Error::last_os_error())));
                        }
                        pid
                    }
                };
                (pid, Isolation::Joined { join, cgroup, report })
            }
        };
        if pid == 0 {
            standby(
                Ends {
                    orders: orders_r,
                    stdio: [stdin_r, stdout_w, stderr_w],
                    err: err_w,
                    inits: [stdin_w, stdout_r, stderr_r, err_r, orders_w, sigchld],
                },
                isolation,
            )
        }
        drop((stdin_r, stdout_w, stderr_w, err_w, orders_r, isolation));
        // A joiner's root, built or not: its image's users, or why it has none.
        let (passwd, group) = match joined {
            Some((_, _, (r, w))) => {
                drop(w);
                match crate::join::reported(r) {
                    Ok((users, kept)) => {
                        if let Born::Joined(_, id) = born {
                            join_layers().push((id, kept));
                        }
                        users
                    }
                    Err(message) => {
                        // SAFETY: waits for our own child, which exits after reporting.
                        unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
                        if let Born::Joined(_, id) = born {
                            if let Some(first) = reaper {
                                // SAFETY: kill(2) and waitpid(2) of init's own child, not
                                // yet waited for.
                                unsafe {
                                    libc::kill(first, libc::SIGKILL);
                                    libc::waitpid(first, std::ptr::null_mut(), 0);
                                }
                            }
                            crate::join::remove_cgroup(id);
                        }
                        return Err(setup_failed(message));
                    }
                }
            }
            None => (passwd, group),
        };
        Ok(Standby {
            pid,
            born,
            reaper,
            reaper_ids,
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
    /// standby that has ended is replaced first. With it, the image's Agentfile as read
    /// before the workload starts, for a run that starts its domains (D115).
    fn start(self, spec: &Spec) -> Result<(Workload, Option<crate::domains::Read>), Failure> {
        // A network's address, where it is not the template's (D46): before /etc/hosts,
        // which names it.
        if let Some(to) = spec.setup.iter().find_map(|e| e.strip_prefix(b"address=")) {
            let to = std::str::from_utf8(to).ok().and_then(crate::net::parse);
            let from = crate::net::current();
            if let (Some(to), Some(from)) = (to, from)
                && to != from
            {
                crate::net::readdress((from.0, from.1), to)
                    .map_err(|e| setup_failed(format!("eth0's address: {e}")))?;
            }
        }
        // Its IPv6 address, on a network with IPv6 (D99).
        if let Some(to) = spec.setup.iter().find_map(|e| e.strip_prefix(b"address6=")) {
            let to = std::str::from_utf8(to)
                .ok()
                .and_then(crate::net::parse6)
                .ok_or_else(|| setup_failed("a malformed address6 entry"))?;
            crate::net::address6(to).map_err(|e| setup_failed(format!("eth0's IPv6 address: {e}")))?;
        }
        // The image's Agentfile, for a run that starts its domains (the daemon sends their
        // filter, D59), read now that eth0 has its address and before the command starts,
        // so that the run gives its command nothing that reaches them (D115), as the
        // daemon refuses it: what the image declares, in the image as built (D109).
        let declared = match crate::setup::filter_named(&spec.setup, b"domains-seccomp=") {
            Some(_) if spec.builtin != run::builtin::HOLD => {
                Some(crate::domains::read().map_err(setup_failed)?)
            }
            _ => None,
        };
        if let Some((all, domains, _)) = &declared
            && !all.is_empty()
        {
            let beside = Beside {
                domains: u32::try_from(all.len()).map_err(|_| setup_failed("more domains than uids"))?,
                uplink: domains.iter().any(crate::domains::uplinked),
            };
            beside.refuse(&spec.setup, "run")?;
            let _ = BESIDE.set(beside);
        }
        // Where its command is born (D115): where the template's standby was, PID 1 of a
        // namespace of its own, but for `--init` and `--pid host`.
        let born = if spec.builtin == run::builtin::HOLD {
            Born::Own
        } else if spec.setup.iter().any(|e| e == b"pid=host") {
            Born::Host
        } else if spec.setup.iter().any(|e| e == b"init") {
            // As the command's user, as docker-init runs, so that each may signal the
            // other; refused before it is born where the command's would be (`launch`).
            let ExecUser { uid, gid, .. } =
                user::resolve(&spec.user, self.passwd.as_deref(), self.group.as_deref())
                    .map_err(setup_failed)?;
            none_of_theirs(uid, gid, &[])?;
            Born::Reaped(uid, gid)
        } else {
            Born::Own
        };
        let mut status = 0;
        // SAFETY: waitpid(2) for our own child, without blocking.
        let ended = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) } == self.pid;
        let standby = if ended || self.born != born {
            self.reborn(born, ended)?
        } else {
            self
        };
        if let Some(reaper) = standby.reaper {
            let _ = REAPER.set(reaper);
        }
        // An image whose agents have grants past the microVM: the run's own processes kept
        // from them before its command can start (D59, D99).
        if spec.setup.iter().any(|e| e == b"confine-eth0") {
            crate::links::confine_eth0().map_err(|e| setup_failed(format!("eth0's confinement: {e}")))?;
        }
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
            let held = Workload {
                pid,
                stdin: None,
                stdout: Some(stdout),
                stderr: Some(stderr),
                sigchld,
                tty: false,
                _held: Some(orders),
                domains: Vec::new(),
                memory: None,
            };
            return Ok((held, None));
        }
        // Its devices first, as dockerd finds them and runc makes their nodes, init's: in
        // the /dev it shares with the workload, the limits and filter on the cgroup it is
        // outside of.
        let devices = spec
            .setup
            .iter()
            .find_map(|e| e.strip_prefix(b"devices="))
            .map(crate::devices::prepare)
            .transpose()
            .map_err(setup_failed)?;
        limit(&spec.cgroup, devices.as_ref())?;
        // A joiner's volumes are its own (D119): the microVM's own run mounts none.
        if spec.setup.iter().any(|e| e.starts_with(b"join-volume=")) {
            return Err(setup_failed(
                "a joining container's volume in the microVM's own run",
            ));
        }
        // Sysctls, by init; the rest by the standby, in its namespaces.
        let mut inherited = Inherited::default();
        let setup = sort_setup(&spec.setup, &mut inherited)?;
        // Its endpoint's, after its own, as libnetwork sets them on the interface it
        // configures after runc's (PM M171).
        for kv in spec
            .setup
            .iter()
            .filter_map(|e| e.strip_prefix(b"endpoint-sysctl="))
        {
            crate::setup::endpoint_sysctl("eth0", &String::from_utf8_lossy(kv)).map_err(|e| {
                setup_failed(format!(
                    "failed to set up container networking: failed to add interface eth0 to sandbox: {e}"
                ))
            })?;
        }
        let _ = WORKLOAD.set(inherited.clone());
        standby
            .launch(spec, false, setup, &inherited)
            .map(|w| (w, declared))
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
        none_of_theirs(uid, gid, &groups)?;
        let env = user::prepare_env(&spec.env, uid, passwd).map_err(setup_failed)?;
        let cwd = if exec {
            exec_cwd(&spec.cwd)?
        } else if matches!(standby.born, Born::Joined(..)) {
            // Made by the joiner's standby in its own root (`make_workdir`).
            joined_workdir(&spec.cwd)?
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
            caps: process.caps.unwrap_or_else(default_caps),
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
                domains: Vec::new(),
                memory: None,
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
            domains: Vec::new(),
            memory: None,
        })
    }
}

/// The standby's side of the fork: it closes init's ends, waits for its orders, and runs
/// them as `child` does. The standby is single-threaded, as init was when it forked, so
/// it may allocate.
fn standby(ends: Ends, isolation: Isolation) -> ! {
    let Ends {
        orders,
        stdio,
        err,
        inits,
    } = ends;
    drop(inits);
    // Before the orders: the standby the template keeps is isolated before its snapshot.
    let joined = matches!(isolation, Isolation::Joined { .. });
    let isolated = match isolation {
        Isolation::Workload => isolate(None, false),
        Isolation::Beside(pid, joiner) => isolate(Some(pid), joiner),
        Isolation::Joined { join, cgroup, report } => {
            let built = crate::join::build(join, &cgroup);
            let failed = built.is_err();
            crate::join::report(report, &built);
            if failed {
                // SAFETY: ends this process, which init waits for, without running atexit
                // handlers inherited from init.
                unsafe { libc::_exit(NOT_RUN as libc::c_int) }
            }
            Ok(())
        }
    };
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
    // A joiner's working directory, made in its root as a run's is (`workdir`).
    if joined && let Err(errno) = make_workdir(&cwd) {
        fail(step::MKDIR, errno, 0);
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
    // Docker's AppArmor's mount rule (`seccomp-mounts=`), loaded before the profile.
    let mounts_filter = crate::setup::filter_named(&o.setup, b"seccomp-mounts=");
    let mounts_prog = mounts_filter.as_ref().map(|(_, p)| libc::sock_fprog {
        len: u16::try_from(p.len()).unwrap_or(u16::MAX),
        filter: p.as_ptr().cast_mut(),
    });
    let mounts = mounts_filter.as_ref().map(|(f, _)| *f).zip(mounts_prog.as_ref());
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
            mounts,
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
    /// A container joining the workload's network (D119), rather than a command beside it.
    joined: bool,
    /// A joiner's writable layer, as the host asks for it once it has the joiner's end.
    save: Save,
    /// Init's own work for a joiner (D119), its built-in in a child of init's: spared as
    /// the workload's end ends the rest, as the joiner is.
    into_joiner: bool,
}

/// A joiner's writable layer after its end (D119), as the workload's is after its own
/// (layer.rs, `save`): its connection kept until the host closes it, having asked for the
/// layer ([`kind::SAVE`]) or not.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Save {
    /// Not asked for.
    Unasked,
    /// Asked for, to send once the joiner's end is.
    Asked,
    /// Being sent by a child of init's, this one.
    Sending(libc::pid_t),
    /// Sent, or not to be: the host closes the connection once it has it.
    Done,
}

/// One of init's own for an exec ([`Spec::builtin`]), done in a child of init's, so that
/// the relay goes on, its output on a pipe as a command's: what is asked for on stdout,
/// status 0; why it could not be had on stderr, status 1.
/// Init's built-in `kind`, for the workload, or for joiner `joiner` (D119): in its mount
/// namespace, whose root and `/proc` are its own, and with its cgroup.
fn builtin(kind: u8, args: &[Vec<u8>], joiner: Option<(libc::pid_t, u32)>) -> Result<Started, Failure> {
    // A joiner's layers, kept as its root was built (D119): their descriptors, which its
    // diff, size and commit read here as the workload's are read.
    let layers = match joiner {
        Some((_, id)) => {
            let kept = join_layers();
            let found = kept.iter().find(|(j, _)| *j == id).map(|(_, k)| {
                (
                    k.lower
                        .try_clone()
                        .map_err(|e| setup_failed(format!("the joiner's layers: {e}"))),
                    k.upper
                        .try_clone()
                        .map_err(|e| setup_failed(format!("the joiner's layers: {e}"))),
                )
            });
            match found {
                Some((lower, upper)) => Some((lower?, upper?)),
                None => return Err(setup_failed("the joiner's layers were not kept")),
            }
        }
        None => None,
    };
    // Whose processes `top` lists: a joiner's by its cgroup, which in a PID namespace of
    // another's has others' beside its own.
    let members = joiner.map(|(_, id)| format!("/join-{id}"));
    let joiner = joiner.map(|(pid, _)| pid);
    let cgroup = match joiner {
        Some(pid) => joiner_cgroup(pid).map_err(|e| {
            setup_failed(format!(
                "the joiner's cgroup: {}",
                io::Error::from_raw_os_error(e)
            ))
        })?,
        None => WORKLOAD_CGROUP.to_string(),
    };
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
        // Init's `/proc`, through which a joiner's mounts are read from within its mount
        // namespace, whose own `/proc` is its PID namespace's, with no `/proc/self` for
        // this process (proc_self_get_link).
        let procfs = joiner.filter(|_| kind == run::builtin::CHANGES).map(|_| {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
                .open("/proc")
        });
        // A joiner's files and processes: its mount namespace, its root and its `/proc`
        // with it. Its cgroup's, and its layers', are read where init sees them.
        if let Some(joined) = joiner.filter(|_| {
            matches!(
                kind,
                run::builtin::STAT
                    | run::builtin::ARCHIVE
                    | run::builtin::EXTRACT
                    | run::builtin::EXPORT
                    | run::builtin::PROCESSES
                    | run::builtin::CHANGES
            )
        }) {
            let entered = File::open(format!("/proc/{joined}/ns/mnt")).and_then(|ns| {
                // SAFETY: setns(2) on a descriptor this process holds, a single-threaded
                // fork of init.
                if unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNS) } == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            });
            if let Err(e) = entered {
                let _ = writeln!(File::from(stderr_w), "the joiner's files: {e}");
                // SAFETY: _exit(2) ends the child without running init's exit paths.
                unsafe { libc::_exit(1) }
            }
        }
        let mut out = io::BufWriter::new(File::from(stdout_w));
        // `cp`'s work, which says how it failed by its status (copy.rs).
        if matches!(
            kind,
            run::builtin::STAT | run::builtin::ARCHIVE | run::builtin::EXTRACT
        ) {
            let path = args.first().map_or(&[][..], Vec::as_slice);
            // The container's files, as dockerd's are in its root: without init's /proc,
            // /sys and /dev, kernel filesystems no file of the run's is in, and whose
            // /proc reaches every process of the microVM, an agent's root among them
            // (`/proc/PID/root`), which this process, init's, could read and write (D115).
            if let Err(e) = files_alone() {
                let _ = writeln!(File::from(stderr_w), "the container's files: {e}");
                // SAFETY: _exit(2) ends the child without running init's exit paths.
                unsafe { libc::_exit(1) }
            }
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
            run::builtin::PROCESSES => out.write_all(&match &members {
                Some(own) => crate::procs::dump(None, crate::procs::Members::Joiner(own)),
                None => crate::procs::dump(REAPER.get().copied(), crate::procs::Members::Workload),
            }),
            run::builtin::CHANGES => match (&layers, procfs) {
                (Some((lower, upper)), Some(procfs)) => procfs
                    .and_then(|procfs| read_at(&procfs, c"self/mountinfo"))
                    .and_then(|mounts| crate::changes::write_with(lower, upper, &mounts, &mut out)),
                (Some(_), None) => Err(io::Error::other("the joiner's mounts were not read")),
                (None, _) => crate::changes::write(&mut out),
            },
            run::builtin::EXPORT => export(&mut out),
            run::builtin::CGROUP => write_cgroup_in(&cgroup, args).map_err(io::Error::other),
            run::builtin::STATS => out.write_all(stats(&cgroup).as_bytes()),
            run::builtin::FREEZE => freeze(&cgroup, args.first().is_some_and(|a| a == b"1")),
            run::builtin::SIZE => match &layers {
                Some((_, upper)) => Some(format!("/proc/self/fd/{}", upper.as_raw_fd())),
                None => crate::changes::upper(),
            }
            .ok_or_else(|| io::Error::other("the writable layer was not kept"))
            .and_then(|u| crate::layer::usage(std::path::Path::new(&u)))
            .and_then(|n| write!(out, "{n}")),
            run::builtin::LAYER => {
                // Paused as dockerd pauses a container it commits (moby daemon/commit.go):
                // every process but init and this one stopped, then let go on; a joiner's
                // (D119) frozen in its cgroup alone, its provider running on.
                let pause = args.first().is_some_and(|a| a == b"pause");
                match &layers {
                    Some((_, upper)) => {
                        let path = format!("/proc/self/fd/{}", upper.as_raw_fd());
                        if pause {
                            // Thawed whatever the packing did; a thaw that fails is the
                            // error to say, its container left frozen.
                            freeze(&cgroup, true).and_then(|()| {
                                let packed = crate::layer::pack_from(&path, &mut out);
                                freeze(&cgroup, false).and(packed)
                            })
                        } else {
                            crate::layer::pack_from(&path, &mut out)
                        }
                    }
                    None => {
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
                }
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

/// Leaves this process, a child of init's, in a mount namespace of its own in which the
/// run's root holds its files alone: init's `/proc`, `/sys` and `/dev`, and what is
/// mounted on them, unmounted, so that each is the directory the image has there.
fn files_alone() -> io::Result<()> {
    // SAFETY: unshare(2), mount(2) and umount2(2) of NUL-terminated literals, in a
    // single-threaded child of init's.
    unsafe {
        if libc::unshare(libc::CLONE_NEWNS) != 0
            || libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ) != 0
        {
            return Err(io::Error::last_os_error());
        }
        for kernel in [c"/proc", c"/sys", c"/dev"] {
            if libc::umount2(kernel.as_ptr(), libc::MNT_DETACH) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

/// File `name` of directory `dir`, read whole.
fn read_at(dir: &File, name: &std::ffi::CStr) -> io::Result<Vec<u8>> {
    // SAFETY: openat(2) of a NUL-terminated name in a directory this process holds.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor openat(2) just returned, which nothing else owns.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut read = Vec::new();
    file.read_to_end(&mut read)?;
    Ok(read)
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

/// Starts joiner `id` (D119): its image `range` of the join disk, joining `workload`'s
/// network, its limits on its own cgroup, its command as its spec says, as a run's is; in
/// the PID namespace of one of `joiners` (each an id and its command's process) where its
/// spec names one.
fn start_joiner(
    range: Result<(u64, u64), String>,
    id: u32,
    workload: libc::pid_t,
    joiners: &[(u32, libc::pid_t)],
    spec: &Spec,
) -> Result<Started, Failure> {
    let (offset, len) = range.map_err(setup_failed)?;
    let layer = crate::join::layer_range(&spec.setup)
        .transpose()
        .map_err(setup_failed)?;
    joiner_takes(&spec.setup)?;
    let join = crate::join::Join {
        offset,
        len,
        workload,
        layer,
        pid: crate::join::pid(&spec.setup, joiners).map_err(setup_failed)?,
        init: spec.setup.iter().any(|e| e == b"init"),
    };
    // Its devices, as the workload's are found and confined (D44): the VM's, their nodes
    // made in its own /dev once its root is built, its cgroup's filter theirs.
    let devices = spec
        .setup
        .iter()
        .find_map(|e| e.strip_prefix(b"devices="))
        .map(crate::devices::prepare_joiner)
        .transpose()
        .map_err(setup_failed)?;
    let mut standby = Standby::forked(Born::Joined(join, id), (None, None))?;
    let (pid, reaper) = (standby.pid, standby.reaper);
    // Under `--init`, its reaper, which a start that fails ends with it.
    let end_reaper = || {
        if let Some(first) = reaper {
            // SAFETY: kill(2) and waitpid(2) of init's own child, not yet waited for.
            unsafe {
                libc::kill(first, libc::SIGKILL);
                libc::waitpid(first, std::ptr::null_mut(), 0);
            }
        }
    };
    // Its sysctls in its own namespaces, as runc writes a container's from within them:
    // init's own would be its provider's (`sort_setup`).
    let (sysctls, setup): (Vec<Vec<u8>>, Vec<Vec<u8>>) = spec
        .setup
        .iter()
        .cloned()
        .partition(|e| e.starts_with(b"sysctl="));
    let mut inherited = Inherited::default();
    let ready = sysctls_in(pid, &sysctls)
        .and_then(|()| devices.as_ref().map_or(Ok(()), |d| nodes_in(pid, d.nodes())))
        .and_then(|()| limit_in(&crate::join::cgroup(id), &spec.cgroup, devices.as_ref(), Some(id)))
        .and_then(|()| sort_setup(&setup, &mut inherited))
        .and_then(|setup| reaper_ready(&mut standby, spec).map(|()| setup));
    let setup = match ready {
        Ok(setup) => setup,
        Err(f) => {
            // Its standby, given no orders, ends as they close: waited for, so that its
            // cgroup is empty as it goes.
            drop(standby);
            // SAFETY: waits for our own child, not yet waited for.
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
            end_reaper();
            crate::join::remove_cgroup(id);
            return Err(f);
        }
    };
    joined().push((id, inherited.clone()));
    match standby.launch(spec, false, setup, &inherited) {
        Ok(w) => Ok(Started {
            pid: w.pid,
            tty: w.tty,
            stdin: w.stdin,
            stdout: w.stdout,
            stderr: w.stderr,
        }),
        // Its standby waited for as it reported why (`launch`).
        Err(f) => {
            joined().retain(|(j, _)| *j != id);
            end_reaper();
            crate::join::remove_cgroup(id);
            Err(f)
        }
    }
}

/// Sends a joiner's reaper, under `--init` (D119), the IDs of its command, whose user its
/// own image's users resolve, as `launch` resolves it again; and waits for it to be ready,
/// as no process of the joiner's may run in its namespace before ([`await_reaper`]).
fn reaper_ready(standby: &mut Standby, spec: &Spec) -> Result<(), Failure> {
    let (Some(first), Some((send, ready))) = (standby.reaper, standby.reaper_ids.take()) else {
        return Ok(());
    };
    let ExecUser { uid, gid, .. } =
        user::resolve(&spec.user, standby.passwd.as_deref(), standby.group.as_deref())
            .map_err(setup_failed)?;
    File::from(send)
        .write_all(&[uid.to_be_bytes(), gid.to_be_bytes()].concat())
        .map_err(|e| setup_failed(format!("the joiner's PID namespace: {e}")))?;
    await_reaper(first, ready, "the joiner's").map(drop)
}

/// Writes `entries`, `sysctl=KEY=VALUE` each, from within the IPC, UTS and network
/// namespaces of joiner `pid` (D119), in a child of init's: its own IPC and UTS
/// namespaces' and its provider's network namespace's, which Docker's joiner may set
/// too (runc libcontainer/configs/validate: a `net.*` sysctl where the network namespace
/// is another container's). Each in the words a run's says it.
fn sysctls_in(pid: libc::pid_t, entries: &[Vec<u8>]) -> Result<(), Failure> {
    if entries.is_empty() {
        return Ok(());
    }
    let (said_r, said_w) = pipe()?;
    // SAFETY: init is single-threaded, so its child may run anything.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(setup_failed(format!("fork: {}", io::Error::last_os_error())));
    }
    if child == 0 {
        drop(said_r);
        let written = (|| -> Result<(), String> {
            for (ns, kind) in [
                ("ipc", libc::CLONE_NEWIPC),
                ("uts", libc::CLONE_NEWUTS),
                ("net", libc::CLONE_NEWNET),
            ] {
                let file = File::open(format!("/proc/{pid}/ns/{ns}"))
                    .map_err(|e| format!("the joiner's {ns} namespace: {e}"))?;
                // SAFETY: setns(2) on a descriptor this process holds.
                if unsafe { libc::setns(file.as_raw_fd(), kind) } != 0 {
                    return Err(format!(
                        "the joiner's {ns} namespace: {}",
                        io::Error::last_os_error()
                    ));
                }
            }
            for entry in entries {
                let kv =
                    String::from_utf8_lossy(entry.strip_prefix(b"sysctl=").unwrap_or(entry)).into_owned();
                let (k, v) = kv.split_once('=').unwrap_or((&kv, ""));
                crate::setup::write_sysctl(k, v)?;
            }
            Ok(())
        })();
        let code = match written {
            Ok(()) => 0,
            Err(said) => {
                let _ = File::from(said_w).write_all(said.as_bytes());
                1
            }
        };
        // SAFETY: _exit(2) ends the child without running init's exit paths.
        unsafe { libc::_exit(code) }
    }
    drop(said_w);
    let mut said = String::new();
    let _ = File::from(said_r).read_to_string(&mut said);
    // SAFETY: waits for our own child.
    unsafe { libc::waitpid(child, std::ptr::null_mut(), 0) };
    if said.is_empty() {
        Ok(())
    } else {
        Err(setup_failed(said))
    }
}

/// Makes `nodes` in joiner `pid`'s own `/dev`, from within its mount namespace, in a
/// child of init's (D44, D119): the workload's are made in the `/dev` init shares with it,
/// a joiner's root is its own. What failed, as runc says it.
fn nodes_in(pid: libc::pid_t, nodes: &[crate::devices::Node]) -> Result<(), Failure> {
    if nodes.is_empty() {
        return Ok(());
    }
    let (said_r, said_w) = pipe()?;
    // SAFETY: init is single-threaded, so its child may run anything.
    let child = unsafe { libc::fork() };
    if child < 0 {
        return Err(setup_failed(format!("fork: {}", io::Error::last_os_error())));
    }
    if child == 0 {
        drop(said_r);
        let made = File::open(format!("/proc/{pid}/ns/mnt"))
            .map_err(|e| format!("the joiner's mount namespace: {e}"))
            .and_then(|ns| {
                // SAFETY: setns(2) on a descriptor this process holds.
                if unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWNS) } != 0 {
                    return Err(format!(
                        "the joiner's mount namespace: {}",
                        io::Error::last_os_error()
                    ));
                }
                crate::devices::make_nodes(nodes)
            });
        let code = match made {
            Ok(()) => 0,
            Err(said) => {
                let _ = File::from(said_w).write_all(said.as_bytes());
                1
            }
        };
        // SAFETY: _exit(2) ends the child without running init's exit paths.
        unsafe { libc::_exit(code) }
    }
    drop(said_w);
    let mut said = String::new();
    let _ = File::from(said_r).read_to_string(&mut said);
    // SAFETY: waits for our own child.
    unsafe { libc::waitpid(child, std::ptr::null_mut(), 0) };
    if said.is_empty() {
        Ok(())
    } else {
        Err(setup_failed(said))
    }
}

/// What a joiner (D119) does not take yet, refused by name before it starts: what init
/// would do in the workload's namespaces rather than the joiner's own, and the workload's
/// own shared directories, which its run alone was given.
fn joiner_takes(setup: &[Vec<u8>]) -> Result<(), Failure> {
    for entry in setup {
        let what = if entry.starts_with(b"volume=") || entry.starts_with(b"domain-volume=") {
            "a volume"
        } else if entry.starts_with(b"dns=")
            || entry.starts_with(b"address=")
            || entry.starts_with(b"address6=")
            || entry.starts_with(b"endpoint-sysctl=")
            || entry == b"confine-eth0"
            || entry.starts_with(b"domains-seccomp")
        {
            "a network of its own"
        } else {
            continue;
        };
        return Err(setup_failed(format!(
            "{what} is not supported in a container joining another's network yet"
        )));
    }
    Ok(())
}

impl Exec {
    /// Starts the command a host's [`kind::EXEC`] frame asks for, unless the workload
    /// has ended (`running`), and dials its connection. Without a connection, the exec's
    /// id and why, for the host to hear on the workload's ([`kind::EXEC_FAILED`]); none if
    /// the frame does not say which exec it is.
    fn start(
        payload: &[u8],
        running: bool,
        workload: libc::pid_t,
        joiners: &[(u32, libc::pid_t)],
    ) -> Result<Exec, Option<(u32, String)>> {
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
            joined: false,
            save: Save::Unasked,
            into_joiner: false,
        };
        exec.to_conn
            .extend(&[&run::header(kind::HELLO, run::TOKEN as u32), token]);
        // An exec in a joiner's namespaces, which init knows by its exec's id (D119).
        let into = spec
            .setup
            .iter()
            .find_map(|e| e.strip_prefix(b"in-join="))
            .map(|n| std::str::from_utf8(n).ok().and_then(|n| n.parse::<u32>().ok()));
        let started = if let Some(range) = crate::join::range(&spec.setup) {
            exec.joined = true;
            if running {
                start_joiner(range, id, workload, joiners, &spec)
            } else {
                Err(setup_failed("the container whose network it joins has ended"))
            }
        } else if let Some(joiner) = into {
            match joiner.and_then(|j| joiners.iter().find(|(id, _)| *id == j)) {
                // Init's own work, in the joiner's namespaces and cgroup.
                Some(&(id, pid)) if spec.builtin != 0 => {
                    exec.into_joiner = true;
                    builtin(spec.builtin, &spec.argv, Some((pid, id)))
                }
                Some(&(joiner, pid)) => {
                    // The joiner's process, but for what the exec says (`--privileged`).
                    let mut process = joined()
                        .iter()
                        .find(|(j, _)| *j == joiner)
                        .map(|(_, p)| p.clone())
                        .unwrap_or_default();
                    if let Some(caps) = spec.setup.iter().find_map(|e| e.strip_prefix(b"caps=")) {
                        process.caps = std::str::from_utf8(caps).ok().and_then(|c| c.parse().ok());
                    }
                    Standby::fork(Born::InJoiner(pid))
                        .and_then(|standby| standby.launch(&spec, true, process.setup.clone(), &process))
                }
                .map(|w| Started {
                    pid: w.pid,
                    tty: w.tty,
                    stdin: w.stdin,
                    stdout: w.stdout,
                    stderr: w.stderr,
                }),
                None => Err(Failure {
                    daemon: true,
                    ..setup_failed("the container is not running")
                }),
            }
        } else if spec.builtin != 0 && running {
            builtin(spec.builtin, &spec.argv, None)
        } else if running {
            // The workload's process, but for what the exec says (`--privileged`), which
            // the run refuses beside its domains as it refused the workload's (D115).
            let mut process = WORKLOAD.get().cloned().unwrap_or_default();
            if let Some(caps) = spec.setup.iter().find_map(|e| e.strip_prefix(b"caps=")) {
                process.caps = std::str::from_utf8(caps).ok().and_then(|c| c.parse().ok());
            }
            BESIDE
                .get()
                .map_or(Ok(()), |b| b.refuse(&spec.setup, "exec"))
                .and_then(|()| Standby::fork(Born::Beside(workload)))
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
    /// sent, or its connection gone. A joiner's connection is the host's to close, once it
    /// has asked for its writable layer or not (D119).
    fn finished(&self) -> bool {
        self.status.is_some()
            && self.stdout.is_none()
            && self.stderr.is_none()
            && !matches!(self.save, Save::Sending(_))
            && (self.conn.is_none() || self.ended && self.to_conn.is_empty() && !self.joined)
    }

    /// Whether its end is all said and its connection waits for the host: a joiner's
    /// (D119), whose writable layer the host may ask for, then closes.
    fn awaiting_host(&self) -> bool {
        self.joined
            && self.ended
            && self.to_conn.is_empty()
            && matches!(self.save, Save::Unasked | Save::Done)
    }

    /// Sends this joiner's writable layer on its connection from a child of init's, as
    /// the host asked once it had its end: the layers its root was built of, kept until
    /// now (`JOIN_LAYERS`). Where it has none, its root never built, or no child can be
    /// had, its connection goes unanswered, which the host hears as no layer.
    fn send_layer(&mut self) {
        let upper = join_layers()
            .iter()
            .find(|(j, _)| *j == self.id)
            .and_then(|(_, k)| k.upper.try_clone().ok());
        let (Some(upper), Some(conn)) = (upper, self.conn.as_ref()) else {
            self.save = Save::Done;
            self.conn = None;
            return;
        };
        // SAFETY: init is single-threaded, so its child may run anything.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // The connection's file is init's too, which writes no more to it.
            set_nonblocking(conn.as_raw_fd(), false);
            let at = format!("/proc/self/fd/{}", upper.as_raw_fd());
            let code = match crate::layer::save_from(conn, &at) {
                Ok(()) => 0,
                Err(e) => {
                    let _ = writeln!(io::stderr(), "shards-init: saving a joiner's files: {e}");
                    1
                }
            };
            // SAFETY: _exit(2) ends the child without running init's exit paths.
            unsafe { libc::_exit(code) }
        }
        if pid < 0 {
            self.save = Save::Done;
            self.conn = None;
        } else {
            self.save = Save::Sending(pid);
        }
    }
}

/// Sends the workload's writable layer on `conn` from a child of init's, as the host asks
/// once it has the workload's end while containers joined to its network go on (D119):
/// its pid, init's loop serving them meanwhile. Where no child can be had, init sends it
/// itself, its loop waiting.
fn send_workload_layer(conn: &File) -> Option<libc::pid_t> {
    let said = |e: io::Error| {
        let _ = writeln!(io::stderr(), "shards-init: saving the container's files: {e}");
    };
    // SAFETY: init is single-threaded, so its child may run anything.
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // The connection's file is init's too, which writes nothing to it meanwhile.
        set_nonblocking(conn.as_raw_fd(), false);
        let code = match crate::layer::save(conn) {
            Ok(()) => 0,
            Err(e) => {
                said(e);
                1
            }
        };
        // SAFETY: _exit(2) ends the child without running init's exit paths.
        unsafe { libc::_exit(code) }
    }
    if pid > 0 {
        return Some(pid);
    }
    set_nonblocking(conn.as_raw_fd(), false);
    if let Err(e) = crate::layer::save(conn) {
        said(e);
    }
    set_nonblocking(conn.as_raw_fd(), true);
    None
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
    /// A domain's output, by its index.
    Domain(usize),
    /// The domains' memory.
    Memory,
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
    /// Relays the workload, and its execs, until they end: its status, and whether its end
    /// was said already, as it is where containers joined to its network outlive it (D119):
    /// its OOM and EXIT as soon as its own output is drained, and its writable layer saved
    /// as the host asks, while the joiners go on.
    fn relay(mut self, conn: &File, mut signals: Option<File>) -> (u32, bool) {
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
        // The workload ended while containers joined to its network ran (D119); its end
        // said; the host's ask for its writable layer heard, or its end of asking; the
        // child of init's sending that layer meanwhile.
        let (mut outlived, mut reported, mut asked) = (false, false, false);
        let mut packing: Option<libc::pid_t> = None;
        // Reused each turn: six for the workload, four for each exec.
        let (mut set, mut owners) = (Vec::<libc::pollfd>::new(), Vec::<Owner>::new());
        loop {
            let exited = status.is_some();
            // A joiner's writable layer, sent as the host asked once its end is said; its
            // layers kept until all of it is (D119).
            for e in &mut execs {
                if e.save == Save::Asked && e.ended && e.to_conn.is_empty() {
                    e.send_layer();
                }
            }
            for e in execs.iter().filter(|e| e.joined && e.finished()) {
                join_layers().retain(|(j, _)| *j != e.id);
                // What it left was killed as it ended, and has gone by now.
                crate::join::remove_cgroup(e.id);
            }
            execs.retain(|e| !e.finished());
            // Its end said as soon as its own output is, while its joiners go on (D119).
            if outlived
                && !reported
                && self.stdout.is_none()
                && self.stderr.is_none()
                && self.domains.iter().all(|d| d.out.is_none())
            {
                if oom_killed() {
                    to_host.extend(&[&run::header(kind::OOM, 0)]);
                }
                to_host.extend(&[
                    &run::header(kind::EXIT, 4),
                    &status.unwrap_or(NOT_RUN).to_be_bytes(),
                ]);
                reported = true;
            }
            if exited
                && self.stdout.is_none()
                && self.stderr.is_none()
                && (to_host.is_empty() || host.is_none())
                && execs.is_empty()
                && packing.is_none()
                && self.domains.iter().all(|d| d.out.is_none())
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
            let host_events = if (!exited && !stdin_eof && to_stdin.len() < BUFFERED) || (reported && !asked)
            {
                libc::POLLIN
            } else {
                0
            } | if to_host.is_empty() { 0 } else { libc::POLLOUT };
            // The workload's layer is written to it meanwhile, by a child of init's: what
            // init has for the host waits until it has been, after it.
            poll(host.filter(|_| packing.is_none()), host_events, Owner::Host);
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
            for (i, d) in self.domains.iter().enumerate() {
                poll(
                    d.out.as_ref().map(AsRawFd::as_raw_fd),
                    out_events,
                    Owner::Domain(i),
                );
            }
            poll(
                self.memory.as_ref().map(crate::domains::Memory::fd),
                libc::POLLIN,
                Owner::Memory,
            );
            for (i, e) in execs.iter().enumerate() {
                let conn_events = if !e.connected {
                    libc::POLLOUT
                } else if matches!(e.save, Save::Sending(_)) {
                    // A joiner's layer is written to it meanwhile, by a child of init's.
                    0
                } else {
                    (if e.to_conn.is_empty() { 0 } else { libc::POLLOUT })
                        | if e.awaiting_host() || (!e.stdin_eof && e.to_stdin.len() < BUFFERED) {
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
                                // A container ends with its main process; containers joined
                                // to its network outlive it, as Docker's do (D119).
                                if execs.iter().any(|e| e.joined && e.pid > 0 && e.status.is_none()) {
                                    outlived = true;
                                    // And init's own work for them: their built-ins, and
                                    // the layers of those that ended, being sent.
                                    let spared: Vec<libc::pid_t> = execs
                                        .iter()
                                        .filter_map(|e| match e.save {
                                            Save::Sending(pid) => Some(pid),
                                            _ => (e.into_joiner && e.pid > 0 && e.status.is_none())
                                                .then_some(e.pid),
                                        })
                                        .collect();
                                    kill_all_but_joiners(&spared);
                                } else {
                                    // SAFETY: kill(2) of every process but init.
                                    unsafe { libc::kill(-1, libc::SIGKILL) };
                                }
                            } else if let Some(e) = execs.iter_mut().find(|e| e.pid == pid) {
                                e.status = Some(code);
                                // Its stdin goes with it.
                                e.stdin = None;
                                e.to_stdin.clear();
                                // A joiner's cgroup, empty now: the kernel ends a PID
                                // namespace's other processes before its first's end is
                                // reaped (D119).
                                // In another's PID namespace, what it started is killed
                                // as it ends, as runc kills it.
                                if e.joined {
                                    joined().retain(|(j, _)| *j != e.id);
                                    crate::join::kill_rest(e.id);
                                    crate::join::remove_cgroup(e.id);
                                }
                            } else if packing == Some(pid) {
                                // The workload's layer sent, whole or not; the connection's
                                // file, which the child made blocking, nonblocking again.
                                packing = None;
                                set_nonblocking(conn.as_raw_fd(), true);
                            } else if let Some(e) = execs.iter_mut().find(|e| e.save == Save::Sending(pid)) {
                                // Its layer sent, whole or not: the host keeps only a whole
                                // one, and closes the connection once it has what came. The
                                // connection's file, which the child made blocking to write
                                // it, is nonblocking again for this loop.
                                e.save = Save::Done;
                                if let Some(conn) = &e.conn {
                                    set_nonblocking(conn.as_raw_fd(), true);
                                }
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
                                // The host is gone: nothing more for stdin, nor any ask.
                                Ok(0) | Err(_) => {
                                    stdin_eof = true;
                                    asked |= reported;
                                }
                                Ok(n) => {
                                    from_host.extend_from_slice(buf.get(..n).unwrap_or_default());
                                    let mut closed = false;
                                    let mut save = false;
                                    let whole = each_frame(&mut from_host, |which, payload| {
                                        if which == kind::STDIN && !reported {
                                            closed |= payload.is_empty();
                                            to_stdin.extend(&[payload]);
                                        }
                                        save |= which == kind::SAVE && reported;
                                    });
                                    stdin_eof |= closed || !whole;
                                    // Its writable layer, as the host asks once it has the
                                    // end (layer.rs), sent beside its joiners as they go on.
                                    if save && !asked {
                                        asked = true;
                                        packing = send_workload_layer(conn);
                                    }
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
                                        kind::EXEC => match Exec::start(
                                            payload,
                                            running,
                                            pid,
                                            &execs
                                                .iter()
                                                .filter(|e| e.joined && e.pid > 0 && e.status.is_none())
                                                .map(|e| (e.id, e.pid))
                                                .collect::<Vec<_>>(),
                                        ) {
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
                    Owner::Domain(i) => {
                        let Some(d) = self.domains.get_mut(i) else {
                            continue;
                        };
                        let (data, eof) = match read(fd, &mut buf) {
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                            Ok(0) | Err(_) => (&[][..], true),
                            Ok(n) => (buf.get(..n).unwrap_or_default(), false),
                        };
                        crate::frames::prefixed_lines(&d.label, &mut d.partial, data, eof, |line| {
                            let len = u32::try_from(line.len()).unwrap_or(u32::MAX);
                            to_host.extend(&[&run::header(kind::STDERR, len), line]);
                        });
                        if eof {
                            d.out = None;
                        }
                    }
                    Owner::Memory => {
                        if let Some(m) = self.memory.as_ref() {
                            m.relieve(&self.domains, |line| {
                                let len = u32::try_from(line.len()).unwrap_or(u32::MAX);
                                to_host.extend(&[&run::header(kind::STDERR, len), line]);
                            });
                        }
                    }
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
                                        // The end of the host's stdin; past a joiner's end,
                                        // of its asking (D119).
                                        Ok(0) | Err(_) => {
                                            e.stdin_eof = true;
                                            if e.awaiting_host() {
                                                e.conn = None;
                                            }
                                        }
                                        Ok(n) => {
                                            e.from_conn.extend_from_slice(buf.get(..n).unwrap_or_default());
                                            let live = e.stdin.is_some();
                                            let (mut closed, mut save, to) = (false, false, &mut e.to_stdin);
                                            let whole = each_frame(&mut e.from_conn, |which, payload| {
                                                if which == kind::STDIN {
                                                    closed |= payload.is_empty();
                                                    if live {
                                                        to.extend(&[payload]);
                                                    }
                                                }
                                                save |= which == kind::SAVE;
                                            });
                                            e.stdin_eof |= closed || !whole;
                                            // A joiner's writable layer, as the host asks once
                                            // it has its end; a connection that says what no
                                            // frame is ends its asking.
                                            if save && e.joined && e.save == Save::Unasked {
                                                e.save = Save::Asked;
                                            } else if !whole && e.awaiting_host() {
                                                e.conn = None;
                                            }
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
        (status.unwrap_or(NOT_RUN), reported)
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

/// A joiner's working directory (D119), as [`workdir`] checks a run's: `/` if none,
/// refused unless absolute; made in the joiner's root by its standby, which alone sees it.
fn joined_workdir(cwd: &[u8]) -> Result<Vec<u8>, Failure> {
    match cwd.first() {
        None => Ok(b"/".to_vec()),
        Some(b'/') => Ok(cwd.to_vec()),
        Some(_) => Err(setup_failed(format!(
            "the working directory {:?} is not absolute",
            String::from_utf8_lossy(cwd)
        ))),
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
        // As `workdir` says it for a run's.
        step::MKDIR => {
            let path = String::from_utf8_lossy(cwd);
            return setup_failed(if errno == libc::ENOTDIR {
                format!("Cannot mkdir: {path} is not a directory")
            } else {
                format!("mkdir {path}: {}", io::Error::from_raw_os_error(errno))
            });
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
    /// The filter that stands in for Docker's AppArmor's mount rule, loaded before its
    /// seccomp filter; that filter; each with its seccomp(2) flags; and whether
    /// no_new_privs is set.
    mounts: Option<(u32, &'a libc::sock_fprog)>,
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
        if !c.nnp {
            for f in [c.mounts, c.seccomp].into_iter().flatten() {
                if !load(f) {
                    fail(step::SECCOMP);
                }
            }
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
            for f in [c.mounts, c.seccomp].into_iter().flatten() {
                if !load(f) {
                    fail(step::SECCOMP);
                }
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

pub(crate) fn set_nonblocking(fd: RawFd, on: bool) {
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
