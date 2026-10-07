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
//! - sees the microVM's system read-only, `nosuid` and `nodev` (workloads inherit the
//!   microVM's OS), its own directory and grants with it; every other domain's directory
//!   and grants hidden under an empty tmpfs no one may read; `/sys` hidden; a `/proc` of
//!   its PID namespace; a `/dev` of six nodes, the `fd` and stdio links, and a `shm` of
//!   its own; and a scratch tmpfs of its own at `/tmp`, lost when it ends;
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
pub const FIRST_ID: u32 = 200_000;
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
pub type Read = (Vec<Vec<u8>>, Vec<Domain>, Vec<(usize, usize)>);

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
    let gateway = crate::net::current().map(|(_, _, g)| g);
    for (d, link) in out.iter_mut().zip(plan.links) {
        if let (Some(l), Some(g)) = (&link, gateway)
            && !l.egress.is_empty()
        {
            d.resolv = Some(format!("nameserver {g}\noptions ndots:0\n").into_bytes());
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
fn switch(domains: &[Domain], pairs: &[(usize, usize)]) -> io::Result<crate::links::Switch> {
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
    let uplink = match egress.is_empty() && ingress.is_empty() {
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
    crate::links::Switch::new(pairs, &egress, &ingress, uplink.as_ref(), resolver)
}

/// Starts each of `domains`, hiding from each the directories of `all` but its own, under
/// the filter of `filters` it needs, which the host compiles (`domains-seccomp=`, and
/// `domains-seccomp-none=` for `--processes=none`); none starts without it. Those with a
/// link are linked once they exist, `pairs` of them allowed to open connections.
pub fn start(
    all: &[Vec<u8>],
    domains: &[Domain],
    pairs: &[(usize, usize)],
    filters: &[Option<Filter>; 2],
) -> Result<Vec<Started>, String> {
    if domains.is_empty() {
        return Ok(Vec::new());
    }
    match std::fs::create_dir(CGROUPS) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(format!("{CGROUPS}: {e}")),
        _ => {}
    }
    std::fs::write(format!("{CGROUPS}/cgroup.subtree_control"), "+pids")
        .map_err(|e| format!("{CGROUPS}: enabling pids: {e}"))?;
    let last_cap = crate::defaults::last_cap();
    landlock_abi()?;
    let mut started = Vec::new();
    for (i, d) in domains.iter().enumerate() {
        let group = format!("{CGROUPS}/{}", d.cgroup);
        std::fs::create_dir(&group).map_err(|e| format!("{group}: {e}"))?;
        if let Some(n) = d.pids {
            std::fs::write(format!("{group}/pids.max"), n.to_string())
                .map_err(|e| format!("{group}/pids.max: {e}"))?;
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
        let prepared = Prepared {
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
            out: Some(read_end),
            partial: Vec::new(),
        });
    }
    Ok(started)
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
        if libc::mkdir(c"/dev/shm".as_ptr(), 0o755) != 0 {
            fail(out, c"making its /dev/shm");
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
