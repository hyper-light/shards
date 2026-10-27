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
//!   it as the memory comes online ([`grow`]); and, where memory asked for stops coming,
//!   to ask again, and then to let the build run on what the builder has, never held at a
//!   `memory.high` that nothing will raise (D137).

use std::ffi::CString;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
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
/// At boot, before any snapshot: a guest reports the memory it holds free to its microVM
/// (D151) only where its kernel treats what a pass of reporting holds as free memory
/// (`page_reporting.held`, resources/kernel/patches/0006). One that does not counts up to
/// 128 MiB gone for as long as a pass holds it: readings of its free memory dip by as much,
/// and an allocation that needs it fails, and a task is ended (PM M224). There, each device
/// of the virtio-balloon driver is let go of, through a sysfs of init's own, and the guest
/// keeps what it frees.
pub fn settle_reporting() {
    const AT: &str = "/reporting";
    if std::fs::create_dir_all(AT).is_err() {
        return;
    }
    let flags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    // SAFETY: mount(2) of NUL-terminated literals.
    if unsafe {
        libc::mount(
            c"sysfs".as_ptr(),
            c"/reporting".as_ptr(),
            c"sysfs".as_ptr(),
            flags,
            std::ptr::null(),
        )
    } == 0
    {
        if !std::path::Path::new("/reporting/module/page_reporting/parameters/held").exists() {
            let_go_of_balloons("/reporting/bus/virtio/drivers/virtio_balloon");
        }
        // SAFETY: umount2(2) of the mount above.
        unsafe { libc::umount2(c"/reporting".as_ptr(), libc::MNT_DETACH) };
    }
    let _ = std::fs::remove_dir(AT);
}

/// Unbinds each device bound to the driver at `driver` (sysfs's `unbind`), saying any it
/// cannot.
fn let_go_of_balloons(driver: &str) {
    let Ok(entries) = std::fs::read_dir(driver) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.as_encoded_bytes().starts_with(b"virtio") {
            continue;
        }
        if let Err(e) = std::fs::write(format!("{driver}/unbind"), name.as_encoded_bytes()) {
            let _ = writeln!(
                io::stderr(),
                "shards-init: letting go of {}: {e}",
                name.to_string_lossy()
            );
        }
    }
}

/// How long the memory asked for may take to come online, from the last of it that came,
/// before the process that grows it asks again: a thousand times the 5-7 ms 1 GiB took
/// (PM M193). Asked again, the device tells its driver again, which then tries what it
/// could not plug at once, not at its retry timer's 50 s (D137).
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

/// The memory a builder, or a run's microVM (D148), may have plugged, and what it booted
/// with.
#[derive(Debug, Clone, Copy)]
pub struct Plug {
    /// MemTotal as it booted.
    boot: u64,
    /// The most it may have plugged: a builder's, or a run's microVM's own, which the host
    /// sets as the microVM takes its run, and which its grower reads again as it grows
    /// (D148).
    most: u64,
    /// The cgroup it holds: the build's, or every process of a run's microVM past init.
    cgroup: &'static str,
    /// Whose memory its notes say it is.
    whose: &'static str,
}

fn err(what: impl std::fmt::Display) -> io::Error {
    io::Error::other(what.to_string())
}

/// The builder's memory to plug, where its VM has a device for it.
pub fn plug() -> io::Result<Option<Plug>> {
    plug_of(BUILD, "the builder's")
}

/// A run's microVM's memory to plug, where its VM has a device for it (D148): its
/// processes past init held in `cgroup`.
pub fn run_plug(cgroup: &'static str) -> io::Result<Option<Plug>> {
    plug_of(cgroup, "the microVM's")
}

/// The kernel's log, opened where init's /dev still has it, before its root's own /dev
/// holds a container's devices alone: where a run's grower says what it sees (D148).
pub fn kmsg() -> io::Result<OwnedFd> {
    std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/kmsg")
        .map(OwnedFd::from)
        .map_err(|e| err(format!("/dev/kmsg: {e}")))
}

/// MemTotal as the VM booted, none of its memory plugged (D148): a builder's, or a run's
/// microVM's, kept in a template, as its restores boot from it.
static BOOT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// The memory the VM may still have plugged, in bytes: a builder's most, or a run's own
/// (D148), less what is plugged already; none where it plugs none.
pub fn still_pluggable() -> u64 {
    let Some(&boot) = BOOT.get() else {
        return 0;
    };
    let Ok(most) = control_read64(control::HOTPLUG_SIZE) else {
        return 0;
    };
    most.saturating_sub(mem_total().map_or(0, |total| total.saturating_sub(boot)))
}

/// [`Plug::tmpfs`] of a tmpfs made as the microVM runs, a joiner's writable layer (D119):
/// of all the memory it may have now, its run's own most; none where it plugs none, whose
/// tmpfs is sized by the memory it has, all of it.
pub fn tmpfs_now() -> Option<String> {
    let boot = *BOOT.get()?;
    let most = control_read64(control::HOTPLUG_SIZE)
        .ok()
        .filter(|&most| most > 0)?;
    Some(tmpfs_holding(boot.saturating_add(most)))
}

/// The options of a tmpfs that holds `full` bytes, and the inodes a tmpfs has by default
/// in that much memory.
fn tmpfs_holding(full: u64) -> String {
    format!("size={full},nr_inodes={}", full / page_size() / 2)
}

fn plug_of(cgroup: &'static str, whose: &'static str) -> io::Result<Option<Plug>> {
    let most = control_read64(control::HOTPLUG_SIZE)?;
    if most == 0 {
        return Ok(None);
    }
    let boot = mem_total()?;
    let _ = BOOT.set(boot);
    Ok(Some(Plug {
        boot,
        most,
        cgroup,
        whose,
    }))
}

/// The kernel's memory and what of it is free, in bytes: /proc/meminfo's MemTotal and
/// MemFree, from sysinfo(2), which both come from (mm/show_mem.c `si_meminfo`), without
/// a file: init asks before it has mounted /proc. Memory plugged counts as it comes online;
/// memory still to be plugged, which the kernel reports as its own (D149), does not.
fn memory() -> io::Result<(u64, u64)> {
    let [total, free] = unreported(|| {
        // SAFETY: a zeroed sysinfo is a valid out-parameter, which sysinfo(2) fills.
        let mut info: libc::sysinfo = unsafe { std::mem::zeroed() };
        // SAFETY: sysinfo(2) writes `info`.
        if unsafe { libc::sysinfo(&mut info) } != 0 {
            return Err(err(format!("sysinfo: {}", io::Error::last_os_error())));
        }
        let unit = u64::from(info.mem_unit.max(1));
        Ok([
            info.totalram.saturating_mul(unit),
            info.freeram.saturating_mul(unit),
        ])
    })?;
    Ok((total, free))
}

/// What `read` reads of the kernel's memory, each amount less what the kernel adds to it
/// of the memory still to be plugged (D149): its report, read before and after. The
/// report changes only as memory is plugged, or unplugged, one way at a time, so the same
/// report before and after is the one `read` saw; where memory came between, it reads
/// again.
pub fn unreported<const N: usize>(read: impl FnMut() -> io::Result<[u64; N]>) -> io::Result<[u64; N]> {
    unreported_by(reported, read)
}

/// [`unreported`], the report read by `report`.
fn unreported_by<const N: usize>(
    mut report: impl FnMut() -> io::Result<u64>,
    mut read: impl FnMut() -> io::Result<[u64; N]>,
) -> io::Result<[u64; N]> {
    loop {
        let before = report()?;
        let amounts = read()?;
        if report()? == before {
            return Ok(amounts.map(|a| a.saturating_sub(before)));
        }
    }
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
        tmpfs_holding(self.full())
    }

    /// Raises what the kernel sized by the memory it booted with to what it sizes by all
    /// of it: each limit in proportion to the memory scaled by their ratio; files by the
    /// memory added; TCP's buffers within their caps; TCP's orphans and TIME-WAIT buckets
    /// by its table's sizing; inotify's watches, which the kernel holds at 8192 below
    /// about 1 GiB, in their proportion to epoll's items, within their clamps; and init's
    /// RLIMIT_SIGPENDING, half the threads, as the kernel gives PID 1 (`fork_init`), which
    /// every step inherits. Its processes are Docker's, unlimited (`defaults::limits`,
    /// PM M211), not the kernel's.
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
        let limit = libc::rlimit {
            rlim_cur: half,
            rlim_max: half,
        };
        // SAFETY: setrlimit(2) of PID 1's own limit; it holds CAP_SYS_RESOURCE.
        if unsafe { libc::setrlimit(libc::RLIMIT_SIGPENDING, &limit) } != 0 {
            return Err(err(format!(
                "raising a limit to {half}: {}",
                io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    /// Holds the build at the memory the builder has: mounts the builder's cgroup2 (D133),
    /// makes the build's cgroup with its `memory.high`, starts the process that grows it
    /// in the root cgroup, which nothing slows, and moves init into the build's. What the
    /// grower says comes out of the descriptor returned, a line at a time (D137).
    pub fn hold(&self) -> io::Result<OwnedFd> {
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
        // Its peak, counted afresh by each giving back through this descriptor
        // ([`shrink_run`], [`answer_give_backs`]), as a run's; none where it does not open.
        if let Ok(peak) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{BUILD}/memory.peak"))
        {
            let _ = PEAK.set(peak);
        }
        set_high(self.cgroup, high(self.cgroup, self.most, 0)?)?;
        let events = watch_events(self.cgroup)?;
        let mut fds = [0; 2];
        // SAFETY: pipe2(2) into an array of two descriptors.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(err(format!(
                "the memory's grower's notes: {}",
                io::Error::last_os_error()
            )));
        }
        // SAFETY: the two descriptors pipe2 just made, owned here alone.
        let (notes, said) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
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
            drop(notes);
            grow(*self, &events, &said);
        }
        drop((events, said));
        std::fs::write(format!("{BUILD}/init/cgroup.procs"), "1")
            .map_err(|e| err(format!("moving init into the build's cgroup: {e}")))?;
        Ok(notes)
    }

    /// Holds a run's microVM at the memory it has (D148): every process past init, in its
    /// cgroup, made already, at a `memory.high` of the memory plugged so far, and a process
    /// of init's in the root cgroup, which nothing slows, that grows it as they reach it,
    /// up to the run's own most, which the host sets as the microVM takes its run. What it
    /// sees it says into `notes`, the kernel's log ([`kmsg`]), beside the driver's own
    /// words. Called by PID 1 before it starts any thread; a template's grower goes on in
    /// each of its restores.
    pub fn hold_run(&self, notes: OwnedFd) -> io::Result<()> {
        // Its peak, counted afresh by each giving back through this descriptor
        // ([`shrink_run`]), which its builtins' forks share; none where it does not open.
        if let Ok(peak) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/memory.peak", self.cgroup))
        {
            let _ = PEAK.set(peak);
        }
        set_high(self.cgroup, high(self.cgroup, self.most, 0)?)?;
        let events = watch_events(self.cgroup)?;
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
            grow(*self, &events, &notes);
        }
        let _ = GROWER.set(pid);
        Ok(())
    }
}

/// The most a run's cgroup may hold that its host's budget leaves its microVM (D188), in
/// bytes: no hold of its memory passes it ([`set_high`]), and its grower lifts none past
/// it, so that its own kernel drops its run's page cache before its host must take memory
/// the guest holds, which with no swap is its own RAM, a template's file pages (PM M309);
/// `u64::MAX` where it has none. In a page shared by init, its grower and the forks its
/// builtins run in (`MAP_SHARED`, made as init starts, [`make_cap`]),
/// so that what the host says to a builtin holds the grower too; and in a template, which
/// its restores resume with none.
static CAP: std::sync::OnceLock<&'static std::sync::atomic::AtomicU64> = std::sync::OnceLock::new();

/// Makes [`CAP`], none yet: by PID 1 as it starts, before it forks anything, a grower or a
/// builtin's fork, each of which shares it.
pub fn make_cap() -> io::Result<()> {
    if CAP.get().is_some() {
        return Ok(());
    }
    let len = usize::try_from(page_size()).map_err(err)?;
    // SAFETY: mmap(2) of a new shared anonymous mapping, which nothing references yet.
    let page = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if page == libc::MAP_FAILED {
        return Err(err(format!(
            "the run's budget's page: {}",
            io::Error::last_os_error()
        )));
    }
    // SAFETY: the page just mapped, zeroed, aligned to a page and never unmapped: an
    // AtomicU64 at its start, a valid one in its zeroes, lives as long as the process,
    // and every process forked from it shares it.
    let cap = unsafe { &*page.cast::<std::sync::atomic::AtomicU64>() };
    cap.store(u64::MAX, std::sync::atomic::Ordering::SeqCst);
    let _ = CAP.set(cap);
    Ok(())
}

/// [`CAP`]'s bytes: `u64::MAX` where it has none.
fn cap() -> u64 {
    CAP.get()
        .map_or(u64::MAX, |cap| cap.load(std::sync::atomic::Ordering::SeqCst))
}

/// The run's `memory.high` as its grower holds it ([`Plug::hold_run`]), what it uses and
/// what is free, less the page structures of what may still be plugged, with no budget:
/// none (`u64::MAX`) where all it may have is plugged, or it plugs none.
fn grown_hold(cgroup: &str) -> io::Result<u64> {
    let Some(&boot) = BOOT.get() else {
        return Ok(u64::MAX);
    };
    let most = most_within(boot, control_read64(control::HOTPLUG_SIZE)?, cgroup);
    let plugged = staying(boot, mem_total()?);
    if plugged >= most {
        return Ok(u64::MAX);
    }
    high(cgroup, most, plugged)
}

/// Holds the run in `cgroup` within what its host's budget leaves it (D188, the builtin
/// `ROOM`): `room` bytes past what it uses now, and never less past it than the room a run
/// needs to go on ([`working_room`]), which a room short of it raises no hold past (what
/// it holds past the room is given back by the host's passes, `RECLAIM`, whose pages the
/// kernel reports free together); or with no budget (`None`), as its memory alone holds
/// it. What the hold leaves above it the kernel reclaims at once, which is then reported
/// free (D151). The bytes it is held within past what it uses; `None` with no budget.
pub fn hold_within(cgroup: &str, room: Option<i64>) -> io::Result<Option<u64>> {
    let used = number(&format!("{cgroup}/memory.current"))
        .ok_or_else(|| err(format!("{cgroup}/memory.current unread")))?;
    set_cap(room.map_or(u64::MAX, |r| within(used, r, working_room(), cap())));
    set_high(cgroup, grown_hold(cgroup)?)?;
    crate::run::flush_page_reports();
    Ok(room.map(|_| cap().saturating_sub(used)))
}

/// The least room a run is held within past what it uses, however short its budget (D188),
/// in bytes: what its kernel charges it ahead of the pages it takes, a batch of 64 pages
/// for each CPU (`MEMCG_CHARGE_BATCH`, mm/memcontrol.c `refill_stock`), which no reclaim
/// takes back; the pages a read holds as its file is read ahead of it, two windows of 128
/// KiB (`VM_READAHEAD_PAGES`, mm/readahead.c: the window read and the one read ahead of
/// it, a share's own, as its server keeps the window the kernel offers); a reader's block
/// as large as one request of a share carries ([`shards_abi::SHARE_MAX_PAGES`]), which
/// with no swap no reclaim takes either; and the page cache that request fills on its way
/// to the block, as many pages. Held within nothing past what it used, a run's `dd` of 1
/// MiB blocks through FUSE took 233 s for 32 MiB it read in 0.28 s with no budget, asleep
/// in the kernel's throttle of a cgroup past its `memory.high`
/// (`__mem_cgroup_handle_over_high`), its kernel's reclaim taking 3% of what it scanned;
/// within 1 MiB, 3.9 to 201 s, and within 1.25 MiB on 4 vCPUs 0.8 to 7.6 s (PM M309).
/// Held within all but the request's page cache, a gigabyte's read took its reads' own
/// pages back and read them again, 2,899 times at the median and 16,327 at p90 on CI's EPYC
/// 7763s, and three reads in 144 took over twice their CPU's time with none, the slowest
/// 2.77 times; with it, 1 and 687 times, the slowest 1.12 times (PM M321).
fn working_room() -> u64 {
    // SAFETY: sysconf(3) reads a value of the running kernel's.
    let cpus = u64::try_from(unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }).map_or(1, |c| c.max(1));
    working_room_of(cpus, page_size())
}

/// [`working_room`] of a guest with `cpus` CPUs online and pages of `page` bytes.
fn working_room_of(cpus: u64, page: u64) -> u64 {
    const CHARGE_BATCH: u64 = 64;
    const READ_AHEAD: u64 = 128 << 10;
    CHARGE_BATCH
        .saturating_mul(cpus)
        .saturating_add(2 * u64::from(shards_abi::SHARE_MAX_PAGES))
        .saturating_mul(page)
        .saturating_add(2 * READ_AHEAD)
}

/// The most a run's microVM may have plugged within what its host's budget leaves it
/// ([`CAP`]): what lets its run hold its cap beside what its kernel holds now and its
/// [`SLACK`], with the page structures of what is plugged, a 64th of it, which the kernel
/// takes from its own memory, in whole blocks; `most` with no budget. Its grower doubling
/// what it asked for past this, as a give-back's hold below the cap reached it, plugged its
/// 16 GiB, whose page structures alone took 256 MiB of a 384 MiB budget (PM M309).
fn most_within(boot: u64, most: u64, cgroup: &str) -> u64 {
    let cap = cap();
    if cap == u64::MAX {
        return most;
    }
    let Some(used) = number(&format!("{cgroup}/memory.current")) else {
        return most;
    };
    let Ok((total, free)) = memory() else {
        return most;
    };
    let kernel = total.saturating_sub(free).saturating_sub(used);
    plug_for(boot, most, cap, kernel)
}

/// [`most_within`] of a run held at `cap` bytes, its kernel holding `kernel` beside it.
fn plug_for(boot: u64, most: u64, cap: u64, kernel: u64) -> u64 {
    let want = cap.saturating_add(kernel).saturating_add(SLACK);
    want.saturating_add(want / 63)
        .saturating_sub(boot)
        .checked_next_multiple_of(ALIGN)
        .unwrap_or(u64::MAX)
        .min(most)
}

/// What a run that uses `used` bytes is held at within the `room` its host's budget leaves
/// it: that room past what it uses, where it is `least` or more, the room a run needs to go
/// on ([`working_room`]); else `least` past what it uses, but no higher than it was held
/// (`was`), so that a budget short of room for it holds it within what it had, and each
/// word of it, told again each window of the host's pressure while it is short, raises it
/// by none.
fn within(used: u64, room: i64, least: u64, was: u64) -> u64 {
    let room = room.max(0).unsigned_abs();
    if room >= least {
        used.saturating_add(room)
    } else {
        used.saturating_add(least).min(was)
    }
}

/// Sets [`CAP`], where it was made, in whole pages, as the kernel keeps `memory.high`
/// (mm/memcontrol.c, `page_counter_memparse`): a cap past a page boundary read back short
/// of itself, and its grower took a run held at its cap for one held short of it, writing
/// its hold again at each of its run's events, each a reclaim the kernel makes at once.
fn set_cap(bytes: u64) {
    if let Some(cap) = CAP.get() {
        cap.store(in_pages(bytes, page_size()), std::sync::atomic::Ordering::SeqCst);
    }
}

/// `bytes` in whole pages of `page` bytes, rounded down: none for none.
fn in_pages(bytes: u64, page: u64) -> u64 {
    if bytes == u64::MAX {
        return bytes;
    }
    bytes - bytes % page.max(1)
}

/// The run's `memory.peak`, opened by PID 1 as it holds the run ([`Plug::hold_run`]) and
/// shared by its builtins' forks: written, the kernel counts its peak afresh for this open
/// file alone (Linux 6.12, c6f53ed8f213), so that each giving back reads the run's peak
/// since the one before ([`shrink_run`]).
static PEAK: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();

/// The run's peak since it was last read here, `used` at least, counted afresh from now:
/// its peak since it began where the kernel counts no open file's own, or [`PEAK`] is not
/// open.
fn peak_since(used: u64) -> u64 {
    use std::os::unix::fs::FileExt as _;
    let Some(file) = PEAK.get() else {
        return used;
    };
    let mut buf = [0u8; 32];
    let peak = file
        .read_at(&mut buf, 0)
        .ok()
        .and_then(|n| {
            std::str::from_utf8(buf.get(..n)?)
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()
        })
        .unwrap_or(used);
    let _ = file.write_at(b"reset", 0);
    peak.max(used)
}

/// What a run keeps plugged past what its microVM booted with as its host asks for memory
/// back (D188), from what its microVM booted with (`boot`), the most it may have plugged,
/// what stays of what it has plugged ([`staying`]) and has asked to have, what it uses and
/// what it used at its peak
/// since it was last asked: twice what it uses, as the grower asks for twice what it
/// reaches (D148), never less than its peak, in whole memory blocks. None where it gives
/// back nothing: less than a block, or while memory it asked for to grow is still to come,
/// which the giving back would take away from it as it grows (PM M275).
fn to_keep(boot: u64, most: u64, plugged: u64, asked: u64, used: u64, peak: u64) -> Option<u64> {
    if asked.saturating_sub(plugged) >= ALIGN {
        return None;
    }
    let keep = used
        .saturating_mul(2)
        .max(peak)
        .saturating_sub(boot)
        .checked_next_multiple_of(ALIGN)
        .unwrap_or(u64::MAX)
        .min(most);
    (plugged.saturating_sub(keep) >= ALIGN).then_some(keep)
}

/// Gives back what the run holds plugged past what it keeps ([`to_keep`], D188). The run is
/// held at what it will have first, so that the grower hears it reach that and plugs again
/// however much the driver unplugs, which it does as it can: a block the kernel holds part
/// of stays. The bytes asked to be unplugged; none where it keeps all it has.
pub fn shrink_run(cgroup: &str) -> io::Result<u64> {
    let Some(&boot) = BOOT.get() else {
        return Ok(0);
    };
    // No more than its budget's room lets its run hold (D188).
    let most = most_within(boot, control_read64(control::HOTPLUG_SIZE)?, cgroup);
    let total = mem_total()?;
    let plugged = total.saturating_sub(boot);
    let asked = control_read64(control::HOTPLUG_REQUEST).unwrap_or(plugged);
    let used = number(&format!("{cgroup}/memory.current"))
        .ok_or_else(|| err(format!("{cgroup}/memory.current unread")))?;
    // Of what stays: what a give-back before asked its driver to unplug goes already.
    let stays = staying(boot, total);
    let Some(keep) = to_keep(boot, most, stays, asked, used, peak_since(used)) else {
        return Ok(0);
    };
    // The free memory that goes with it counted out of the hold now, before it goes, as
    // every hold counts out what goes already ([`high`]).
    set_high(
        cgroup,
        high(cgroup, most, keep)?.saturating_sub(stays.saturating_sub(keep)),
    )?;
    control_write64(control::HOTPLUG_REQUEST, keep)?;
    Ok(plugged.saturating_sub(keep))
}

/// Answers its host's asks for what the builder holds cold back, on `conn`
/// (shards_abi::build::GIVE_PORT, D188), from a process of PID 1's in the root cgroup,
/// which no `memory.high` slows, as the grower is: the host asks as it runs short, when the
/// build may be held at its own. At each `GIVE_BACK`, what the build's cgroup holds cold of
/// its files reclaimed and reported free at once, then what the builder holds plugged past
/// what it keeps asked to be unplugged ([`shrink_run`]), as a run's init answers `RECLAIM`;
/// else why it gave nothing back. It ends as its connection does. Called by PID 1 before it
/// starts any thread.
pub fn answer_give_backs(conn: std::fs::File) -> io::Result<()> {
    // SAFETY: fork(2) of PID 1 before it starts any thread: the child runs on a copy of a
    // process with no other thread, whose locks no one holds.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(err(format!(
            "starting the give-back's answerer: {}",
            io::Error::last_os_error()
        )));
    }
    if pid == 0 {
        // Out of the build's cgroup, where PID 1 is: the root's ("0", the writer itself).
        let _ = std::fs::write("/sys/fs/cgroup/cgroup.procs", "0");
        answer(
            &conn,
            || crate::run::reclaim_cold(BUILD).and_then(|freed| Ok((freed, shrink_run(BUILD)?))),
            |room| hold_within(BUILD, room),
        );
        // SAFETY: _exit(2) ends this process alone.
        unsafe { libc::_exit(0) };
    }
    Ok(())
}

/// The most a frame on the give-back connection holds: a `GIVE_BACK` holds nothing and a
/// `ROOM` a number, and a frame past this ends the connection rather than have its answerer
/// hold it.
const GIVE_FRAME: usize = 4096;

/// What [`answer_give_backs`]'s process does until its connection ends: each frame read
/// whole; each `GIVE_BACK` answered with a `GAVE` of what `give` gave back (the bytes its
/// files freed and those it asked to unplug), or of why nothing; each `ROOM` with a `HELD`
/// of what `hold` held the build within past what it uses (nothing with no budget, its
/// room's payload empty), or of why not; any other frame passed over.
fn answer(
    conn: &std::fs::File,
    mut give: impl FnMut() -> io::Result<(u64, u64)>,
    mut hold: impl FnMut(Option<i64>) -> io::Result<Option<u64>>,
) {
    use shards_abi::{build::kind, run};
    let mut h = [0u8; run::HEADER];
    let mut payload = Vec::new();
    loop {
        let mut r = conn;
        if r.read_exact(&mut h).is_err() {
            return;
        }
        let Some((which, len)) = run::parse_header(h) else {
            return;
        };
        let Some(len) = usize::try_from(len).ok().filter(|&len| len <= GIVE_FRAME) else {
            return;
        };
        payload.resize(len, 0);
        if r.read_exact(&mut payload).is_err() {
            return;
        }
        let (answers, said) = match which {
            kind::GIVE_BACK => (
                kind::GAVE,
                match give() {
                    Ok((freed, unplugging)) => format!("{freed} {unplugging}"),
                    Err(e) => format!("nothing given back: {e}"),
                },
            ),
            kind::ROOM => {
                let room = match std::str::from_utf8(&payload).map(str::trim) {
                    Ok("") => Ok(None),
                    Ok(text) => text
                        .parse::<i64>()
                        .map(Some)
                        .map_err(|_| err(format!("a room that is no number of bytes: {text:?}"))),
                    Err(_) => Err(err("a room that is no text")),
                };
                (
                    kind::HELD,
                    match room.and_then(&mut hold) {
                        Ok(Some(held)) => held.to_string(),
                        Ok(None) => String::new(),
                        Err(e) => format!("not held: {e}"),
                    },
                )
            }
            _ => continue,
        };
        if crate::run::send(conn, answers, said.as_bytes()).is_err() {
            return;
        }
    }
}

/// Holds a run's processes in `cgroup` at what is plugged so far again, as [`Plug::hold_run`]
/// held them (D148), where the host has raised its most since (`update -m`, its `PLUG`):
/// its grower lifted the hold once all its old most was plugged, and is woken only as its
/// processes reach a hold. A most no larger than what is plugged leaves the hold as it is,
/// the run's `memory.max` holding it.
pub fn rehold_run(cgroup: &'static str) -> io::Result<()> {
    let Some(&boot) = BOOT.get() else {
        return Ok(());
    };
    let most = most_within(boot, control_read64(control::HOTPLUG_SIZE)?, cgroup);
    let plugged = staying(boot, mem_total()?);
    if plugged >= most {
        return Ok(());
    }
    set_high(cgroup, high(cgroup, most, plugged)?)
}

/// The guest kernel's report of the memory the host may still plug (D149):
/// `vm.pluggable_kbytes`, which shards' kernel adds to the total, free and available memory
/// sysinfo(2) and /proc/meminfo report, and to what its overcommit heuristic counts.
/// Written, the memory the host may plug from then on; read, what of it is still to be
/// plugged, which the kernel lowers as memory is plugged (resources/kernel/patches/0004).
/// Opened by init before /proc/sys is masked, where the kernel has it, and kept: a
/// template's restores write through it, and its grower reads it.
static PLUGGABLE: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();

/// Opens [`PLUGGABLE`], where the kernel has it: one without reports what it has alone.
pub fn open_pluggable() {
    if let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/proc/sys/vm/pluggable_kbytes")
    {
        let _ = PLUGGABLE.set(file);
    }
}

/// Tells the kernel what the host may still plug ([`still_pluggable`]), where it reports
/// it: at offset 0, as a sysctl's writes elsewhere are ignored (`sysctl_writes_strict`).
/// Once for each run, as it begins, before anything of it grows its memory: the kernel
/// lowers the report itself as memory is plugged.
pub fn report_pluggable() -> io::Result<()> {
    use std::os::unix::fs::FileExt as _;
    let Some(file) = PLUGGABLE.get() else {
        return Ok(());
    };
    file.write_at(format!("{}\n", still_pluggable() / 1024).as_bytes(), 0)
        .map(drop)
        .map_err(|e| err(format!("vm.pluggable_kbytes: {e}")))
}

/// What the kernel reports of the memory still to be plugged, in bytes: none where init has
/// not told it ([`report_pluggable`]).
fn reported() -> io::Result<u64> {
    use std::os::unix::fs::FileExt as _;
    let Some(file) = PLUGGABLE.get() else {
        return Ok(0);
    };
    let mut text = [0u8; 32];
    let n = file
        .read_at(&mut text, 0)
        .map_err(|e| err(format!("vm.pluggable_kbytes: {e}")))?;
    std::str::from_utf8(text.get(..n).unwrap_or_default())
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .map(|kib| kib.saturating_mul(1024))
        .ok_or_else(|| err("vm.pluggable_kbytes: not a number"))
}

/// A run's microVM's grower ([`Plug::hold_run`]): a process of init's, which the end of
/// the run's workload spares, as its joiners go on and may need memory (D148).
static GROWER: std::sync::OnceLock<libc::pid_t> = std::sync::OnceLock::new();

/// The pid of a run's microVM's grower, where it has one.
pub fn grower() -> Option<libc::pid_t> {
    GROWER.get().copied()
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

/// The build's `memory.high`: what it holds now and what is free and stays plugged, less
/// the [`reserve`] for what may still be plugged, `left` of it. Its page cache counts as
/// its own, so it is not free; nothing else the kernel keeps is the build's.
fn high(cgroup: &str, most: u64, plugged: u64) -> io::Result<u64> {
    let current = number(&format!("{cgroup}/memory.current")).unwrap_or(0);
    let (total, free) = memory()?;
    let leaving = BOOT.get().map_or(0, |&boot| {
        total.saturating_sub(boot).saturating_sub(staying(boot, total))
    });
    Ok(hold_of(
        current,
        free,
        leaving,
        reserve(total, most.saturating_sub(plugged)),
    ))
}

/// The hold of a run that uses `current` bytes, its kernel `free`, `leaving` of its memory
/// plugged its device's to unplug and the kernel's `reserve` beside it: what it uses and
/// what is free, but for what goes, free or not, and the reserve. A hold that counted
/// what goes as free, set as a give-back's memory was unplugged (the host's word of the
/// room its budget had after it), held a run past all its microVM would have once it had
/// gone: its page cache took the rest, its guest's kernel throttled its own reclaim at its
/// zones' least, and its grower, woken only at the hold, plugged none (PM M324).
fn hold_of(current: u64, free: u64, leaving: u64, reserve: u64) -> u64 {
    current
        .saturating_add(free)
        .saturating_sub(leaving)
        .saturating_sub(reserve)
}

/// What stays of the memory plugged past `boot` while the kernel has `total`: no more than
/// its device's request, which a give-back lowers (D188) and its driver unplugs down to as
/// it can; what is plugged past it goes.
fn staying(boot: u64, total: u64) -> u64 {
    let plugged = total.saturating_sub(boot);
    control_read64(control::HOTPLUG_REQUEST).map_or(plugged, |asked| plugged.min(asked))
}

/// Sets `cgroup`'s `memory.high` to `high`, never past what its host's budget leaves it
/// ([`CAP`]): `max` for no hold at all.
fn set_high(cgroup: &str, high: u64) -> io::Result<()> {
    let high = high.min(cap());
    let text = if high == u64::MAX {
        "max".to_string()
    } else {
        high.to_string()
    };
    std::fs::write(format!("{cgroup}/memory.high"), text)
        .map_err(|e| err(format!("{cgroup}/memory.high: {e}")))
}

/// An inotify watch of the build's `memory.events`, which the kernel marks modified as
/// it slows the build past its `memory.high` ("Conventions", cgroup-v2.rst).
fn watch_events(cgroup: &str) -> io::Result<OwnedFd> {
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
    let path = CString::new(format!("{cgroup}/memory.events")).map_err(err)?;
    // SAFETY: inotify_add_watch(2) of a NUL-terminated path.
    if unsafe { libc::inotify_add_watch(events.as_raw_fd(), path.as_ptr(), libc::IN_MODIFY) } < 0 {
        return Err(err(format!(
            "watching the build's memory: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(events)
}

/// Says `what` of the builder's memory, a line into `notes`, which init sends as the
/// output of the step that runs (D137); a line the pipe has no room for is said on the
/// builder's console instead, as nothing may block the grower.
/// A note of `whose` memory, as the grower and a step's runner say one.
pub(crate) fn note(whose: &str, what: &str) -> String {
    format!("shards-init: {whose} memory: {what}\n")
}

fn say(notes: &OwnedFd, whose: &str, what: &str) {
    let line = note(whose, what);
    // SAFETY: write(2) of a buffer of the length given, to a descriptor this process owns,
    // made non-blocking as the grower starts.
    let n = unsafe { libc::write(notes.as_raw_fd(), line.as_ptr().cast(), line.len()) };
    if usize::try_from(n).ok() != Some(line.len()) {
        let _ = io::stderr().write_all(line.as_bytes());
    }
}

/// What the guest kernel said of its virtio-mem driver, its last `most` lines (/dev/kmsg,
/// Documentation/ABI/testing/dev-kmsg): why memory asked for did not come, in its words.
fn kernel_said(most: usize) -> Vec<String> {
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/kmsg")
    else {
        return Vec::new();
    };
    let mut said: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut record = [0u8; 8192];
    // Each read is one record, `level,seq,usecs,flags;text`; past the last, EAGAIN.
    while let Ok(n) = (&file).read(&mut record) {
        let text = String::from_utf8_lossy(record.get(..n).unwrap_or_default());
        let Some((_, text)) = text.split_once(';') else {
            if n == 0 {
                break;
            }
            continue;
        };
        let text = text.lines().next().unwrap_or_default();
        if text.contains("virtio_mem") {
            said.push_back(text.to_string());
            if said.len() > most {
                said.pop_front();
            }
        }
    }
    said.into()
}

/// How memory asked for came.
#[derive(Debug, PartialEq, Eq)]
enum Came {
    /// All of it.
    All,
    /// Not all, though asked for twice: what of it came, in bytes.
    Stalled(u64),
}

/// What growing the builder's memory acts on: the guest's memory, the host's device, the
/// build's `memory.high`, time, and where it says what it saw. The builder's own
/// ([`Builder`]), or a test's.
trait Grows {
    /// MemTotal, memory plugged counted as it comes online.
    fn total(&mut self) -> io::Result<u64>;
    /// Asks the host for `bytes` past what the guest booted with.
    fn ask(&mut self, bytes: u64) -> io::Result<()>;
    /// `plugged` bytes are online: the build's `memory.high` raised to match.
    fn raise(&mut self, plugged: u64);
    fn say(&mut self, what: &str);
    /// The kernel's last words of its virtio-mem driver.
    fn kernel_said(&mut self) -> Vec<String>;
    /// How long since `from` began, from [`Grows::now`].
    fn now(&mut self) -> Duration;
    /// Waits a moment before looking again.
    fn pause(&mut self);
}

/// The builder's own memory, its device and its build's cgroup.
struct Builder<'n> {
    plug: Plug,
    notes: &'n OwnedFd,
    began: Instant,
}

impl Grows for Builder<'_> {
    fn total(&mut self) -> io::Result<u64> {
        mem_total()
    }

    fn ask(&mut self, bytes: u64) -> io::Result<()> {
        control_write64(control::HOTPLUG_REQUEST, bytes)
    }

    fn raise(&mut self, plugged: u64) {
        let cgroup = self.plug.cgroup;
        if let Err(e) = high(cgroup, self.plug.most, plugged).and_then(|h| set_high(cgroup, h)) {
            say(self.notes, self.plug.whose, &e.to_string());
        }
    }

    fn say(&mut self, what: &str) {
        say(self.notes, self.plug.whose, what);
    }

    fn kernel_said(&mut self) -> Vec<String> {
        kernel_said(3)
    }

    fn now(&mut self) -> Duration {
        self.began.elapsed()
    }

    fn pause(&mut self) {
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Asks for `asked` bytes past what the builder booted with, and raises the build's
/// `memory.high` as they come online. Where none comes for [`STALLED`], asks again, which
/// tells the driver again; where none comes for as long again, stops.
fn come(plug: Plug, asked: u64, g: &mut impl Grows) -> Came {
    let want = plug.boot.saturating_add(asked);
    let Ok(start) = g.total() else {
        return Came::Stalled(0);
    };
    let (mut last, mut since) = (start, g.now());
    let mut again = false;
    if let Err(e) = g.ask(asked) {
        g.say(&format!("asking for {asked} bytes: {e}"));
        return Came::Stalled(start.saturating_sub(plug.boot));
    }
    while let Ok(now) = g.total() {
        if now > last {
            (last, since) = (now, g.now());
            g.raise(now.saturating_sub(plug.boot));
        }
        if now >= want {
            return Came::All;
        }
        if g.now().saturating_sub(since) > STALLED {
            if again {
                break;
            }
            let came = now.saturating_sub(plug.boot);
            let kernel = g.kernel_said();
            g.say(&format!(
                "{asked} bytes asked for, {came} came in {} s; asking again{}{}",
                STALLED.as_secs(),
                if kernel.is_empty() {
                    ""
                } else {
                    "; the kernel said: "
                },
                kernel.join("; ")
            ));
            if let Err(e) = g.ask(asked) {
                g.say(&format!("asking for {asked} bytes again: {e}"));
                break;
            }
            (again, since) = (true, g.now());
        }
        g.pause();
    }
    Came::Stalled(last.saturating_sub(plug.boot))
}

/// How many of `cgroup`'s processes the kernel has ended for want of memory, by any of its
/// OOM killers (`memory.events` `oom_kill`, cgroup-v2.rst), its descendants' with them.
pub(crate) fn oom_kills(cgroup: &str) -> u64 {
    std::fs::read_to_string(format!("{cgroup}/memory.events"))
        .ok()
        .and_then(|e| {
            e.lines()
                .find_map(|l| l.strip_prefix("oom_kill ")?.trim().parse().ok())
        })
        .unwrap_or(0)
}

/// What the kernel said as it last ended a process for want of memory (/dev/kmsg): what
/// asked for memory and of what order, the memory free in each zone against its
/// watermarks, and what it ended (mm/oom_kill.c `dump_header`, `__oom_kill_process`;
/// mm/show_mem.c `show_free_areas`). It counts the kill before it says what it ended, so
/// where that is not said yet, it is read again, for as long as [`STALLED`].
pub(crate) fn oom_said() -> Vec<String> {
    let began = Instant::now();
    loop {
        let said = oom_report();
        if said.iter().any(|l| ended(l)) || began.elapsed() > STALLED {
            return said;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Whether a line of the kernel's says what it ended: "Out of memory: Killed process", or
/// "Memory cgroup out of memory: Killed process" (mm/oom_kill.c `oom_kill_process`).
fn ended(line: &str) -> bool {
    line.to_ascii_lowercase()
        .contains("out of memory: killed process")
}

/// The lines of the kernel's last report of a process ended for want of memory, as
/// [`oom_said`] relays them.
fn oom_report() -> Vec<String> {
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/kmsg")
    else {
        // A run's root moved over the VM's, its /dev has no kmsg: the log read whole with
        // syslog(2), which init, the VM's own, may.
        return oom_lines(&syslog_records());
    };
    let mut records = Vec::new();
    let mut record = [0u8; 8192];
    loop {
        // Each read is one record, `level,seq,usecs,flags;text`; past the last, EAGAIN.
        let n = match (&file).read(&mut record) {
            Ok(0) => break,
            Ok(n) => n,
            // Records overwritten before they were read: the next is the oldest kept.
            Err(e) if e.raw_os_error() == Some(libc::EPIPE) => continue,
            Err(_) => break,
        };
        records.push(String::from_utf8_lossy(record.get(..n).unwrap_or_default()).into_owned());
    }
    oom_lines(&records)
}

/// The kernel's log read whole with syslog(2) (`SYSLOG_ACTION_READ_ALL`), each line a
/// record as /dev/kmsg gives one (`LEVEL,SEQ,USECS,FLAGS;TEXT`), its fields but the text
/// left as nothing is read of them.
fn syslog_records() -> Vec<String> {
    const READ_ALL: libc::c_int = 3;
    const SIZE_BUFFER: libc::c_int = 10;
    // SAFETY: syslog(2)'s size query takes no buffer.
    let size = unsafe { libc::klogctl(SIZE_BUFFER, std::ptr::null_mut(), 0) };
    let Ok(len) = usize::try_from(size) else {
        return Vec::new();
    };
    let mut buf = vec![0u8; len.max(1)];
    let room = libc::c_int::try_from(buf.len()).unwrap_or(libc::c_int::MAX);
    // SAFETY: a buffer of `room` bytes, which syslog(2) fills at most.
    let read = unsafe { libc::klogctl(READ_ALL, buf.as_mut_ptr().cast(), room) };
    let Ok(read) = usize::try_from(read) else {
        return Vec::new();
    };
    String::from_utf8_lossy(buf.get(..read).unwrap_or_default())
        .lines()
        .map(syslog_record)
        .collect()
}

/// A line of syslog(2)'s, `<LEVEL>[SECONDS] TEXT`, as /dev/kmsg's record of it.
fn syslog_record(line: &str) -> String {
    let text = line.split_once("] ").map_or(line, |(_, t)| t);
    format!("0,0,0,-;{text}")
}

/// Of the kernel's log `records`, the last report of a process ended for want of memory:
/// each of its lines that says why, one line each, as a record of several lines is
/// (`\x0a`, Documentation/ABI/testing/dev-kmsg).
fn oom_lines(records: &[String]) -> Vec<String> {
    let mut said = Vec::new();
    for record in records {
        let Some((_, text)) = record.split_once(';') else {
            continue;
        };
        let text = text.lines().next().unwrap_or_default();
        if text.contains("invoked oom-killer") {
            said.clear();
        }
        if text.contains("invoked oom-killer")
            // A cgroup's own: what it held and was allowed, of memory and of swap
            // (mm/memcontrol.c, mem_cgroup_print_oom_meminfo).
            || text.starts_with("memory: usage")
            || text.starts_with("swap: usage")
            || text.starts_with("active_anon:")
            || (text.starts_with("Node ") && text.contains(" min:"))
            || text.starts_with("oom-kill:")
            || ended(text)
        {
            said.push(
                text.replace("\\x0a", " ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
    }
    said
}

/// The process that grows the builder's memory, in the root cgroup: each time the build
/// reaches its `memory.high`, it asks for as much again as the builder has, whole memory
/// blocks, up to what it may have (growth by doubling, which keeps what is plugged ahead
/// of the build at most what it holds, and the times it is asked for to the logarithm),
/// and raises `memory.high` as each block comes online. With all of it plugged, the
/// build's `memory.high` goes: it is held only where the builder's memory ends. Where
/// memory asked for stops coming (`come`), the build's `memory.high` goes too, and says
/// so: the build runs on what the builder has, as one of that size would, and is ended
/// by the kernel where it needs more, never held at a `memory.high` nothing raises (D137).
///
/// What the kernel says as it ends a process for want of memory is not the grower's to say:
/// a step's runner says it, once the step has ended (build.rs), a run's kernel keeps it in its
/// log, which a run's notes are.
fn grow(plug: Plug, events: &OwnedFd, notes: &OwnedFd) -> ! {
    // SAFETY: fcntl(2) of a descriptor this process owns.
    unsafe { libc::fcntl(notes.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    let cgroup = plug.cgroup;
    // No hold but what its host's budget leaves it (D188).
    let lift = || {
        if let Err(e) = set_high(cgroup, u64::MAX) {
            say(notes, plug.whose, &e.to_string());
        }
    };
    let mut asked = 0u64;
    let mut buf = [0u8; 4096];
    loop {
        // SAFETY: read(2) into a buffer this process owns; it blocks for the next event.
        let n = unsafe { libc::read(events.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            say(
                notes,
                plug.whose,
                &format!("its watch failed: {}", io::Error::last_os_error()),
            );
            lift();
            // SAFETY: _exit(2) of this child alone.
            unsafe { libc::_exit(1) };
        }
        let (Some(current), Some(high_now)) = (
            number(&format!("{cgroup}/memory.current")),
            number(&format!("{cgroup}/memory.high")),
        ) else {
            continue;
        };
        // The most it may have, read again: a run's microVM's is the run's own, which the
        // host sets as the microVM takes its run (D148), and no more than its budget's
        // room lets its run hold (D188).
        let plug = Plug {
            most: most_within(
                plug.boot,
                control_read64(control::HOTPLUG_SIZE).unwrap_or(plug.most),
                cgroup,
            ),
            ..plug
        };
        if current < high_now {
            continue;
        }
        // Held where its host's budget holds it (D188): no memory plugged would be its.
        if high_now >= cap() {
            continue;
        }
        let Ok(total) = mem_total() else { continue };
        // What stays plugged: a give-back's driver unplugs the rest (D188).
        let total = plug.boot.saturating_add(staying(plug.boot, total));
        if total.saturating_sub(plug.boot) >= plug.most {
            lift();
            continue;
        }
        // Never less than is asked for now: the device's request, which a run gives back
        // by (D188), else what it asked for last.
        let asking = control_read64(control::HOTPLUG_REQUEST).unwrap_or(asked);
        asked = total
            .saturating_mul(2)
            .saturating_sub(plug.boot)
            .checked_next_multiple_of(ALIGN)
            .unwrap_or(u64::MAX)
            .max(asking)
            .min(plug.most);
        let mut builder = Builder {
            plug,
            notes,
            began: Instant::now(),
        };
        match come(plug, asked, &mut builder) {
            Came::All if asked >= plug.most => lift(),
            Came::All => {}
            Came::Stalled(came) => {
                say(
                    notes,
                    plug.whose,
                    &format!(
                        "{asked} bytes asked for, {came} came, though asked for twice: what it holds now runs on what it has, {} bytes",
                        mem_total().unwrap_or(total)
                    ),
                );
                lift();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    const PLUG: Plug = Plug {
        boot: 512 * MIB,
        most: 2048 * MIB,
        cgroup: BUILD,
        whose: "the builder's",
    };

    /// A builder whose memory is what `comes` makes of the requests so far and the time.
    struct Fake<F> {
        comes: F,
        asks: Vec<u64>,
        raised: Vec<u64>,
        said: Vec<String>,
        clock: Duration,
    }

    impl<F: FnMut(&[u64], Duration) -> u64> Grows for Fake<F> {
        fn total(&mut self) -> io::Result<u64> {
            Ok((self.comes)(&self.asks, self.clock))
        }
        fn ask(&mut self, bytes: u64) -> io::Result<()> {
            self.asks.push(bytes);
            Ok(())
        }
        fn raise(&mut self, plugged: u64) {
            self.raised.push(plugged);
        }
        fn say(&mut self, what: &str) {
            self.said.push(what.to_string());
        }
        fn kernel_said(&mut self) -> Vec<String> {
            vec!["virtio_mem virtio5: adding memory failed: -12".into()]
        }
        fn now(&mut self) -> Duration {
            self.clock
        }
        fn pause(&mut self) {
            self.clock += Duration::from_millis(1);
        }
    }

    fn fake<F: FnMut(&[u64], Duration) -> u64>(comes: F) -> Fake<F> {
        Fake {
            comes,
            asks: Vec::new(),
            raised: Vec::new(),
            said: Vec::new(),
            clock: Duration::ZERO,
        }
    }

    /// Under a budget a run's microVM plugs no more than lets its run hold its cap beside
    /// its kernel's and the slack, with the page structures of what it plugs, in whole
    /// blocks, never past its most; with a cap its boot holds, none.
    #[test]
    fn a_microvm_held_to_a_budget_plugs_no_more_than_its_cap_needs() {
        let boot = 256 * MIB;
        let most = 16 << 30;
        // 96 MiB of room past 200 MiB used, 40 MiB its kernel's: 400 MiB with the slack,
        // 406 with its page structures, 150 past boot, two blocks.
        assert_eq!(plug_for(boot, most, 296 * MIB, 40 * MIB), 2 * ALIGN);
        assert_eq!(plug_for(boot, most, 100 * MIB, 40 * MIB), 0);
        assert_eq!(plug_for(boot, 128 * MIB, 4 << 30, 40 * MIB), 128 * MIB);
        assert_eq!(plug_for(boot, most, u64::MAX - 1, 0), most);
    }

    /// A run is held within the room its host's budget leaves it (D188): that room past
    /// what it uses, and never less than the room it needs to go on; a room short of that
    /// raises no hold it had, as its host tells it again while it is short, and lowers one
    /// it no longer fills; in whole pages, as its kernel keeps the hold, none still none.
    #[test]
    fn a_run_is_held_within_its_budgets_room() {
        let least = 2 * MIB;
        assert_eq!(within(100 * MIB, 50 * MIB as i64, least, u64::MAX), 150 * MIB);
        assert_eq!(within(100 * MIB, 0, least, u64::MAX), 102 * MIB);
        assert_eq!(within(100 * MIB, -50 * MIB as i64, least, u64::MAX), 102 * MIB);
        assert_eq!(within(100 * MIB, MIB as i64, least, u64::MAX), 102 * MIB);
        // Held short at 102 MiB, its run grown to it: told short again, held there still.
        assert_eq!(within(102 * MIB, -(MIB as i64), least, 102 * MIB), 102 * MIB);
        // Its run since shrunk: held as close as the least room.
        assert_eq!(within(60 * MIB, -(MIB as i64), least, 102 * MIB), 62 * MIB);
        // Room again: past it.
        assert_eq!(within(102 * MIB, 40 * MIB as i64, least, 102 * MIB), 142 * MIB);
        assert_eq!(within(u64::MAX - 1, 10, least, u64::MAX), u64::MAX);
        assert_eq!(in_pages(100 * MIB + 4097, 4096), 100 * MIB + 4096);
        assert_eq!(in_pages(4095, 4096), 0);
        assert_eq!(in_pages(u64::MAX, 4096), u64::MAX);
    }

    /// A hold counts out what its device has been asked to unplug, free or not, so that the
    /// run, its kernel's own and its reserve fit what stays: set as a give-back's last 128
    /// MiB still went (CI 40148050379's h, a past128 read: booted with 233 MiB, the run
    /// using 20 MiB, 302 MiB free), the hold counting it as free held the run at 252 MiB,
    /// past all its microVM had once it had gone (PM M324).
    #[test]
    fn a_hold_counts_out_what_goes_as_it_is_unplugged() {
        let (boot, total, free, current) = (244_293_632u64, 378_511_360u64, 316_907_520u64, 20_647_936u64);
        let (kernel, leaving) = (total - free - current, total - boot);
        let reserve = reserve(total, u64::MAX);
        assert_eq!(
            hold_of(current, free, 0, reserve),
            264_532_352,
            "the hold it was given"
        );
        assert_eq!(hold_of(current, free, 0, reserve) + kernel + reserve, total);
        let held = hold_of(current, free, leaving, reserve);
        assert_eq!(held + kernel + reserve, boot, "within what stays");
        // With nothing going, as before.
        assert_eq!(hold_of(current, free, 0, 0), current + free);
        assert_eq!(hold_of(current, free, current + free, reserve), 0);
    }

    /// The least room a run is held within covers what its kernel charges it ahead on
    /// each CPU, two read-ahead windows, and a share's largest request twice, its reader's
    /// block and the page cache it fills: on 4 CPUs of 4 KiB pages, 3.25 MiB, past the 1.5
    /// MiB a reader of 1 MiB blocks needed, and on 18, 6.75 MiB, past the 2 MiB it needed
    /// there (PM M309, M321); this guest's, by its own CPUs.
    #[test]
    fn a_run_held_short_keeps_the_room_its_reads_need() {
        assert_eq!(working_room_of(4, 4096), 13 * MIB / 4);
        assert!(working_room_of(4, 4096) >= 3 * MIB / 2);
        assert_eq!(working_room_of(18, 4096), 27 * MIB / 4);
        assert!(working_room_of(18, 4096) >= 2 * MIB);
        // SAFETY: sysconf(3) reads a value of the running kernel's.
        let cpus = u64::try_from(unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) })
            .unwrap()
            .max(1);
        assert_eq!(working_room(), working_room_of(cpus, page_size()));
    }

    /// A run gives back what it holds plugged past twice what it uses, in whole blocks, as
    /// its host asks; never what it used at its peak since it was last asked, nor anything
    /// while memory it asked for to grow is still to come, nor less than a block.
    #[test]
    fn a_run_keeps_its_peak_and_what_it_asked_for() {
        const GIB: u64 = 1 << 30;
        let boot = 256 * MIB;
        // Using 300 MiB of 4 GiB plugged: it keeps 600 less what it booted with, 344 MiB,
        // in whole blocks, 384.
        assert_eq!(
            to_keep(boot, 8 * GIB, 4 * GIB, 4 * GIB, 300 * MIB, 300 * MIB),
            Some(384 * MIB)
        );
        // Having reached 2.5 GiB since it was last asked: it keeps that.
        let peak = 2560 * MIB;
        assert_eq!(
            to_keep(boot, 8 * GIB, 4 * GIB, 4 * GIB, 300 * MIB, peak),
            Some((peak - boot).next_multiple_of(ALIGN))
        );
        // Growing, a block or more it asked for not yet plugged: it gives back nothing.
        assert_eq!(
            to_keep(boot, 8 * GIB, 4 * GIB, 5 * GIB, 300 * MIB, 300 * MIB),
            None
        );
        // What it would give back is less than a block, 448 MiB plugged and 384 kept; and a
        // block exactly.
        assert_eq!(
            to_keep(boot, 8 * GIB, 448 * MIB, 448 * MIB, 300 * MIB, 300 * MIB),
            None
        );
        assert_eq!(
            to_keep(boot, 8 * GIB, 512 * MIB, 512 * MIB, 300 * MIB, 300 * MIB),
            Some(384 * MIB)
        );
        // Never past its most.
        assert_eq!(to_keep(boot, GIB, 4 * GIB, 4 * GIB, 3 * GIB, 3 * GIB), Some(GIB));
    }

    /// The kernel's memory less its report of what may still be plugged (D149): read
    /// again where memory came between the reports, as the one the kernel added is then
    /// not known.
    #[test]
    fn memory_is_read_without_the_kernels_report() {
        let mut reports = [640 * MIB, 512 * MIB, 512 * MIB, 512 * MIB].into_iter();
        let mut reads = [[896 * MIB, 700 * MIB], [896 * MIB, 690 * MIB]].into_iter();
        let memory = unreported_by(
            || Ok(reports.next().unwrap_or(0)),
            || Ok(reads.next().unwrap_or_default()),
        );
        assert_eq!(memory.unwrap(), [384 * MIB, 178 * MIB]);
        assert_eq!((reports.next(), reads.next()), (None, None));
    }

    /// What the kernel said as it ended a step's process for want of memory, as a builder of
    /// 1 GiB said it (6.18.48, a step growing past all it may have): of its last report,
    /// the lines that say why, each one line.
    #[test]
    fn a_kill_is_said_in_the_kernels_words() {
        let records: Vec<String> = [
            "3,900,1000,-;awk invoked oom-killer: gfp_mask=0x140dca(GFP_HIGHUSER_MOVABLE|__GFP_ZERO|__GFP_COMP), order=0, oom_score_adj=0",
            "3,901,1000,-;Out of memory: Killed process 150 (awk) total-vm:900000kB, anon-rss:890000kB, file-rss:0kB, shmem-rss:0kB, UID:0 pgtables:1800kB oom_score_adj:0",
            "6,902,2000,-;virtio_mem virtio5: plugged size: 0x20000000",
            "3,1001,3000,-;awk invoked oom-killer: gfp_mask=0x140dca(GFP_HIGHUSER_MOVABLE|__GFP_ZERO|__GFP_COMP), order=0, oom_score_adj=0",
            "4,1002,3000,-;Mem-Info:",
            "4,1003,3000,-;active_anon:279 inactive_anon:236227 isolated_anon:0\\x0a active_file:0 inactive_file:0 isolated_file:0\\x0a free:815 free_pcp:5968 free_cma:0",
            "4,1004,3000,-;Node 0 active_anon:1116kB inactive_anon:944896kB all_unreclaimable? yes",
            "4,1005,3000,-;Node 0 DMA free:3220kB boost:0kB min:1920kB low:2400kB high:2880kB managed:483492kB free_pcp:17300kB",
            "4,1006,3000,-;lowmem_reserve[]: 0 0 0 0",
            "4,1007,3000,-;Node 0 Normal free:40kB boost:0kB min:2080kB low:2604kB high:3128kB managed:524288kB free_pcp:6572kB",
            "4,1008,3000,-;Node 0 DMA: 5*4kB (UME) 2*8kB (M) = 36kB",
            "6,1009,3000,-;Tasks state (memory values in pages):",
            "6,1010,3000,-;[    199]     0   199   236855   236120  1888        0 awk",
            "6,1011,3000,-;oom-kill:constraint=CONSTRAINT_NONE,nodemask=(null),cpuset=0,mems_allowed=0,global_oom,task_memcg=/build/steps/0,task=awk,pid=199,uid=0",
            "3,1012,3000,-;Out of memory: Killed process 199 (awk) total-vm:947420kB, anon-rss:944496kB, file-rss:576kB, shmem-rss:0kB, UID:0 pgtables:1888kB oom_score_adj:0",
        ]
        .iter()
        .map(|r| r.to_string())
        .collect();
        assert_eq!(
            oom_lines(&records),
            [
                "awk invoked oom-killer: gfp_mask=0x140dca(GFP_HIGHUSER_MOVABLE|__GFP_ZERO|__GFP_COMP), order=0, oom_score_adj=0",
                "active_anon:279 inactive_anon:236227 isolated_anon:0 active_file:0 inactive_file:0 isolated_file:0 free:815 free_pcp:5968 free_cma:0",
                "Node 0 DMA free:3220kB boost:0kB min:1920kB low:2400kB high:2880kB managed:483492kB free_pcp:17300kB",
                "Node 0 Normal free:40kB boost:0kB min:2080kB low:2604kB high:3128kB managed:524288kB free_pcp:6572kB",
                "oom-kill:constraint=CONSTRAINT_NONE,nodemask=(null),cpuset=0,mems_allowed=0,global_oom,task_memcg=/build/steps/0,task=awk,pid=199,uid=0",
                "Out of memory: Killed process 199 (awk) total-vm:947420kB, anon-rss:944496kB, file-rss:576kB, shmem-rss:0kB, UID:0 pgtables:1888kB oom_score_adj:0",
            ]
        );
        assert!(ended(
            "Memory cgroup out of memory: Killed process 7 (cc1) total-vm:1kB"
        ));
    }

    /// A cgroup's own kill (6.18.48, a joiner of 64 MiB and no swap allocating 96): what
    /// its cgroup held and was allowed, of memory and of swap, said with the rest; and the
    /// log as syslog(2) reads it, each line a record as /dev/kmsg gives one.
    #[test]
    fn a_cgroups_kill_says_its_memory_and_swap() {
        let records: Vec<String> = [
            "<4>[    5.101010] kworker/u72:3 invoked oom-killer: gfp_mask=0xc00(GFP_NOIO), order=0, oom_score_adj=0",
            "<6>[    5.101020] memory: usage 65536kB, limit 65536kB, failcnt 144",
            "<6>[    5.101030] swap: usage 0kB, limit 0kB, failcnt 0",
            "<6>[    5.101040] Memory cgroup stats for /run/join-2:",
            "<6>[    5.101050] oom-kill:constraint=CONSTRAINT_MEMCG,nodemask=(null),cpuset=/,mems_allowed=0,oom_memcg=/run/join-2,task_memcg=/run/join-2,task=testguest,pid=202,uid=1000",
            "<3>[    5.101060] Memory cgroup out of memory: Killed process 202 (testguest) total-vm:68060kB, anon-rss:65088kB, file-rss:64kB, shmem-rss:0kB, UID:1000 pgtables:168kB oom_score_adj:0",
        ]
        .iter()
        .map(|l| syslog_record(l))
        .collect();
        assert_eq!(
            records[1],
            "0,0,0,-;memory: usage 65536kB, limit 65536kB, failcnt 144"
        );
        assert_eq!(
            oom_lines(&records),
            [
                "kworker/u72:3 invoked oom-killer: gfp_mask=0xc00(GFP_NOIO), order=0, oom_score_adj=0",
                "memory: usage 65536kB, limit 65536kB, failcnt 144",
                "swap: usage 0kB, limit 0kB, failcnt 0",
                "oom-kill:constraint=CONSTRAINT_MEMCG,nodemask=(null),cpuset=/,mems_allowed=0,oom_memcg=/run/join-2,task_memcg=/run/join-2,task=testguest,pid=202,uid=1000",
                "Memory cgroup out of memory: Killed process 202 (testguest) total-vm:68060kB, anon-rss:65088kB, file-rss:64kB, shmem-rss:0kB, UID:1000 pgtables:168kB oom_score_adj:0",
            ]
        );
    }

    /// Memory that comes is asked for once, and raises the build's `memory.high`.
    #[test]
    fn memory_asked_for_comes() {
        let mut g = fake(|asks: &[u64], _| PLUG.boot + asks.last().copied().unwrap_or(0));
        assert_eq!(come(PLUG, 512 * MIB, &mut g), Came::All);
        assert_eq!(g.asks, [512 * MIB]);
        assert_eq!(g.raised, [512 * MIB]);
        assert!(g.said.is_empty(), "{:?}", g.said);
    }

    /// Memory that comes slowly, a block every four seconds, is never stalled.
    #[test]
    fn memory_that_comes_slowly_is_waited_for() {
        let mut g = fake(|asks: &[u64], t: Duration| {
            PLUG.boot + (128 * MIB * (t.as_secs() / 4)).min(asks.last().copied().unwrap_or(0))
        });
        assert_eq!(come(PLUG, 512 * MIB, &mut g), Came::All);
        assert_eq!(g.asks, [512 * MIB]);
        assert_eq!(g.raised, [128 * MIB, 256 * MIB, 384 * MIB, 512 * MIB]);
    }

    /// Memory that stops coming is asked for again after [`STALLED`], saying so with the
    /// kernel's words, which tells the driver again; it comes then, and is waited for.
    #[test]
    fn memory_that_stops_coming_is_asked_for_again() {
        let mut g = fake(|asks: &[u64], _| PLUG.boot + if asks.len() > 1 { 512 * MIB } else { 0 });
        assert_eq!(come(PLUG, 512 * MIB, &mut g), Came::All);
        assert_eq!(g.asks, [512 * MIB, 512 * MIB]);
        assert_eq!(
            g.said,
            [
                "536870912 bytes asked for, 0 came in 5 s; asking again; the kernel said: virtio_mem virtio5: adding memory failed: -12"
            ]
        );
    }

    /// Memory that does not come though asked for twice is given up, with what came of it,
    /// after [`STALLED`] twice and no sooner.
    #[test]
    fn memory_that_never_comes_is_given_up() {
        let mut g = fake(|asks: &[u64], _| PLUG.boot + if asks.is_empty() { 0 } else { 256 * MIB });
        assert_eq!(come(PLUG, 512 * MIB, &mut g), Came::Stalled(256 * MIB));
        assert_eq!(g.asks.len(), 2);
        assert!(
            g.clock > 2 * STALLED && g.clock < 2 * STALLED + Duration::from_millis(10),
            "{:?}",
            g.clock
        );
    }

    /// The give-back connection's two ends: the host's, and the answerer's as init holds it.
    fn give_pair() -> (std::os::unix::net::UnixStream, std::fs::File) {
        let (host, guest) = std::os::unix::net::UnixStream::pair().unwrap();
        (host, std::fs::File::from(std::os::fd::OwnedFd::from(guest)))
    }

    /// The host's end of a give-back connection, shut both ways as it drops: a test that
    /// fails still ends its answerer, which would otherwise wait on it for ever.
    struct Shut<'a>(&'a std::os::unix::net::UnixStream);

    impl Drop for Shut<'_> {
        fn drop(&mut self) {
            let _ = self.0.shutdown(std::net::Shutdown::Both);
        }
    }

    /// The next frame the answerer sent the host: its kind and payload.
    fn heard(host: &std::os::unix::net::UnixStream) -> (u8, String) {
        use shards_abi::run;
        let mut r = host;
        let mut h = [0u8; run::HEADER];
        r.read_exact(&mut h).unwrap();
        let (which, len) = run::parse_header(h).unwrap();
        let mut payload = vec![0u8; len as usize];
        r.read_exact(&mut payload).unwrap();
        (which, String::from_utf8(payload).unwrap())
    }

    /// Each `GIVE_BACK` is answered with a `GAVE` of what was given back, the bytes its
    /// files freed and those it asked to unplug, in decimal, or of why nothing was; a frame of
    /// any other kind is passed over, and its answerer ends as its connection does.
    #[test]
    fn a_give_back_is_answered_with_what_was_given_back() {
        use shards_abi::{build::kind, run};
        let (host, guest) = give_pair();
        let gave = std::sync::Mutex::new(vec![Err(io::Error::other("no cgroup")), Ok((5 * MIB, 128 * MIB))]);
        std::thread::scope(|s| {
            let answerer = s.spawn(|| answer(&guest, || gave.lock().unwrap().pop().unwrap(), |_| Ok(None)));
            let _shut = Shut(&host);
            let mut w = &host;
            w.write_all(&run::header(kind::EMULATE, 3)).unwrap();
            w.write_all(b"abc").unwrap();
            w.write_all(&run::header(kind::GIVE_BACK, 0)).unwrap();
            assert_eq!(heard(&host), (kind::GAVE, "5242880 134217728".into()));
            w.write_all(&run::header(kind::GIVE_BACK, 0)).unwrap();
            assert_eq!(heard(&host), (kind::GAVE, "nothing given back: no cgroup".into()));
            host.shutdown(std::net::Shutdown::Write).unwrap();
            answerer.join().unwrap();
        });
        assert!(gave.lock().unwrap().is_empty());
    }

    /// Each `ROOM` is answered with a `HELD` of what the build was held within past what it
    /// uses: the room parsed as signed decimal bytes, none where its payload is empty; a room
    /// that is no number refused unheld, and a hold that failed said, each answered all the
    /// same.
    #[test]
    fn a_room_is_answered_with_what_the_build_is_held_within() {
        use shards_abi::{build::kind, run};
        let (host, guest) = give_pair();
        let rooms = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|s| {
            let answerer = s.spawn(|| {
                answer(
                    &guest,
                    || Ok((0, 0)),
                    |room| {
                        rooms.lock().unwrap().push(room);
                        match room {
                            Some(r) if r < 0 => Err(io::Error::other("no memory.high")),
                            Some(_) => Ok(Some(64 * MIB)),
                            None => Ok(None),
                        }
                    },
                )
            });
            let _shut = Shut(&host);
            let mut w = &host;
            let mut room = |payload: &[u8]| {
                w.write_all(&run::header(kind::ROOM, u32::try_from(payload.len()).unwrap()))
                    .unwrap();
                w.write_all(payload).unwrap();
                heard(&host)
            };
            assert_eq!(room(b"134217728"), (kind::HELD, "67108864".into()));
            assert_eq!(room(b""), (kind::HELD, String::new()));
            assert_eq!(
                room(b"x"),
                (
                    kind::HELD,
                    "not held: a room that is no number of bytes: \"x\"".into()
                )
            );
            assert_eq!(room(b"-5"), (kind::HELD, "not held: no memory.high".into()));
            host.shutdown(std::net::Shutdown::Write).unwrap();
            answerer.join().unwrap();
        });
        assert_eq!(*rooms.lock().unwrap(), [Some(128 * MIB as i64), None, Some(-5)]);
    }

    /// A frame longer than any its host sends ends the connection, unread and unanswered,
    /// rather than have its answerer hold it.
    #[test]
    fn a_give_back_frame_past_its_most_ends_the_connection() {
        use shards_abi::{build::kind, run};
        let (host, guest) = give_pair();
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let (ended, end) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                answer(
                    &guest,
                    || {
                        asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        Ok((0, 0))
                    },
                    |_| Ok(None),
                );
                ended.send(()).unwrap();
            });
            let len = u32::try_from(GIVE_FRAME + 1).unwrap();
            (&host).write_all(&run::header(kind::GIVE_BACK, len)).unwrap();
            let gone = end.recv_timeout(Duration::from_secs(10));
            // Ends a mutant's read, which waits for the frame's bytes.
            host.shutdown(std::net::Shutdown::Both).unwrap();
            gone.unwrap();
        });
        assert_eq!(asked.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
}
