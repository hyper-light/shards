//! A builder's memory, plugged as its build needs it (D133). The builder boots with little
//! memory, and its VM has a virtio-mem device of what it may have in all
//! (shards_abi::control::HOTPLUG_SIZE). Init:
//!
//! - raises what the kernel sized by the memory it booted with to what it sizes by all of
//!   it ([`Plug::raise_limits`]), so that a build sees the limits of a builder booted with
//!   all its memory;
//! - holds the build, itself and its steps, in a cgroup whose `memory.high` is the memory
//!   the builder has, less what its kernel needs ([`Plug::hold`]): past it the kernel slows
//!   the build's allocations, in the charge and on the way back to user space, and ends
//!   none, and says so in `memory.events` (Documentation/admin-guide/cgroup-v2.rst);
//! - and leaves a process outside that cgroup, which the kernel never slows, to ask the
//!   host for twice the memory each time the build reaches its `memory.high`, and to raise
//!   it as the memory comes online ([`grow`]).

use std::ffi::CString;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use shards_abi::control;

use crate::linux::{control_read64, control_write64};

/// The build's cgroup: init in `init`, each step in one of its own under `steps`.
pub const BUILD: &str = "/sys/fs/cgroup/build";
/// What the memory to plug is asked for in: Linux's memory block with 4 KiB pages
/// (shards_vmm's `virtio::mem::ALIGN`), which its driver adds memory in.
const ALIGN: u64 = 128 << 20;
/// What the kernel needs beside the build that its cgroup is not charged for, and what
/// the build may pass its `memory.high` by before the kernel's slowing catches it
/// (PM M193), on top of the page structures of the next memory plugged ([`reserve`]).
const SLACK: u64 = 64 << 20;
/// How long the memory asked for may take to come online, from the last of it that came,
/// before the process that grows it waits for the build to ask again: the driver retries
/// what it could not plug (drivers/virtio/virtio_mem.c, its retry timer).
const STALLED: Duration = Duration::from_secs(5);
/// inotify's cost of a watch and epoll's of an item in this kernel, in bytes
/// (`INOTIFY_WATCH_COST`, `EP_ITEM_COST`): what makes their limits 1:28.17 at every size
/// the pinned kernel was measured at, 4, 16 and 64 GiB (PM M193).
const INOTIFY_WATCH_COST: u64 = 1296;
const EP_ITEM_COST: u64 = 184;
/// inotify's per-user watches, as the kernel clamps them
/// (fs/notify/inotify/inotify_user.c, `inotify_user_setup`).
const INOTIFY_WATCHES: (u64, u64) = (8192, 1_048_576);
/// TCP's largest receive and send buffers (net/ipv4/tcp.c, `tcp_init`): within these, a
/// 128th of the memory.
const TCP_RMEM_MAX: u64 = 32 << 20;
const TCP_WMEM_MAX: u64 = 4 << 20;

/// The kernel's limits sized by its memory at boot, each in proportion to it (kernel/fork.c
/// `fork_init`, fs/eventpoll.c `eventpoll_init`, net/ipv4/tcp.c `tcp_init`, net/ipv4/udp.c
/// `udp_init`), as PM M193 measured them.
const LINEAR: &[&str] = &[
    "kernel/threads-max",
    "user/max_user_namespaces",
    "user/max_pid_namespaces",
    "user/max_uts_namespaces",
    "user/max_ipc_namespaces",
    "user/max_net_namespaces",
    "user/max_mnt_namespaces",
    "user/max_cgroup_namespaces",
    "user/max_time_namespaces",
    "fs/epoll/max_user_watches",
    "net/ipv4/tcp_mem",
    "net/ipv4/udp_mem",
];
/// TCP's orphans and TIME-WAIT sockets, each half its established table's buckets
/// (net/ipv4/tcp.c `tcp_init`): a bucket for each 128 KiB of the memory rounded up to a
/// MiB, rounded up to a power of two, at most 512 Ki (mm/mm_init.c
/// `alloc_large_system_hash`).
const TCP_TABLE: &[&str] = &["net/ipv4/tcp_max_orphans", "net/ipv4/tcp_max_tw_buckets"];
const TCP_TABLE_MOST: u64 = 512 << 10;
/// Files: one for each 10 KiB of the memory the kernel has as it starts its caches, less
/// half again what it has used by then (fs/file_table.c, `files_maxfiles_init`), so more
/// memory adds a file for each 10 KiB of it; a proportion would scale what it had used
/// too, and came out short of a boot with all of it (PM M193, x86_64).
const FILE_BYTES: u64 = 10 << 10;

/// The memory a builder may have plugged, and what it booted with.
#[derive(Debug, Clone, Copy)]
pub struct Plug {
    /// MemTotal as it booted.
    boot: u64,
    /// The most it may have plugged.
    most: u64,
}

fn err(what: impl std::fmt::Display) -> io::Error {
    io::Error::other(what.to_string())
}

/// The builder's memory to plug, where its VM has a device for it.
pub fn plug() -> io::Result<Option<Plug>> {
    let most = control_read64(control::HOTPLUG_SIZE)?;
    if most == 0 {
        return Ok(None);
    }
    Ok(Some(Plug {
        boot: mem_total()?,
        most,
    }))
}

/// The kernel's memory and what of it is free, in bytes: /proc/meminfo's MemTotal and
/// MemFree, from sysinfo(2), which both come from (mm/show_mem.c `si_meminfo`), without
/// a file: init asks before it has mounted /proc. Memory plugged counts as it comes online.
fn memory() -> io::Result<(u64, u64)> {
    // SAFETY: a zeroed sysinfo is a valid out-parameter, which sysinfo(2) fills.
    let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
    // SAFETY: sysinfo(2) writes `info`.
    if unsafe { libc::sysinfo(&mut info) } != 0 {
        return Err(err(format!("sysinfo: {}", io::Error::last_os_error())));
    }
    let unit = u64::from(info.mem_unit.max(1));
    Ok((
        info.totalram.saturating_mul(unit),
        info.freeram.saturating_mul(unit),
    ))
}

/// MemTotal: what the kernel has, memory plugged included as it comes online.
fn mem_total() -> io::Result<u64> {
    memory().map(|(total, _)| total)
}

fn page_size() -> u64 {
    // SAFETY: sysconf(3) reads a constant.
    u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap_or(4096)
}

/// A number from a file: a cgroup's or the kernel's.
fn number(path: &str) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// `v` in the proportion `to` is to `from`, without overflow.
fn scaled(v: u64, to: u64, from: u64) -> u64 {
    let s = u128::from(v) * u128::from(to) / u128::from(from.max(1));
    u64::try_from(s).unwrap_or(u64::MAX)
}

impl Plug {
    /// MemTotal with all of it plugged: each page plugged is the kernel's, its page
    /// structures taken from what it has.
    pub fn full(&self) -> u64 {
        self.boot.saturating_add(self.most)
    }

    /// The options of a tmpfs that holds as much as one does in a builder booted with all
    /// its memory: `size=100%` and the default `nr_inodes`, half the pages (mm/shmem.c,
    /// `shmem_default_max_inodes`), both of all of it. A size of 100% would be of the
    /// memory it boots with.
    pub fn tmpfs(&self) -> String {
        let full = self.full();
        format!("size={full},nr_inodes={}", full / page_size() / 2)
    }

    /// Raises what the kernel sized by the memory it booted with to what it sizes by all
    /// of it: each limit in proportion to the memory scaled by their ratio; files by the
    /// memory added; TCP's buffers within their caps; TCP's orphans and TIME-WAIT buckets
    /// by its table's sizing; inotify's watches, which the kernel holds at 8192 below
    /// about 1 GiB, in their proportion to epoll's items, within their clamps; and init's
    /// RLIMIT_NPROC and RLIMIT_SIGPENDING, half the threads, as the kernel gives PID 1
    /// (`fork_init`), which every step inherits.
    pub fn raise_limits(&self) -> io::Result<()> {
        let (to, from) = (self.full(), self.boot);
        let sysctl = |name: &str| format!("/proc/sys/{name}");
        for name in LINEAR {
            let path = sysctl(name);
            let now = std::fs::read_to_string(&path).map_err(|e| err(format!("{path}: {e}")))?;
            let raised: Vec<String> = now
                .split_whitespace()
                .map(|v| v.parse::<u64>().map(|v| scaled(v, to, from).to_string()))
                .collect::<Result<_, _>>()
                .map_err(|e| err(format!("{path}: {e}")))?;
            std::fs::write(&path, raised.join(" ")).map_err(|e| err(format!("{path}: {e}")))?;
        }
        for (name, cap) in [
            ("net/ipv4/tcp_rmem", TCP_RMEM_MAX),
            ("net/ipv4/tcp_wmem", TCP_WMEM_MAX),
        ] {
            let path = sysctl(name);
            let now = std::fs::read_to_string(&path).map_err(|e| err(format!("{path}: {e}")))?;
            let mut values: Vec<u64> = now
                .split_whitespace()
                .map(str::parse)
                .collect::<Result<_, _>>()
                .map_err(|e| err(format!("{path}: {e}")))?;
            if let Some(max) = values.last_mut() {
                *max = scaled(*max, to, from).min(cap).max(*max);
            }
            let line: Vec<String> = values.iter().map(u64::to_string).collect();
            std::fs::write(&path, line.join(" ")).map_err(|e| err(format!("{path}: {e}")))?;
        }
        let path = sysctl("fs/file-max");
        let files = number(&path).ok_or_else(|| err(format!("{path}: not a number")))?;
        let files = files.saturating_add(to.saturating_sub(from) / FILE_BYTES);
        std::fs::write(&path, files.to_string()).map_err(|e| err(format!("{path}: {e}")))?;
        let mib = to.div_ceil(1 << 20);
        let buckets = (mib << 3).next_power_of_two().min(TCP_TABLE_MOST);
        for name in TCP_TABLE {
            let path = sysctl(name);
            let now = number(&path).ok_or_else(|| err(format!("{path}: not a number")))?;
            std::fs::write(&path, (buckets / 2).max(now).to_string())
                .map_err(|e| err(format!("{path}: {e}")))?;
        }
        let epoll =
            number(&sysctl("fs/epoll/max_user_watches")).ok_or_else(|| err("fs.epoll.max_user_watches"))?;
        let watches =
            scaled(epoll, EP_ITEM_COST, 4 * INOTIFY_WATCH_COST).clamp(INOTIFY_WATCHES.0, INOTIFY_WATCHES.1);
        std::fs::write(sysctl("fs/inotify/max_user_watches"), watches.to_string())
            .map_err(|e| err(format!("fs.inotify.max_user_watches: {e}")))?;
        let threads = number(&sysctl("kernel/threads-max")).ok_or_else(|| err("kernel.threads-max"))?;
        let half = threads / 2;
        for resource in [libc::RLIMIT_NPROC, libc::RLIMIT_SIGPENDING] {
            let limit = libc::rlimit {
                rlim_cur: half,
                rlim_max: half,
            };
            // SAFETY: setrlimit(2) of PID 1's own limit; it holds CAP_SYS_RESOURCE.
            if unsafe { libc::setrlimit(resource, &limit) } != 0 {
                return Err(err(format!(
                    "raising a limit to {half}: {}",
                    io::Error::last_os_error()
                )));
            }
        }
        Ok(())
    }

    /// Holds the build at the memory the builder has: mounts the builder's cgroup2 (D133),
    /// makes the build's cgroup with its `memory.high`, starts the process that grows it
    /// in the root cgroup, which nothing slows, and moves init into the build's.
    pub fn hold(&self) -> io::Result<()> {
        mount_cgroups()?;
        let enable = |dir: &str| {
            std::fs::write(format!("{dir}/cgroup.subtree_control"), "+memory +cpu +cpuset")
                .map_err(|e| err(format!("{dir}: enabling controllers: {e}")))
        };
        enable("/sys/fs/cgroup")?;
        for dir in [
            BUILD.to_string(),
            format!("{BUILD}/init"),
            format!("{BUILD}/steps"),
        ] {
            match std::fs::create_dir(&dir) {
                Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(err(format!("{dir}: {e}"))),
                _ => {}
            }
        }
        enable(BUILD)?;
        enable(&format!("{BUILD}/steps"))?;
        set_high(high(self.most, 0)?)?;
        let events = watch_events()?;
        // SAFETY: fork(2) of PID 1 before it starts any thread: the child runs on a copy
        // of a process with no other thread, whose locks no one holds.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(err(format!(
                "starting the memory's grower: {}",
                io::Error::last_os_error()
            )));
        }
        if pid == 0 {
            grow(*self, &events);
        }
        drop(events);
        std::fs::write(format!("{BUILD}/init/cgroup.procs"), "1")
            .map_err(|e| err(format!("moving init into the build's cgroup: {e}")))
    }
}

/// The builder's cgroup2 at /sys/fs/cgroup, mounted once: by [`Plug::hold`], or by the
/// first step with limits.
pub fn mount_cgroups() -> io::Result<()> {
    let root = std::path::Path::new("/sys/fs/cgroup");
    if root.join("cgroup.controllers").exists() {
        return Ok(());
    }
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
    Ok(())
}

/// What the kernel keeps beside the build's `memory.high`: [`SLACK`], and the page
/// structures of the memory plugged next, a 64th of it (a 64-byte `struct page` for each
/// 4 KiB page), which the kernel takes from what it has before the memory comes online.
/// Growth asks for as much as the builder has ([`grow`]), so that is a 64th of what it
/// has, until it has all it may.
fn reserve(total: u64, left: u64) -> u64 {
    SLACK.saturating_add(total.min(left) / 64)
}

/// The build's `memory.high`: what it holds now and what is free, less the [`reserve`]
/// for what may still be plugged, `left` of it. Its page cache counts as its own, so it is
/// not free; nothing else the kernel keeps is the build's.
fn high(most: u64, plugged: u64) -> io::Result<u64> {
    let current = number(&format!("{BUILD}/memory.current")).unwrap_or(0);
    let (total, free) = memory()?;
    Ok(current
        .saturating_add(free)
        .saturating_sub(reserve(total, most.saturating_sub(plugged))))
}

fn set_high(high: u64) -> io::Result<()> {
    std::fs::write(format!("{BUILD}/memory.high"), high.to_string())
        .map_err(|e| err(format!("{BUILD}/memory.high: {e}")))
}

/// An inotify watch of the build's `memory.events`, which the kernel marks modified as
/// it slows the build past its `memory.high` ("Conventions", cgroup-v2.rst).
fn watch_events() -> io::Result<OwnedFd> {
    // SAFETY: inotify_init1(2), whose descriptor is owned below.
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(err(format!(
            "watching the build's memory: {}",
            io::Error::last_os_error()
        )));
    }
    // SAFETY: the descriptor inotify_init1 just made, owned here alone.
    let events = unsafe { OwnedFd::from_raw_fd(fd) };
    let path = CString::new(format!("{BUILD}/memory.events")).map_err(err)?;
    // SAFETY: inotify_add_watch(2) of a NUL-terminated path.
    if unsafe { libc::inotify_add_watch(events.as_raw_fd(), path.as_ptr(), libc::IN_MODIFY) } < 0 {
        return Err(err(format!(
            "watching the build's memory: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(events)
}

/// Says what went wrong on the builder's console, as init says its own.
fn say(what: &str) {
    let _ = writeln!(io::stderr(), "shards-init: the builder's memory: {what}");
}

/// The process that grows the builder's memory, in the root cgroup: each time the build
/// reaches its `memory.high`, it asks for as much again as the builder has, whole memory
/// blocks, up to what it may have (growth by doubling, which keeps what is plugged ahead
/// of the build at most what it holds, and the times it is asked for to the logarithm),
/// and raises `memory.high` as each block comes online. With all of it plugged, the
/// build's `memory.high` goes: it is held only where the builder's memory ends.
fn grow(plug: Plug, events: &OwnedFd) -> ! {
    let mut asked = 0u64;
    let mut buf = [0u8; 4096];
    loop {
        // SAFETY: read(2) into a buffer this process owns; it blocks for the next event.
        let n = unsafe { libc::read(events.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            say(&format!("its watch failed: {}", io::Error::last_os_error()));
            // SAFETY: _exit(2) of this child alone.
            unsafe { libc::_exit(1) };
        }
        let (Some(current), Some(high_now)) = (
            number(&format!("{BUILD}/memory.current")),
            number(&format!("{BUILD}/memory.high")),
        ) else {
            continue;
        };
        if current < high_now || asked >= plug.most {
            continue;
        }
        let Ok(total) = mem_total() else { continue };
        asked = total
            .saturating_mul(2)
            .saturating_sub(plug.boot)
            .checked_next_multiple_of(ALIGN)
            .unwrap_or(u64::MAX)
            .max(asked)
            .min(plug.most);
        if let Err(e) = control_write64(control::HOTPLUG_REQUEST, asked) {
            say(&format!("asking for {asked} bytes: {e}"));
            continue;
        }
        let want = plug.boot.saturating_add(asked);
        let (mut last, mut since) = (total, Instant::now());
        while let Ok(now) = mem_total() {
            if now > last {
                (last, since) = (now, Instant::now());
                let plugged = now.saturating_sub(plug.boot);
                if let Err(e) = high(plug.most, plugged).and_then(set_high) {
                    say(&e.to_string());
                }
            }
            if now >= want {
                break;
            }
            if since.elapsed() > STALLED {
                say(&format!(
                    "{} bytes asked for, {} came",
                    asked,
                    now.saturating_sub(plug.boot)
                ));
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        if asked >= plug.most
            && last >= want
            && let Err(e) = std::fs::write(format!("{BUILD}/memory.high"), "max")
        {
            say(&format!("{BUILD}/memory.high: {e}"));
        }
    }
}
