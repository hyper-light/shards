//! What a workload is given by default, a container's in a `docker run` and in a BuildKit
//! step (moby docker-v29.3.1 daemon/pkg/oci/defaults.go, caps/defaults.go; runc
//! libcontainer/rootfs_linux.go; docs/research/buildkit-run.md §3–4), and measured in both
//! (Docker Desktop's dockerd 29.3.1 and BuildKit v0.28, 2026-10-02): its capabilities,
//! the paths masked or made read-only, and what /dev holds.

/// Capabilities, by number (shards_abi::run::CAPS).
pub use shards_abi::run::CAPS;

/// Paths masked: a file under /dev/null, a directory under an empty read-only tmpfs.
pub const MASKED: [&str; 12] = [
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

/// Paths made read-only.
pub const READONLY: [&str; 5] = [
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
];

/// The devices /dev holds, by name, major and minor, each 0666: runc's.
pub const DEVICES: [(&str, u32, u32); 6] = [
    ("null", 1, 3),
    ("zero", 1, 5),
    ("full", 1, 7),
    ("random", 1, 8),
    ("urandom", 1, 9),
    ("tty", 5, 0),
];

/// The links /dev holds, by target and name: runc's (setupDevSymlinks), and the
/// workload's own devpts's ptmx. `core`, to /proc/kcore, where that exists.
pub const LINKS: [(&str, &str); 5] = [
    ("/proc/self/fd", "fd"),
    ("/proc/self/fd/0", "stdin"),
    ("/proc/self/fd/1", "stdout"),
    ("/proc/self/fd/2", "stderr"),
    ("pts/ptmx", "ptmx"),
];

/// The highest capability number of the kernel shards boots (Linux 6.x,
/// CAP_CHECKPOINT_RESTORE), where /proc/sys/kernel/cap_last_cap cannot be read.
pub const CAP_LAST_CAP: u32 = 40;

/// The kernel's highest capability number.
pub fn last_cap() -> u32 {
    std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(CAP_LAST_CAP)
}

/// capset(2)'s header and data, version 3.
#[repr(C)]
struct CapHeader {
    version: u32,
    pid: libc::c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Drops from the bounding set each capability up to `last` that `keep` does not keep;
/// whether all went. Async-signal-safe: prctl(2) alone.
pub fn bound(last: u32, keep: impl Fn(u32) -> bool) -> bool {
    // SAFETY: prctl(2) with constant arguments.
    (0..=last)
        .all(|c| keep(c) || unsafe { libc::prctl(libc::PR_CAPBSET_DROP, c as libc::c_ulong, 0, 0, 0) } == 0)
}

/// Makes the effective and permitted sets the capabilities up to `last` that `keep`
/// keeps, and the inheritable set none; whether it could. Async-signal-safe: capset(2)
/// alone.
pub fn set(last: u32, keep: impl Fn(u32) -> bool) -> bool {
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
    unsafe { libc::syscall(libc::SYS_capset, &raw mut header, data.as_ptr()) == 0 }
}

/// Takes `groups`, `gid` and `uid` (setgroups(2), setresgid(2), setresuid(2)); whether it
/// could. Async-signal-safe: the system calls alone, for a child of `clone`. musl's
/// wrappers change every thread of a process (`__synccall`), finding the others in its
/// thread list unless `gettid()` differs from the caller's recorded tid
/// (src/thread/synccall.c); a child of init's in a PID namespace of its own is tid 1, as
/// init is, so it would look for init's threads, which it does not have, and fail.
pub fn take_ids(groups: &[libc::gid_t], gid: libc::gid_t, uid: libc::uid_t) -> bool {
    // SAFETY: setgroups(2) with a list of the length given, then setresgid(2) and
    // setresuid(2) with values; each changes the calling thread, the child's only one.
    unsafe {
        libc::syscall(libc::SYS_setgroups, groups.len(), groups.as_ptr()) == 0
            && libc::syscall(libc::SYS_setresgid, gid, gid, gid) == 0
            && libc::syscall(libc::SYS_setresuid, uid, uid, uid) == 0
    }
}

/// Gives every signal its default disposition and blocks none, as a new process has them
/// (signal(7)), whatever init ignores or blocks: execve(2) keeps an ignored signal ignored
/// and the mask as it was, and Rust's runtime ignores SIGPIPE in init before main (std
/// `reset_sigpipe`, `sigpipe::DEFAULT`). A signal whose disposition cannot change keeps
/// its default: SIGKILL and SIGSTOP, and the three musl keeps for itself (32 to 34), which
/// its sigaction refuses. Whether the mask was set. Async-signal-safe: sigaction(2) and
/// sigprocmask(2) alone, for a child of `clone`.
pub fn signals() -> bool {
    for sig in 1..=shards_abi::run::NSIG as libc::c_int {
        // SAFETY: signal(2) setting a disposition; one it cannot set it refuses.
        unsafe { libc::signal(sig, libc::SIG_DFL) };
    }
    // SAFETY: an empty set on this stack, then the mask set to it.
    unsafe {
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&raw mut none);
        libc::sigprocmask(libc::SIG_SETMASK, &raw const none, std::ptr::null_mut()) == 0
    }
}

/// Sets a resource limit (prlimit(2) of the caller), not through musl's setrlimit, which
/// applies it as `take_ids` says its ID calls do; whether it could.
pub fn rlimit(resource: libc::c_int, cur: libc::rlim_t, max: libc::rlim_t) -> bool {
    let limit = libc::rlimit {
        rlim_cur: cur,
        rlim_max: max,
    };
    // SAFETY: prlimit64(2) of this process, with a limit struct and no old one.
    unsafe {
        libc::syscall(
            libc::SYS_prlimit64,
            0,
            resource,
            &raw const limit,
            std::ptr::null::<libc::rlimit>(),
        ) == 0
    }
}

/// The resource limits a microVM's workload starts with: those its image's programs are
/// made to meet on a Docker host, whose runtime inherits them from containerd's (its
/// systemd unit's LimitNOFILE and LimitNPROC infinity; measured on Docker Desktop 29.3.1,
/// `cat /proc/self/limits`, 2026-10-05): open files 1048576 (the kernel's nr_open),
/// processes and locked memory unlimited. The guest kernel's own would be its init's
/// (1024:4096 files, and processes and pending signals by the VM's small memory). Pending
/// signals stay the kernel's, sized by the microVM's memory. `--ulimit` sets its own over
/// them.
pub fn limits() {
    let set = |resource, cur, max| {
        let limit = libc::rlimit {
            rlim_cur: cur,
            rlim_max: max,
        };
        // SAFETY: setrlimit(2) of a resource with a limit on our stack; a limit the kernel
        // refuses leaves its own.
        unsafe { libc::setrlimit(resource, &limit) };
    };
    set(libc::RLIMIT_NOFILE, 1_048_576, 1_048_576);
    set(libc::RLIMIT_NPROC, libc::RLIM_INFINITY, libc::RLIM_INFINITY);
    set(libc::RLIMIT_MEMLOCK, libc::RLIM_INFINITY, libc::RLIM_INFINITY);
}

#[cfg(test)]
mod tests {
    /// A child of a process that ignores SIGPIPE (as Rust's runtime leaves this test's and
    /// init's), ignores another signal and blocks one has every signal at its default and
    /// none blocked once [`super::signals`] has run: what it says on its exit status, a bit
    /// for each that is not so.
    #[test]
    fn signals_are_a_new_processs_whatever_its_parent_ignored_or_blocked() {
        // SAFETY: fork(2); the child calls async-signal-safe functions alone, then _exit(2).
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
        if pid == 0 {
            // SAFETY: async-signal-safe calls on sets and actions on this stack, then
            // _exit(2), in the child alone.
            unsafe {
                libc::signal(libc::SIGUSR2, libc::SIG_IGN);
                let mut usr1: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&raw mut usr1);
                libc::sigaddset(&raw mut usr1, libc::SIGUSR1);
                libc::sigprocmask(libc::SIG_BLOCK, &raw const usr1, std::ptr::null_mut());
                let set = super::signals();
                let mut ignored = false;
                for sig in 1..=shards_abi::run::NSIG as libc::c_int {
                    let mut old: libc::sigaction = std::mem::zeroed();
                    ignored |= libc::sigaction(sig, std::ptr::null(), &raw mut old) == 0
                        && old.sa_sigaction == libc::SIG_IGN;
                }
                let mut mask: libc::sigset_t = std::mem::zeroed();
                libc::sigprocmask(libc::SIG_SETMASK, std::ptr::null(), &raw mut mask);
                let blocked = libc::sigismember(&raw const mask, libc::SIGUSR1) == 1;
                libc::_exit(i32::from(!set) | i32::from(ignored) << 1 | i32::from(blocked) << 2)
            }
        }
        let mut status = 0;
        // SAFETY: waitpid(2) of this test's own child.
        assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);
        assert!(libc::WIFEXITED(status), "the child ended by signal: {status:#x}");
        assert_eq!(
            libc::WEXITSTATUS(status),
            0,
            "1: the mask was not set; 2: a signal is still ignored; 4: SIGUSR1 is still blocked"
        );
    }
}
