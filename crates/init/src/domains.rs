//! Agents and harnesses, each started in its domain (docs/design/architecture.md D59;
//! AGENTFILE_ARCH.md §9.3, §9.9): the image's normalized Agentfile (`/.agentfile.json`)
//! says which there are and where each lies, and each one's OSI config (`<dir>.d/osi.json`)
//! how it runs. One whose config says nothing of how it runs is files alone, for another
//! to use.
//!
//! Each starts with `clone3` into namespaces of its own (mount, PID, IPC, network, UTS,
//! cgroup) and into a cgroup of its own, `/sys/fs/cgroup/domains/<kind>-<name>`, whose
//! `pids.max` bounds what it starts. Its first process, PID 1 of its namespace:
//!
//! - sees the image's system as built, read-only, `nosuid` and `nodev` (workloads inherit
//!   the microVM's OS), and nothing the run made of it since: none of its files, mounts or
//!   sockets, a Unix socket's path being reachable to any process that can see it
//!   (unix(7)); its own directory and grants with it; every other domain's directory
//!   and grants hidden under an empty tmpfs no one may read; `/sys` hidden; a `/proc` of
//!   its PID namespace; a `/dev` of six nodes, the `fd` and stdio links, a `shm` of its
//!   own and an `mqueue` of its IPC namespace's; and a scratch tmpfs of its own at
//!   `/tmp`, lost when it ends;
//! - has only its own loopback, brought up;
//! - runs as uid and gid `200000 + n`, the n-th domain's, with no supplementary groups and
//!   no capability left, bounding set included, and `no_new_privs` set;
//! - holds no descriptor of init's but its stdio: stdin `/dev/null`, stdout and stderr one
//!   pipe that init relays a line at a time, each line prefixed with the domain;
//! - runs the command its config says, a relative program its directory's, a bare name
//!   found on `PATH` before it starts.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::json::{self, Value};

/// The first domain's uid and gid; the n-th's is this plus n.
pub const FIRST_ID: u32 = shards_abi::DOMAIN_FIRST_ID;
/// Where the domains' cgroups are.
const CGROUPS: &str = "/sys/fs/cgroup/domains";
/// The `PATH` a domain starts with: Docker's default (moby oci/defaults.go).
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// The nodes a domain's `/dev` holds, as Docker's containers' (moby oci/defaults.go,
/// `DefaultLinuxDevices` less `/dev/console`): name, major, minor.
const DEV: [(&CStr, u32, u32); 6] = [
    (c"/dev/null", 1, 3),
    (c"/dev/zero", 1, 5),
    (c"/dev/full", 1, 7),
    (c"/dev/random", 1, 8),
    (c"/dev/urandom", 1, 9),
    (c"/dev/tty", 5, 0),
];
/// Its links, as Docker's (runc libcontainer/rootfs_linux.go, `setupDevSymlinks`).
const LINKS: [(&CStr, &CStr); 4] = [
    (c"/proc/self/fd", c"/dev/fd"),
    (c"/proc/self/fd/0", c"/dev/stdin"),
    (c"/proc/self/fd/1", c"/dev/stdout"),
    (c"/proc/self/fd/2", c"/dev/stderr"),
];

/// A domain to start.
#[derive(Debug, Clone)]
pub struct Domain {
    /// `agent main`, `harness drive`: what its output lines are prefixed with.
    pub label: String,
    /// Its cgroup's name, `agent-main`, which is its host name too.
    pub cgroup: String,
    pub dir: Vec<u8>,
    pub id: u32,
    /// `pids.max`: `None` for the microVM's own.
    pub pids: Option<u64>,
    /// `--processes=none`: it starts threads alone (§9.9).
    pub no_processes: bool,
    /// `memory.max`, in bytes, where its config asks one (`asks.memory`): under the
    /// domains' own, which every domain has.
    pub memory: Option<u64>,
    pub argv: Vec<CString>,
    pub env: Vec<CString>,
    pub workdir: CString,
    /// Whether it is a harness, and its name, as `CONNECT`s name it.
    pub name: crate::netplan::Name,
    /// Its link to the others, where the Agentfile grants it one.
    pub link: Option<crate::netplan::Link>,
    /// Its `/etc/resolv.conf`, where it reaches past the microVM: the microVM's gateway,
    /// whose network process asks the host's resolvers (D59).
    pub resolv: Option<Vec<u8>>,
}

/// What the image's normalized Agentfile says to start: every domain's directory, the
/// domains that run, and the pairs of them that may open connections, by index.
pub type Read = (
    Vec<Vec<u8>>,
    Vec<Domain>,
    Vec<(usize, usize, Vec<crate::netplan::Egress>)>,
);

/// The directories of every domain the image's normalized Agentfile declares, and those
/// domains that say how they run; nothing where the image has no Agentfile.
pub fn read() -> Result<Read, String> {
    let text = match std::fs::read("/.agentfile.json") {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new(), Vec::new())),
        Err(e) => return Err(format!("/.agentfile.json: {e}")),
    };
    let spec = json::parse(&text).map_err(|e| format!("/.agentfile.json: {e}"))?;
    if spec.get("schemaVersion").and_then(Value::u64) != Some(1) {
        return Err("/.agentfile.json: a schema version this init does not know".into());
    }
    let mut dirs = Vec::new();
    let mut out = Vec::new();
    let mut n = 0u32;
    for (kind, list) in [("agent", "agents"), ("harness", "harnesses")] {
        for d in spec.get(list).map(Value::array).unwrap_or_default() {
            let name = d
                .get("name")
                .and_then(Value::str)
                .ok_or("a domain without a name")?;
            let dir = d
                .get("to")
                .and_then(Value::str)
                .ok_or("a domain without its directory")?;
            if !dir.starts_with('/') {
                return Err(format!("{kind} {name}: its directory {dir:?} is not absolute"));
            }
            dirs.push(dir.as_bytes().to_vec());
            let id = FIRST_ID.checked_add(n).ok_or("more domains than uids")?;
            n += 1;
            let path = format!("{dir}.d/osi.json");
            let config = match std::fs::read(&path) {
                Ok(c) => json::parse(&c).map_err(|e| format!("{path}: {e}"))?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(format!("{path}: {e}")),
            };
            let Some(run) = config.get("run") else { continue };
            out.push(domain(kind, name, dir, id, d, &config, run)?);
        }
    }
    // Their links, clear of the microVM's own network.
    let names: Vec<crate::netplan::Name> = out.iter().map(|d| d.name.clone()).collect();
    let own = crate::net::current().map(|(addr, prefix, _)| {
        let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
        (std::net::Ipv4Addr::from(u32::from(addr) & mask), prefix)
    });
    let plan = crate::netplan::plan(&spec, &names, own)?;
    for (d, link) in out.iter_mut().zip(plan.links) {
        // A resolver only where a grant names one (`--dns`, a remote MCP server): its own
        // gateway, the agents' resolver, which holds it to what it may ask.
        if let Some(l) = &link
            && (l.dns || !l.mcp.is_empty())
            && let Some(a) = l.addresses.first()
        {
            d.resolv = Some(format!("nameserver {}\noptions ndots:0\n", a.gateway).into_bytes());
        }
        d.link = link;
    }
    Ok((dirs, out, plan.pairs))
}

fn domain(
    kind: &str,
    name: &str,
    dir: &str,
    id: u32,
    declared: &Value,
    config: &Value,
    run: &Value,
) -> Result<Domain, String> {
    let label = format!("{kind} {name}");
    let cstr = |s: &str| CString::new(s).map_err(|_| format!("{label}: a NUL in {s:?}"));
    let command = run
        .get("command")
        .and_then(Value::strings)
        .filter(|c| !c.is_empty())
        .ok_or_else(|| format!("{label}: run.command is no list of strings"))?;
    // `--processes` as declared, else what its config asks, else the microVM's. `none`
    // is its seccomp filter's: pids.max counts threads, which it may start.
    let no_processes = matches!(declared.get("processes"), Some(Value::String(s)) if s == "none");
    let pids = match declared.get("processes") {
        Some(Value::String(s)) if s == "none" => None,
        Some(Value::Null) | None => config
            .get("asks")
            .and_then(|a| a.get("processes"))
            .and_then(Value::u64),
        Some(v) => Some(v.u64().ok_or_else(|| format!("{label}: processes is no count"))?),
    };
    let memory = match config.get("asks").and_then(|a| a.get("memory")) {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.u64()
                .ok_or_else(|| format!("{label}: asks.memory is no count of bytes"))?,
        ),
    };
    let workdir = run.get("workdir").and_then(Value::str).unwrap_or(".");
    let mut env = vec![
        cstr(&format!("PATH={PATH}"))?,
        cstr("HOME=/tmp")?,
        cstr(&format!("SHARDS_DOMAIN={label}"))?,
    ];
    for e in run.get("env").and_then(Value::strings).unwrap_or_default() {
        env.push(cstr(&e)?);
    }
    let first = command.first().map(String::as_str).unwrap_or_default();
    let program = if first.starts_with('/') {
        first.to_string()
    } else if first.contains('/') {
        // Relative: its directory's.
        format!("{dir}/{first}")
    } else {
        // A bare name, on PATH as execvp(3) would find it, found here: the child may not
        // allocate.
        PATH.split(':')
            .map(|p| format!("{p}/{first}"))
            .find(|p| {
                CString::new(p.as_str()).is_ok_and(|c| {
                    // SAFETY: access(2) of a NUL-terminated path.
                    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
                })
            })
            .ok_or_else(|| format!("{label}: {first:?} is on no directory of PATH"))?
    };
    let mut argv = vec![cstr(&program)?];
    for a in command.iter().skip(1) {
        argv.push(cstr(a)?);
    }
    let workdir = cstr(&format!("{dir}/{workdir}"))?;
    Ok(Domain {
        cgroup: format!("{kind}-{name}"),
        label,
        dir: dir.as_bytes().to_vec(),
        id,
        pids,
        no_processes,
        memory,
        argv,
        env,
        workdir,
        name: (kind == "harness", name.to_string()),
        link: None,
        resolv: None,
    })
}

/// A started domain: its label, and its output's read end.
#[derive(Debug)]
pub struct Started {
    pub label: String,
    /// Its cgroup's directory.
    pub cgroup: String,
    pub out: Option<OwnedFd>,
    /// What it wrote past its last whole line.
    pub partial: Vec<u8>,
}

/// `struct clone_args` (include/uapi/linux/sched.h), as `clone3` reads it.
#[repr(C)]
#[derive(Default)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

/// include/uapi/linux/sched.h.
const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

/// Landlock (include/uapi/linux/landlock.h; Documentation/userspace-api/landlock.rst).
mod landlock {
    /// `landlock_create_ruleset`'s flag that asks the ABI version.
    pub const CREATE_RULESET_VERSION: u32 = 1;
    pub const RULE_PATH_BENEATH: u32 = 1;
    pub const EXECUTE: u64 = 1 << 0;
    pub const WRITE_FILE: u64 = 1 << 1;
    pub const READ_FILE: u64 = 1 << 2;
    pub const READ_DIR: u64 = 1 << 3;
    pub const REMOVE_FILE: u64 = 1 << 5;
    pub const MAKE_SOCK: u64 = 1 << 9;
    /// `IOCTL_DEV`, ABI 5: the last filesystem right, so every right is below it.
    pub const IOCTL_DEV: u64 = 1 << 15;
    pub const FS_ALL: u64 = (IOCTL_DEV << 1) - 1;
    /// ABI 4.
    pub const NET_BIND_TCP: u64 = 1 << 0;
    pub const NET_CONNECT_TCP: u64 = 1 << 1;
    /// ABI 6.
    pub const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
    pub const SCOPE_SIGNAL: u64 = 1 << 1;
    /// The first ABI with every right used here: IOCTL_DEV is 5, the scopes 6.
    pub const NEEDED: i64 = 6;

    #[repr(C)]
    pub struct RulesetAttr {
        pub handled_access_fs: u64,
        pub handled_access_net: u64,
        pub scoped: u64,
    }

    #[repr(C, packed)]
    pub struct PathBeneathAttr {
        pub allowed_access: u64,
        pub parent_fd: i32,
    }
}

/// Everything a domain's first process uses, made before it is: after `clone3` it calls
/// the kernel alone.
struct Prepared {
    hide: Vec<CString>,
    hostname: CString,
    argv: Vec<*const libc::c_char>,
    envp: Vec<*const libc::c_char>,
    last_cap: u32,
    /// The TCP rights Landlock refuses it: connecting unless it may open connections,
    /// binding unless it may be connected to.
    net_handled: u64,
    /// Its `/etc/hosts`, where it has a link.
    hosts: Option<Vec<u8>>,
    /// Its `/etc/resolv.conf`, where it reaches past the microVM.
    resolv: Option<Vec<u8>>,
    /// Where it waits for its link, before it holds nothing of init's.
    go: Option<RawFd>,
    /// Its root: the image as built, a mount not yet attached.
    root: RawFd,
    /// Its Unix sockets' directories, each a mount not yet attached and where it goes,
    /// `/run/networks/<network>/<name>`; read-only where it only connects.
    sockets: Vec<(RawFd, CString)>,
    /// Those of them it receives on, which Landlock lets it make sockets in.
    receives: Vec<CString>,
}

/// `open_tree`'s and `mount_setattr`'s (include/uapi/linux/mount.h).
mod tree {
    pub const OPEN_TREE_CLONE: u32 = 1;
    pub const OPEN_TREE_CLOEXEC: u32 = libc::O_CLOEXEC as u32;
}

/// A copy of the mount of directory `dir`, attached nowhere, with `attr` set: a mount of
/// init's own, as `open_tree` has copied since Linux 5.2, and `mount_setattr` changes since
/// 5.12.
fn copy_of(dir: &str, attr: u64) -> io::Result<OwnedFd> {
    let path = CString::new(dir).map_err(io::Error::other)?;
    // SAFETY: open_tree(2) of a NUL-terminated path.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_open_tree,
            libc::AT_FDCWD,
            path.as_ptr(),
            tree::OPEN_TREE_CLONE | tree::OPEN_TREE_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor open_tree just made, owned here alone.
    let copy = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
    let set: [u64; 4] = [attr, 0, 0, 0];
    // SAFETY: mount_setattr(2) of the copy just made, with a struct mount_attr it owns.
    if unsafe {
        libc::syscall(
            libc::SYS_mount_setattr,
            copy.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            set.as_ptr(),
            std::mem::size_of_val(&set),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(copy)
}

/// The new mount API's (include/uapi/linux/mount.h).
mod mount_api {
    pub const FSOPEN_CLOEXEC: u32 = 0x1;
    pub const FSCONFIG_SET_STRING: u32 = 1;
    pub const FSCONFIG_CMD_CREATE: u32 = 6;
    pub const FSMOUNT_CLOEXEC: u32 = 0x1;
    pub const MOVE_MOUNT_F_EMPTY_PATH: u32 = 0x4;
}

/// A new mount of `fstype` with `options`, attached nowhere.
fn detached(fstype: &CStr, options: &[(&CStr, &CStr)]) -> io::Result<OwnedFd> {
    use mount_api::*;
    // SAFETY: fsopen(2) of a NUL-terminated name.
    let fs = unsafe { libc::syscall(libc::SYS_fsopen, fstype.as_ptr(), FSOPEN_CLOEXEC) };
    if fs < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor fsopen just made, owned here alone.
    let fs = unsafe { OwnedFd::from_raw_fd(fs as RawFd) };
    for (key, value) in options {
        // SAFETY: fsconfig(2) of NUL-terminated strings on the context just made.
        let set = unsafe {
            libc::syscall(
                libc::SYS_fsconfig,
                fs.as_raw_fd(),
                FSCONFIG_SET_STRING,
                key.as_ptr(),
                value.as_ptr(),
                0,
            )
        };
        if set < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let null = std::ptr::null::<libc::c_char>();
    // SAFETY: fsconfig(2) and fsmount(2) on the context just made.
    let mounted = unsafe {
        if libc::syscall(
            libc::SYS_fsconfig,
            fs.as_raw_fd(),
            FSCONFIG_CMD_CREATE,
            null,
            null,
            0,
        ) < 0
        {
            return Err(io::Error::last_os_error());
        }
        libc::syscall(libc::SYS_fsmount, fs.as_raw_fd(), FSMOUNT_CLOEXEC, 0)
    };
    if mounted < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor fsmount just made, owned here alone.
    Ok(unsafe { OwnedFd::from_raw_fd(mounted as RawFd) })
}

/// What a domain mounts over and the image may lack, made where it does: the run makes
/// them in its writable layer, which a domain does not see.
fn skeleton(lower: std::os::fd::BorrowedFd<'_>, sockets: &[CString]) -> io::Result<OwnedFd> {
    let skel = detached(c"tmpfs", &[(c"mode", c"0755")])?;
    let lacks = |path: &CStr| {
        // SAFETY: fstatat(2) of a NUL-terminated path into a zeroed stat it owns, a type of
        // integers alone.
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            libc::fstatat(
                lower.as_raw_fd(),
                path.as_ptr(),
                &raw mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            ) != 0
        }
    };
    for dir in [c"proc", c"dev", c"sys", c"tmp", c"etc"] {
        if !(lacks(dir) || dir == c"etc") {
            continue;
        }
        // SAFETY: mkdirat(2) of a NUL-terminated path in the tmpfs just made.
        if unsafe { libc::mkdirat(skel.as_raw_fd(), dir.as_ptr(), 0o755) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // Where its Unix sockets' directories go: `run/networks/<network>/<name>`, each made
    // with its parents, `run` merged with the image's own.
    for dir in sockets {
        let bytes = dir.to_bytes();
        for (at, _) in bytes
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == b'/')
            .chain([(bytes.len(), &0)])
        {
            let Ok(part) = CString::new(bytes.get(..at).unwrap_or_default()) else {
                continue;
            };
            // SAFETY: mkdirat(2) of a NUL-terminated path in the tmpfs just made.
            if unsafe { libc::mkdirat(skel.as_raw_fd(), part.as_ptr(), 0o755) } != 0
                && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
            {
                return Err(io::Error::last_os_error());
            }
        }
    }
    for file in [c"etc/hosts", c"etc/resolv.conf"] {
        if !lacks(file) {
            continue;
        }
        // SAFETY: openat(2) of a NUL-terminated path in the tmpfs just made.
        let fd = unsafe {
            libc::openat(
                skel.as_raw_fd(),
                file.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC,
                0o644,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor openat just made, closed here.
        drop(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    Ok(skel)
}

/// A domain's root: the image as built, under what [`skeleton`] adds, an overlay with no
/// writable layer, attached nowhere.
fn image_root(skel: &OwnedFd, lower: std::os::fd::BorrowedFd<'_>) -> io::Result<OwnedFd> {
    let layers = CString::new(format!(
        "/proc/self/fd/{}:/proc/self/fd/{}",
        skel.as_raw_fd(),
        lower.as_raw_fd()
    ))
    .map_err(io::Error::other)?;
    detached(c"overlay", &[(c"lowerdir", layers.as_c_str())])
}

/// The domains' memory, watched: past their share (`memory.high`), the domain holding the
/// most, its scratch counted (`memory.current`), is ended whole (`cgroup.kill`). One ended
/// holds the most until it has left its cgroup, which a process does after its namespaces,
/// and with its mount namespace its scratch, are gone (kernel/exit.c, `do_exit`): so no
/// other is ended for memory it still holds.
pub struct Memory {
    events: OwnedFd,
}

impl Memory {
    /// Watches the domains' `memory.events`, which the kernel marks modified as they change
    /// (Documentation/admin-guide/cgroup-v2.rst, "Conventions").
    pub fn watch() -> Result<Memory, String> {
        // SAFETY: inotify_init1(2), whose descriptor is owned below.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(format!(
                "watching the domains' memory: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: the descriptor inotify_init1 just made, owned here alone.
        let events = unsafe { OwnedFd::from_raw_fd(fd) };
        let path = CString::new(format!("{CGROUPS}/memory.events")).map_err(|e| e.to_string())?;
        // SAFETY: inotify_add_watch(2) of a NUL-terminated path.
        if unsafe { libc::inotify_add_watch(events.as_raw_fd(), path.as_ptr(), libc::IN_MODIFY) } < 0 {
            return Err(format!(
                "watching the domains' memory: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(Memory { events })
    }

    pub fn fd(&self) -> RawFd {
        self.events.as_raw_fd()
    }

    /// After the watch says the domains' memory changed: ends the domain holding the most
    /// where they are past their share, and says so through `say`.
    pub fn relieve(&self, domains: &[Started], mut say: impl FnMut(&[u8])) {
        let mut buf = [0u8; 4096];
        // SAFETY: read(2) into a buffer it owns, until the watch has nothing more.
        while unsafe { libc::read(self.events.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        let number = |path: &str| {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        let populated = |d: &Started| {
            std::fs::read_to_string(format!("{}/cgroup.events", d.cgroup))
                .is_ok_and(|e| e.lines().any(|l| l == "populated 1"))
        };
        let (Some(current), Some(high)) = (
            number(&format!("{CGROUPS}/memory.current")),
            number(&format!("{CGROUPS}/memory.high")),
        ) else {
            return;
        };
        if current <= high {
            return;
        }
        let most = domains
            .iter()
            .enumerate()
            .filter(|(_, d)| populated(d))
            .map(|(i, d)| (i, number(&format!("{}/memory.current", d.cgroup)).unwrap_or(0)))
            .max_by_key(|&(_, held)| held);
        let Some((i, held)) = most else { return };
        let Some(d) = domains.get(i) else { return };
        if std::fs::write(format!("{}/cgroup.kill", d.cgroup), "1").is_ok() {
            say(format!(
                "[{}] shards-init: ended: the agents' memory, {current} bytes, was past their share, {high}, and its {held} the most\n",
                d.label
            )
            .as_bytes());
        }
    }
}

fn cstr_of(s: &str) -> Result<CString, String> {
    CString::new(s).map_err(|_| format!("a NUL in {s:?}"))
}

/// `MemAvailable` of `/proc/meminfo`, in bytes.
fn available_memory() -> Result<u64, String> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").map_err(|e| format!("/proc/meminfo: {e}"))?;
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .and_then(|v| v.trim().strip_suffix("kB"))
        .and_then(|kb| kb.trim().parse::<u64>().ok())
        .and_then(|kb| kb.checked_mul(1024))
        .ok_or_else(|| "/proc/meminfo: no MemAvailable".to_string())
}

/// The Landlock ABI the guest kernel has; an error where it lacks what a domain needs.
fn landlock_abi() -> Result<i64, String> {
    // SAFETY: landlock_create_ruleset(2) asked only its version: no attribute is read.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<landlock::RulesetAttr>(),
            0usize,
            landlock::CREATE_RULESET_VERSION,
        )
    };
    if abi < landlock::NEEDED {
        let why = if abi < 0 {
            io::Error::last_os_error().to_string()
        } else {
            format!("ABI {abi}")
        };
        return Err(format!(
            "the guest kernel's Landlock ({why}) lacks ABI {}, which an agent's domain needs",
            landlock::NEEDED
        ));
    }
    Ok(abi)
}

/// A seccomp filter: its seccomp(2) flags and program.
pub type Filter = (u32, Vec<libc::sock_filter>);

/// The switch the domains' links meet in, kept while the microVM runs: a namespace no
/// process holds lives while a descriptor of it does.
static SWITCH: std::sync::OnceLock<crate::links::Switch> = std::sync::OnceLock::new();

/// The switch `domains` link to: `pairs` of them allowed to open connections to each
/// other, and those with egress grants up through init's namespace and eth0.
fn switch(
    domains: &[Domain],
    pairs: &[(usize, usize, Vec<crate::netplan::Egress>)],
) -> io::Result<crate::links::Switch> {
    let egress: Vec<(usize, Vec<crate::netplan::Egress>)> = domains
        .iter()
        .enumerate()
        .filter_map(|(i, d)| {
            d.link
                .as_ref()
                .filter(|l| !l.egress.is_empty())
                .map(|l| (i, l.egress.clone()))
        })
        .collect();
    let ingress: Vec<(usize, Vec<crate::netplan::Egress>)> = domains
        .iter()
        .enumerate()
        .filter_map(|(i, d)| {
            d.link
                .as_ref()
                .filter(|l| !l.ingress.is_empty())
                .map(|l| (i, l.ingress.clone()))
        })
        .collect();
    let dns: Vec<usize> = domains
        .iter()
        .enumerate()
        .filter(|(_, d)| d.link.as_ref().is_some_and(|l| l.dns || !l.mcp.is_empty()))
        .map(|(i, _)| i)
        .collect();
    let uplink = match egress.is_empty() && ingress.is_empty() && dns.is_empty() {
        true => None,
        false => {
            let (addr, prefix, _) = crate::net::current()
                .ok_or_else(|| io::Error::other("the microVM has no address for its agents' egress"))?;
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            let mut subnets: Vec<(std::net::Ipv4Addr, u8)> = Vec::new();
            for a in domains
                .iter()
                .filter_map(|d| d.link.as_ref())
                .flat_map(|l| &l.addresses)
            {
                if !subnets.contains(&(a.subnet, a.prefix)) {
                    subnets.push((a.subnet, a.prefix));
                }
            }
            // Each to the domain's first address: every one of its addresses is its link's.
            let to_domain = ingress
                .iter()
                .filter_map(|(i, ranges)| {
                    let at = domains.get(*i)?.link.as_ref()?.addresses.first()?.addr;
                    Some((at, ranges.clone()))
                })
                .collect();
            Some(crate::links::Uplink {
                ingress: to_domain,
                subnets,
                eth0: (addr, std::net::Ipv4Addr::from(u32::from(addr) & mask), prefix),
            })
        }
    };
    let resolver = uplink
        .as_ref()
        .and_then(|_| crate::net::current().map(|(_, _, g)| g));
    let switch = crate::links::Switch::new(pairs, &egress, &ingress, &dns, uplink.as_ref())?;
    // The agents' resolver, for those granted names.
    let mut askers: Vec<crate::agentdns::Asker> = Vec::new();
    for &i in &dns {
        let Some(l) = domains.get(i).and_then(|d| d.link.as_ref()) else {
            continue;
        };
        askers.push(crate::agentdns::Asker {
            link: crate::links::link_index(i)?,
            any: l.dns,
            names: l.mcp.iter().map(|(h, _)| h.clone()).collect(),
        });
    }
    if let (false, Some(up)) = (askers.is_empty(), resolver) {
        let (listen, upstream) = switch.resolver_sockets(std::net::SocketAddr::from((up, 53)))?;
        crate::agentdns::start(listen, upstream, askers).map_err(io::Error::other)?;
    }
    Ok(switch)
}

/// Starts each of `domains`, hiding from each the directories of `all` but its own, under
/// the filter of `filters` it needs, which the host compiles (`domains-seccomp=`, and
/// `domains-seccomp-none=` for `--processes=none`); none starts without it. Those with a
/// link are linked once they exist, `pairs` of them allowed to open connections.
pub fn start(
    all: &[Vec<u8>],
    domains: &[Domain],
    pairs: &[(usize, usize, Vec<crate::netplan::Egress>)],
    filters: &[Option<Filter>; 2],
) -> Result<Vec<Started>, String> {
    if domains.is_empty() {
        return Ok(Vec::new());
    }
    match std::fs::create_dir(CGROUPS) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(format!("{CGROUPS}: {e}")),
        _ => {}
    }
    std::fs::write(format!("{CGROUPS}/cgroup.subtree_control"), "+pids +memory")
        .map_err(|e| format!("{CGROUPS}: enabling pids and memory: {e}"))?;
    // The domains together take at most what the microVM can give as they start
    // (`MemAvailable`, Documentation/filesystems/proc.rst), less what the workload's own
    // limit (`-m`) still promises it: never what init and the workload hold or may. Their
    // scratch tmpfs counts, which the OOM killer's choice by resident memory does not
    // (mm/oom_kill.c, `oom_badness`), so the kernel does not choose: past `memory.high`
    // it throttles them and kills none (Documentation/admin-guide/cgroup-v2.rst), in the
    // charge itself as well as on the way back to user space (mm/memcontrol.c,
    // `try_charge_memcg`), and [`Memory`] ends the domain holding the most.
    let promised = crate::run::workload_headroom();
    let available = available_memory()?.saturating_sub(promised);
    std::fs::write(format!("{CGROUPS}/memory.high"), available.to_string())
        .map_err(|e| format!("{CGROUPS}/memory.high: {e}"))?;
    let last_cap = crate::defaults::last_cap();
    landlock_abi()?;
    let lower =
        crate::changes::lower().ok_or("the image's layers were not kept, which a domain's root is")?;
    let skel = skeleton(lower, &[]).map_err(|e| format!("a domain's root: {e}"))?;
    // The Unix sockets granted (`CONNECT --port=unix:<name>`): a directory each, of the
    // tmpfs the run's writable layer lies in, which no path from the run's root reaches and
    // no mount of the run's shares; sticky, so that none removes another's.
    let shared = crate::changes::upper().map(|u| format!("{u}/../agents"));
    let mut made: Vec<(String, String)> = Vec::new();
    for d in domains {
        for (net, name, _) in d.link.as_ref().map(|l| l.unix.as_slice()).unwrap_or_default() {
            if made.iter().any(|(n, m)| n == net && m == name) {
                continue;
            }
            let base = shared
                .as_ref()
                .ok_or("the run's writable layer was not kept, where Unix sockets lie")?;
            let dir = format!("{base}/{net}/{name}");
            std::fs::create_dir_all(&dir).map_err(|e| format!("Unix socket {net}/{name}: {e}"))?;
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777))
                .map_err(|e| format!("Unix socket {net}/{name}: {e}"))?;
            made.push((net.clone(), name.clone()));
        }
    }
    // The in-VM server (§5, D60): an instance for each domain, from the read-only device
    // the daemon attaches to microVMs with agents, mounted beside the run's writable
    // layer, where no path from the run's root reaches, as the Unix sockets granted are.
    let rw = crate::changes::upper()
        .map(|u| format!("{u}/.."))
        .ok_or("the run's writable layer was not kept, where the in-VM server lies")?;
    let server = Server::mount(&rw)?;
    let no_processes = filters
        .get(1)
        .and_then(Option::as_ref)
        .ok_or("the run gave no seccomp filter for the in-VM server")?;
    let mut started = Vec::new();
    for (i, d) in domains.iter().enumerate() {
        let group = format!("{CGROUPS}/{}", d.cgroup);
        std::fs::create_dir(&group).map_err(|e| format!("{group}: {e}"))?;
        if let Some(n) = d.pids {
            std::fs::write(format!("{group}/pids.max"), n.to_string())
                .map_err(|e| format!("{group}/pids.max: {e}"))?;
        }
        // Out of memory, a domain ends whole, and no other with it.
        std::fs::write(format!("{group}/memory.oom.group"), "1")
            .map_err(|e| format!("{group}/memory.oom.group: {e}"))?;
        if let Some(n) = d.memory {
            std::fs::write(format!("{group}/memory.max"), n.to_string())
                .map_err(|e| format!("{group}/memory.max: {e}"))?;
        }
        let cgroup = std::fs::File::open(&group).map_err(|e| format!("{group}: {e}"))?;
        let filter = filters
            .get(usize::from(d.no_processes))
            .and_then(Option::as_ref)
            .ok_or_else(|| format!("{}: the run gave no seccomp filter for it", d.label))?;
        let program = libc::sock_fprog {
            len: u16::try_from(filter.1.len()).map_err(|_| "the domains' seccomp filter is too long")?,
            filter: filter.1.as_ptr().cast_mut(),
        };
        let granted = d.link.as_ref().map(|l| l.unix.as_slice()).unwrap_or_default();
        let mut sockets = Vec::new();
        let mut copies = Vec::new();
        let mut receives = Vec::new();
        for (net, name, receiving) in granted {
            let target = cstr_of(&format!("run/networks/{net}/{name}"))?;
            let base = shared
                .as_ref()
                .ok_or("the run's writable layer was not kept, where Unix sockets lie")?;
            let quiet = libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV | libc::MOUNT_ATTR_NOEXEC;
            let attr = if *receiving {
                quiet
            } else {
                quiet | libc::MOUNT_ATTR_RDONLY
            };
            let copy = copy_of(&format!("{base}/{net}/{name}"), attr)
                .map_err(|e| format!("{}: Unix socket {net}/{name}: {e}", d.label))?;
            let at = cstr_of(&format!("/run/networks/{net}/{name}"))?;
            if *receiving {
                receives.push(at.clone());
            }
            sockets.push((copy.as_raw_fd(), at));
            copies.push((copy, target));
        }
        // Its server instance, listening before it starts, and its way there, read-only:
        // its socket, the instance's CA, its certificate and key.
        let dir = server.start(i, d, no_processes, last_cap)?;
        let quiet = libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV | libc::MOUNT_ATTR_NOEXEC;
        let copy = copy_of(&dir, quiet | libc::MOUNT_ATTR_RDONLY)
            .map_err(|e| format!("{}: the in-VM server: {e}", d.label))?;
        sockets.push((copy.as_raw_fd(), cstr_of(SERVER_AT)?));
        copies.push((copy, cstr_of(SERVER_AT.trim_start_matches('/'))?));
        let own_skel;
        let skel_of = if copies.is_empty() {
            &skel
        } else {
            let dirs: Vec<CString> = copies.iter().map(|(_, t)| t.clone()).collect();
            own_skel = skeleton(lower, &dirs).map_err(|e| format!("{}: its root: {e}", d.label))?;
            &own_skel
        };
        let root = image_root(skel_of, lower).map_err(|e| format!("{}: its root: {e}", d.label))?;
        let prepared = Prepared {
            root: root.as_raw_fd(),
            sockets,
            receives,
            hide: all
                .iter()
                .filter(|x| **x != d.dir)
                .flat_map(|x| [x.clone(), [x.as_slice(), b".d"].concat()])
                .filter_map(|x| CString::new(x).ok())
                .collect(),
            hostname: CString::new(d.cgroup.as_str())
                .map_err(|_| format!("{}: a NUL in its name", d.label))?,
            argv: d
                .argv
                .iter()
                .map(|a| a.as_ptr())
                .chain([std::ptr::null()])
                .collect(),
            envp: d
                .env
                .iter()
                .map(|a| a.as_ptr())
                .chain([std::ptr::null()])
                .collect(),
            last_cap,
            net_handled: match &d.link {
                Some(l) => {
                    (if l.connects { 0 } else { landlock::NET_CONNECT_TCP })
                        | (if l.accepts { 0 } else { landlock::NET_BIND_TCP })
                }
                None => landlock::NET_BIND_TCP | landlock::NET_CONNECT_TCP,
            },
            hosts: d.link.as_ref().map(|l| l.hosts.clone()),
            resolv: d.resolv.clone(),
            go: None,
        };
        let mut prepared = prepared;
        let go = match &d.link {
            Some(_) => {
                let mut fds = [0 as RawFd; 2];
                // SAFETY: pipe2(2) into a two-element array.
                if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
                    return Err(format!(
                        "{}: its link's pipe: {}",
                        d.label,
                        io::Error::last_os_error()
                    ));
                }
                // SAFETY: both descriptors were just made, and are owned here alone.
                let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
                prepared.go = Some(r.as_raw_fd());
                Some((r, w))
            }
            None => None,
        };
        let mut fds = [0 as RawFd; 2];
        // SAFETY: pipe2(2) into a two-element array.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(format!(
                "{}: its output pipe: {}",
                d.label,
                io::Error::last_os_error()
            ));
        }
        // SAFETY: both descriptors were just made, and are owned here alone.
        let (read_end, write_end) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let mut args = CloneArgs {
            flags: (libc::CLONE_NEWNS
                | libc::CLONE_NEWPID
                | libc::CLONE_NEWIPC
                | libc::CLONE_NEWNET
                | libc::CLONE_NEWUTS
                | libc::CLONE_NEWCGROUP) as u64
                | CLONE_INTO_CGROUP,
            exit_signal: libc::SIGCHLD as u64,
            cgroup: cgroup.as_raw_fd() as u64,
            ..CloneArgs::default()
        };
        // SAFETY: clone3(2) with a clone_args of the size given. The child calls only the
        // kernel, on what `prepared` holds, and execs or exits.
        let pid = unsafe { libc::syscall(libc::SYS_clone3, &raw mut args, std::mem::size_of::<CloneArgs>()) };
        if pid < 0 {
            return Err(format!("starting {}: {}", d.label, io::Error::last_os_error()));
        }
        if pid == 0 {
            child(d, &prepared, write_end.as_raw_fd(), filter.0, &program);
        }
        drop(write_end);
        drop(root);
        drop(copies);
        if let (Some(link), Some((r, w))) = (&d.link, go) {
            drop(r);
            let switch = match SWITCH.get() {
                Some(s) => Ok(s),
                None => switch(domains, pairs).map(|s| SWITCH.get_or_init(|| s)),
            };
            let linked = switch.and_then(|s| s.attach(i, pid as libc::pid_t, link));
            if let Err(e) = linked {
                // SAFETY: kill(2) of the child just made, which waits for its link.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                return Err(format!("{}: its link: {e}", d.label));
            }
            // SAFETY: write(2) of one byte to the pipe it waits on.
            if unsafe { libc::write(w.as_raw_fd(), b"g".as_ptr().cast(), 1) } != 1 {
                return Err(format!(
                    "{}: its link's pipe: {}",
                    d.label,
                    io::Error::last_os_error()
                ));
            }
        }
        crate::run::set_nonblocking(read_end.as_raw_fd(), true);
        started.push(Started {
            label: d.label.clone(),
            cgroup: group.clone(),
            out: Some(read_end),
            partial: Vec::new(),
        });
    }
    Ok(started)
}

/// Where a domain finds its server instance: its socket, the instance's CA, and its own
/// certificate and key.
const SERVER_AT: &str = "/run/shards";
/// The device the in-VM server lies on, which the daemon attaches after the root
/// filesystem's (vm_run.rs): its number, as sysfs gives it. The run's /dev holds its own
/// devices alone, as a container's does, so init makes a node of its own for it.
const SERVER_DEVICE: &str = "/sys/block/pmem1/dev";

/// The in-VM server's binary, mounted, and where its instances' directories go.
struct Server {
    binary: OwnedFd,
    base: String,
}

impl Server {
    /// Mounts the server's device read-only (EROFS, DAX: its pages are the host's, not the
    /// guest's memory), once, under `rw`.
    fn mount(rw: &str) -> Result<Server, String> {
        let number = std::fs::read_to_string(SERVER_DEVICE)
            .map_err(|e| format!("the in-VM server's device ({SERVER_DEVICE}): {e}"))?;
        let (major, minor) = number
            .trim()
            .split_once(':')
            .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
            .ok_or_else(|| format!("the in-VM server's device: {SERVER_DEVICE} says {number:?}"))?;
        let node = cstr_of(&format!("{rw}/server-device"))?;
        // SAFETY: mknod(2) of a NUL-terminated path; an old one from a restored template
        // is the same device.
        if unsafe { libc::mknod(node.as_ptr(), libc::S_IFBLK | 0o600, libc::makedev(major, minor)) } != 0
            && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
        {
            return Err(format!(
                "the in-VM server's device: {}",
                io::Error::last_os_error()
            ));
        }
        let at = format!("{rw}/server-binary");
        std::fs::create_dir_all(&at).map_err(|e| format!("{at}: {e}"))?;
        let target = cstr_of(&at)?;
        // SAFETY: mount(2) of NUL-terminated strings.
        if unsafe {
            libc::mount(
                node.as_ptr(),
                target.as_ptr(),
                c"erofs".as_ptr(),
                libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
                c"dax=always".as_ptr().cast(),
            )
        } != 0
        {
            return Err(format!(
                "mounting the in-VM server's device: {}",
                io::Error::last_os_error()
            ));
        }
        let path = cstr_of(&format!("{at}/shards-server"))?;
        // SAFETY: open(2) of a NUL-terminated path, its descriptor owned below.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(format!(
                "the in-VM server's binary: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(Server {
            // SAFETY: the descriptor open just made, owned here alone.
            binary: unsafe { OwnedFd::from_raw_fd(fd) },
            base: format!("{rw}/server"),
        })
    }

    /// Starts the `n`-th domain's instance, least-privileged (its own uid and gid, the
    /// domain's group to give its files to, no capability, `no_new_privs`, its own
    /// network, IPC and UTS namespaces), confined by `filter` once it listens; returns
    /// the directory it listens in, once it does.
    fn start(&self, n: usize, d: &Domain, filter: &Filter, last_cap: u32) -> Result<String, String> {
        let fail = |e: String| format!("{}: its server: {e}", d.label);
        let id = u32::try_from(n)
            .ok()
            .and_then(|n| shards_abi::SERVER_FIRST_ID.checked_add(n))
            .ok_or_else(|| fail("no uid left".into()))?;
        let dir = format!("{}/{}", self.base, d.cgroup);
        std::fs::create_dir_all(&dir).map_err(|e| fail(format!("{dir}: {e}")))?;
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750))
            .map_err(|e| fail(e.to_string()))?;
        std::os::unix::fs::chown(&dir, Some(id), Some(d.id)).map_err(|e| fail(e.to_string()))?;
        // Its directory, which no path of its own reaches, as its descriptor 6.
        let dir_path = cstr_of(&dir)?;
        // SAFETY: open(2) of a NUL-terminated path, its descriptor owned below.
        let dir_fd = unsafe {
            libc::open(
                dir_path.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if dir_fd < 0 {
            return Err(fail(format!("{dir}: {}", io::Error::last_os_error())));
        }
        // SAFETY: the descriptor open just made, owned here alone.
        let dir_fd = unsafe { OwnedFd::from_raw_fd(dir_fd) };
        let argv_owned = [
            cstr_of("shards-server")?,
            cstr_of(&d.label)?,
            cstr_of("/proc/self/fd/6")?,
            cstr_of(&d.id.to_string())?,
            cstr_of("3")?,
            cstr_of("4")?,
        ];
        let argv: Vec<*const libc::c_char> = argv_owned
            .iter()
            .map(|a| a.as_ptr())
            .chain([std::ptr::null()])
            .collect();
        let envp = [std::ptr::null::<libc::c_char>()];
        let pipe = || -> Result<(OwnedFd, OwnedFd), String> {
            let mut fds = [0 as RawFd; 2];
            // SAFETY: pipe2(2) into a two-element array.
            if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
                return Err(fail(io::Error::last_os_error().to_string()));
            }
            // SAFETY: both descriptors were just made, and are owned here alone.
            Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
        };
        let (ready_r, ready_w) = pipe()?;
        let (filter_r, filter_w) = pipe()?;
        // What it says before it listens, if it ends: its stderr until then.
        let (said_r, said_w) = pipe()?;
        let groups = [d.id];
        // SAFETY: fork(2) from init with no other thread running; the child calls only the
        // kernel, on what was made above, and execs or exits.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(fail(io::Error::last_os_error().to_string()));
        }
        if pid == 0 {
            // SAFETY: system calls on descriptors and buffers made before the fork.
            unsafe {
                let up = |fd: RawFd| libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10);
                let (r, f, b, e, w) = (
                    up(ready_w.as_raw_fd()),
                    up(filter_r.as_raw_fd()),
                    up(self.binary.as_raw_fd()),
                    up(said_w.as_raw_fd()),
                    up(dir_fd.as_raw_fd()),
                );
                let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
                if r < 0
                    || f < 0
                    || b < 0
                    || e < 0
                    || w < 0
                    || null < 0
                    || libc::dup2(null, 0) < 0
                    || libc::dup2(null, 1) < 0
                    || libc::dup2(e, 2) < 0
                    || libc::dup2(r, 3) < 0
                    || libc::dup2(f, 4) < 0
                    || libc::dup2(b, 5) < 0
                    || libc::fcntl(5, libc::F_SETFD, libc::FD_CLOEXEC) < 0
                    || libc::dup2(w, 6) < 0
                    || libc::syscall(libc::SYS_close_range, 7u32, u32::MAX, 0u32) != 0
                    || libc::unshare(libc::CLONE_NEWNET | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS) != 0
                    || !crate::defaults::bound(last_cap, |_| false)
                    || libc::setgroups(groups.len(), groups.as_ptr()) != 0
                    || libc::setresgid(id, id, id) != 0
                    || libc::setresuid(id, id, id) != 0
                    || !crate::defaults::set(last_cap, |_| false)
                    || libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) != 0
                    || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                {
                    libc::_exit(126);
                }
                libc::syscall(
                    libc::SYS_execveat,
                    5,
                    c"".as_ptr(),
                    argv.as_ptr(),
                    envp.as_ptr(),
                    libc::AT_EMPTY_PATH,
                );
                libc::_exit(127)
            }
        }
        drop(ready_w);
        drop(filter_r);
        drop(said_w);
        // Its filter, as init's setup carries one: flags, then the program.
        let mut bytes = filter.0.to_le_bytes().to_vec();
        for i in &filter.1 {
            bytes.extend_from_slice(&i.code.to_ne_bytes());
            bytes.extend_from_slice(&[i.jt, i.jf]);
            bytes.extend_from_slice(&i.k.to_ne_bytes());
        }
        use std::io::{Read as _, Write as _};
        std::fs::File::from(filter_w)
            .write_all(&bytes)
            .map_err(|e| fail(format!("giving it its filter: {e}")))?;
        let mut said = [0u8; 1];
        match std::fs::File::from(ready_r).read(&mut said) {
            Ok(1) => Ok(dir),
            _ => {
                // Why: its status (126 a step before its exec, 127 the exec), and what it
                // wrote.
                let mut status = 0;
                // SAFETY: waitpid(2) of the child just forked, which has ended or will.
                unsafe { libc::waitpid(pid, &raw mut status, 0) };
                let mut text = String::new();
                let _ = std::fs::File::from(said_r).take(4096).read_to_string(&mut text);
                Err(fail(format!(
                    "it ended before it listened (status {}): {}",
                    libc::WEXITSTATUS(status),
                    text.trim()
                )))
            }
        }
    }
}

/// Writes `shards-init: <what>: errno <n>` to `out` and exits 125, without allocating.
fn fail(out: RawFd, what: &CStr) -> ! {
    let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
    let mut digits = [0u8; 12];
    let mut at = digits.len();
    let mut v = errno.unsigned_abs();
    loop {
        at -= 1;
        if let Some(d) = digits.get_mut(at) {
            *d = b'0' + (v % 10) as u8;
        }
        v /= 10;
        if v == 0 || at == 0 {
            break;
        }
    }
    let parts: [&[u8]; 5] = [
        b"shards-init: ",
        what.to_bytes(),
        b": errno ",
        digits.get(at..).unwrap_or_default(),
        b"\n",
    ];
    for p in parts {
        // SAFETY: write(2) of a buffer it borrows.
        unsafe { libc::write(out, p.as_ptr().cast(), p.len()) };
    }
    // SAFETY: _exit(2), past which nothing runs.
    unsafe { libc::_exit(125) }
}

/// A domain's first process, before its program.
fn child(d: &Domain, p: &Prepared, out: RawFd, flags: u32, program: &libc::sock_fprog) -> ! {
    let null = std::ptr::null::<libc::c_char>();
    let nil = std::ptr::null::<libc::c_void>();
    let tmpfs = c"tmpfs".as_ptr();
    // SAFETY: each call below is a system call on NUL-terminated strings and buffers made
    // before the clone, in a process of one thread.
    unsafe {
        // Nothing of these mounts reaches init's namespace.
        if libc::mount(null, c"/".as_ptr(), null, libc::MS_REC | libc::MS_PRIVATE, nil) != 0 {
            fail(out, c"making its mounts private");
        }
        // The image as built, over the system's /proc, then its root; the system's mounts
        // under it detached (pivot_root(2), NOTES).
        if libc::syscall(
            libc::SYS_move_mount,
            p.root,
            c"".as_ptr(),
            libc::AT_FDCWD,
            c"/proc".as_ptr(),
            mount_api::MOVE_MOUNT_F_EMPTY_PATH,
        ) != 0
            || libc::chdir(c"/proc".as_ptr()) != 0
            || libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".".as_ptr()) != 0
            || libc::umount2(c".".as_ptr(), libc::MNT_DETACH) != 0
            || libc::chdir(c"/".as_ptr()) != 0
        {
            fail(out, c"entering the image as built");
        }
        // The microVM's system, read-only, nosuid and nodev (struct mount_attr).
        let attr: [u64; 4] = [
            libc::MOUNT_ATTR_RDONLY | libc::MOUNT_ATTR_NOSUID | libc::MOUNT_ATTR_NODEV,
            0,
            0,
            0,
        ];
        if libc::syscall(
            libc::SYS_mount_setattr,
            libc::AT_FDCWD,
            c"/".as_ptr(),
            libc::AT_RECURSIVE,
            attr.as_ptr(),
            std::mem::size_of_val(&attr),
        ) != 0
        {
            fail(out, c"making the system read-only");
        }
        let shut = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
        let empty = c"size=4k,nr_inodes=1,mode=000".as_ptr().cast();
        // Every other domain, and /sys, hidden.
        for h in &p.hide {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::stat(h.as_ptr(), &raw mut st) == 0
                && libc::mount(tmpfs, h.as_ptr(), tmpfs, shut, empty) != 0
            {
                fail(out, c"hiding another domain");
            }
        }
        if libc::mount(tmpfs, c"/sys".as_ptr(), tmpfs, shut, empty) != 0 {
            fail(out, c"hiding /sys");
        }
        // Its PID namespace's /proc.
        let quiet = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
        if libc::mount(c"proc".as_ptr(), c"/proc".as_ptr(), c"proc".as_ptr(), quiet, nil) != 0 {
            fail(out, c"mounting its /proc");
        }
        // Its /dev.
        let dev = c"mode=755,size=64k".as_ptr().cast();
        if libc::mount(
            tmpfs,
            c"/dev".as_ptr(),
            tmpfs,
            libc::MS_NOSUID | libc::MS_NOEXEC,
            dev,
        ) != 0
        {
            fail(out, c"mounting its /dev");
        }
        for (path, major, minor) in DEV {
            if libc::mknod(path.as_ptr(), libc::S_IFCHR | 0o666, libc::makedev(major, minor)) != 0
                || libc::chmod(path.as_ptr(), 0o666) != 0
            {
                fail(out, c"making its /dev");
            }
        }
        for (target, path) in LINKS {
            if libc::symlink(target.as_ptr(), path.as_ptr()) != 0 {
                fail(out, c"linking its /dev");
            }
        }
        if libc::mkdir(c"/dev/shm".as_ptr(), 0o755) != 0 || libc::mkdir(c"/dev/mqueue".as_ptr(), 0o755) != 0 {
            fail(out, c"making its /dev/shm and /dev/mqueue");
        }
        let ro_dev = libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NOEXEC;
        if libc::mount(null, c"/dev".as_ptr(), null, ro_dev, nil) != 0 {
            fail(out, c"making its /dev read-only");
        }
        // Its own scratch: /tmp and /dev/shm.
        for (path, data) in [
            (c"/tmp", c"mode=700".as_ptr()),
            (c"/dev/shm", c"mode=700,size=65536k".as_ptr()),
        ] {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::stat(path.as_ptr(), &raw mut st) != 0 {
                continue;
            }
            if libc::mount(
                tmpfs,
                path.as_ptr(),
                tmpfs,
                libc::MS_NOSUID | libc::MS_NODEV,
                data.cast(),
            ) != 0
                || libc::chown(path.as_ptr(), d.id, d.id) != 0
            {
                fail(out, c"making its scratch");
            }
        }
        // Its POSIX message queues, of its own IPC namespace, as a Docker container's
        // (moby daemon/pkg/oci/defaults.go): mq_open makes them in the same filesystem
        // (ipc/mqueue.c, the namespace's `mq_mnt`), which Landlock lets it write beneath.
        if libc::mount(
            c"mqueue".as_ptr(),
            c"/dev/mqueue".as_ptr(),
            c"mqueue".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
            nil,
        ) != 0
        {
            fail(out, c"mounting its /dev/mqueue");
        }
        // Its names: its peers', over the system's /etc/hosts, and where it reaches past
        // the microVM its resolver, over /etc/resolv.conf; each read-only, made in its
        // scratch and left there by no name.
        let ro = libc::MS_REMOUNT
            | libc::MS_BIND
            | libc::MS_RDONLY
            | libc::MS_NOSUID
            | libc::MS_NODEV
            | libc::MS_NOEXEC;
        for (content, target, made, writing, mounting) in [
            (
                &p.hosts,
                c"/etc/hosts",
                c"/tmp/.hosts",
                c"writing its /etc/hosts",
                c"mounting its /etc/hosts",
            ),
            (
                &p.resolv,
                c"/etc/resolv.conf",
                c"/tmp/.resolv",
                c"writing its /etc/resolv.conf",
                c"mounting its /etc/resolv.conf",
            ),
        ] {
            let mut st: libc::stat = std::mem::zeroed();
            let Some(content) = content else { continue };
            if libc::stat(target.as_ptr(), &raw mut st) != 0 || libc::stat(c"/tmp".as_ptr(), &raw mut st) != 0
            {
                continue;
            }
            let fd = libc::open(
                made.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC,
                0o444,
            );
            if fd < 0 || libc::write(fd, content.as_ptr().cast(), content.len()) != content.len() as isize {
                fail(out, writing);
            }
            libc::close(fd);
            if libc::mount(made.as_ptr(), target.as_ptr(), null, libc::MS_BIND, nil) != 0
                || libc::mount(null, target.as_ptr(), null, ro, nil) != 0
                || libc::unlink(made.as_ptr()) != 0
            {
                fail(out, mounting);
            }
        }
        // Its Unix sockets' directories, each its own mount, read-only where it only
        // connects: connecting needs no write to the directory, and making a socket does.
        for (copy, at) in &p.sockets {
            if libc::syscall(
                libc::SYS_move_mount,
                *copy,
                c"".as_ptr(),
                libc::AT_FDCWD,
                at.as_ptr(),
                mount_api::MOVE_MOUNT_F_EMPTY_PATH,
            ) != 0
            {
                fail(out, c"mounting its Unix sockets");
            }
        }
        // One that receives makes its sockets for others' uids to connect to (unix(7):
        // connecting needs write permission on the socket), and `bind` applies its umask
        // (net/unix/af_unix.c, `unix_bind_bsd`): none, whom the directory shows to only those
        // granted.
        if !p.receives.is_empty() {
            libc::umask(0);
        }
        // Its own name, and loopback.
        if libc::sethostname(p.hostname.as_ptr(), p.hostname.as_bytes().len()) != 0 {
            fail(out, c"naming its host");
        }
        if !crate::run::loopback_up_raw() {
            fail(out, c"bringing its loopback up");
        }
        // Its link, which init makes once it exists.
        if let Some(go) = p.go {
            let mut byte = 0u8;
            if libc::read(go, (&raw mut byte).cast(), 1) != 1 {
                fail(out, c"waiting for its link");
            }
        }
        // Its stdio, and no other descriptor of init's.
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if devnull < 0 || libc::dup2(devnull, 0) < 0 || libc::dup2(out, 1) < 0 || libc::dup2(out, 2) < 0 {
            fail(out, c"giving it its stdio");
        }
        if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) != 0 {
            fail(2, c"closing init's descriptors");
        }
        let out = 2;
        if libc::chdir(d.workdir.as_ptr()) != 0 {
            fail(out, c"entering its working directory");
        }
        // Its own IDs and no capability: the bounding set first, which needs CAP_SETPCAP;
        // the permitted and effective sets go as no uid stays 0 (capabilities(7), "Effect
        // of user ID changes on capabilities"); the inheritable and ambient sets cleared.
        if !crate::defaults::bound(p.last_cap, |_| false) {
            fail(out, c"dropping its capabilities");
        }
        if libc::setgroups(0, std::ptr::null()) != 0
            || libc::setresgid(d.id, d.id, d.id) != 0
            || libc::setresuid(d.id, d.id, d.id) != 0
        {
            fail(out, c"taking its own IDs");
        }
        if !crate::defaults::set(p.last_cap, |_| false)
            || libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) != 0
        {
            fail(out, c"clearing its capabilities");
        }
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            fail(out, c"setting no_new_privs");
        }
        // Landlock, a layer under the mounts': every filesystem right handled, and given
        // back as reads and execution everywhere, writes to /dev's nodes, and everything
        // in its scratch; every TCP bind and connect handled, and none given, as no
        // network grant is yet; signals and abstract Unix sockets scoped to the domain.
        let attr = landlock::RulesetAttr {
            handled_access_fs: landlock::FS_ALL,
            handled_access_net: p.net_handled,
            scoped: landlock::SCOPE_ABSTRACT_UNIX_SOCKET | landlock::SCOPE_SIGNAL,
        };
        let ruleset = libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &raw const attr,
            std::mem::size_of::<landlock::RulesetAttr>(),
            0u32,
        );
        if ruleset < 0 {
            fail(out, c"making its Landlock ruleset");
        }
        let read = landlock::READ_FILE | landlock::READ_DIR | landlock::EXECUTE;
        for (path, allowed) in [
            (c"/", read),
            (c"/dev", read | landlock::WRITE_FILE | landlock::IOCTL_DEV),
            (c"/dev/shm", landlock::FS_ALL),
            (c"/dev/mqueue", landlock::FS_ALL),
            (c"/tmp", landlock::FS_ALL),
        ] {
            let fd = libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC);
            if fd < 0 {
                continue;
            }
            let rule = landlock::PathBeneathAttr {
                allowed_access: allowed,
                parent_fd: fd,
            };
            let added = libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset,
                landlock::RULE_PATH_BENEATH,
                &raw const rule,
                0u32,
            );
            libc::close(fd);
            if added != 0 {
                fail(out, c"adding a Landlock rule");
            }
        }
        for at in &p.receives {
            let fd = libc::open(at.as_ptr(), libc::O_PATH | libc::O_CLOEXEC);
            let rule = landlock::PathBeneathAttr {
                allowed_access: landlock::MAKE_SOCK | landlock::REMOVE_FILE,
                parent_fd: fd,
            };
            if fd < 0
                || libc::syscall(
                    libc::SYS_landlock_add_rule,
                    ruleset,
                    landlock::RULE_PATH_BENEATH,
                    &raw const rule,
                    0u32,
                ) != 0
            {
                fail(out, c"adding a Landlock rule for its Unix sockets");
            }
            libc::close(fd);
        }
        if libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) != 0 {
            fail(out, c"entering its Landlock ruleset");
        }
        libc::close(ruleset as libc::c_int);
        // Its seccomp filter, last: what it refuses, nothing after may need.
        if libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            flags,
            std::ptr::from_ref(program),
        ) != 0
        {
            fail(out, c"loading its seccomp filter");
        }
        libc::execve(
            p.argv.first().copied().unwrap_or(std::ptr::null()),
            p.argv.as_ptr(),
            p.envp.as_ptr(),
        );
        fail(out, c"running its command")
    }
}
