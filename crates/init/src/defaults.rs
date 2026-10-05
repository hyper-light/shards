//! What a container is given by default, the same in a `docker run` and in a BuildKit
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
/// container's own devpts's ptmx. `core`, to /proc/kcore, where that exists.
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
