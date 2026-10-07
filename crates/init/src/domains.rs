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
    pub argv: Vec<CString>,
    pub env: Vec<CString>,
    pub workdir: CString,
}

/// The directories of every domain the image's normalized Agentfile declares, and those
/// domains that say how they run; nothing where the image has no Agentfile.
pub fn read() -> Result<(Vec<Vec<u8>>, Vec<Domain>), String> {
    let text = match std::fs::read("/.agentfile.json") {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), Vec::new())),
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
    Ok((dirs, out))
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
    // `--processes` as declared, else what its config asks, else the microVM's.
    let pids = match declared.get("processes") {
        Some(Value::String(s)) if s == "none" => Some(1),
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
        argv,
        env,
        workdir,
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

/// Everything a domain's first process uses, made before it is: after `clone3` it calls
/// the kernel alone.
struct Prepared {
    hide: Vec<CString>,
    hostname: CString,
    argv: Vec<*const libc::c_char>,
    envp: Vec<*const libc::c_char>,
    last_cap: u32,
}

/// Starts each of `domains`, hiding from each the directories of `all` but its own.
pub fn start(all: &[Vec<u8>], domains: &[Domain]) -> Result<Vec<Started>, String> {
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
    let mut started = Vec::new();
    for d in domains {
        let group = format!("{CGROUPS}/{}", d.cgroup);
        std::fs::create_dir(&group).map_err(|e| format!("{group}: {e}"))?;
        if let Some(n) = d.pids {
            std::fs::write(format!("{group}/pids.max"), n.to_string())
                .map_err(|e| format!("{group}/pids.max: {e}"))?;
        }
        let cgroup = std::fs::File::open(&group).map_err(|e| format!("{group}: {e}"))?;
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
            child(d, &prepared, write_end.as_raw_fd());
        }
        drop(write_end);
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
fn child(d: &Domain, p: &Prepared, out: RawFd) -> ! {
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
        // Its own name, and loopback.
        if libc::sethostname(p.hostname.as_ptr(), p.hostname.as_bytes().len()) != 0 {
            fail(out, c"naming its host");
        }
        if !crate::run::loopback_up_raw() {
            fail(out, c"bringing its loopback up");
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
        libc::execve(
            p.argv.first().copied().unwrap_or(std::ptr::null()),
            p.argv.as_ptr(),
            p.envp.as_ptr(),
        );
        fail(out, c"running its command")
    }
}
