//! How a microVM is sized where its run names no size (D125, D191): every CPU this process
//! may run on, and the memory that holds what one vCPU's default may allocate. One source for
//! a run's own sizing (`shards`' `run::sizes_for`) and shards-api's pools (D253), which
//! resolve a spec of no size before they key and charge it.

/// The memory a VM has unless it is given more or less, in MiB.
pub const MEMORY_MIB: u64 = 256;

/// The size a run that names none is given on this host, as `(vcpus, memory_mib)`: every CPU
/// this process may run on ([`microvm_vcpus`]), and the memory that holds what one vCPU's
/// [`MEMORY_MIB`] may allocate ([`memory_mib`], no limit), at most the host's memory.
pub fn unsized_run() -> (u32, u64) {
    let vcpus = microvm_vcpus();
    (vcpus, memory_mib(0, MEMORY_MIB, crate::host_memory(), vcpus))
}

/// The CPUs a microVM may have, numbered from 0: every CPU this process may run on (D125).
/// On Windows those of its affinity mask, which `available_parallelism` does not read there
/// ("may overcount the amount of parallelism available when limited by process-wide
/// affinity masks", its documentation), where Linux's reads its sched_getaffinity(2) set:
/// a daemon held to 4 of 32 CPUs gave every microVM 32 vCPUs, which ran on those 4 (PM M495).
pub fn host_cpus() -> usize {
    #[cfg(windows)]
    if let Some(cpus) = affinity_cpus() {
        return cpus;
    }
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// The CPUs of this process's affinity mask, or None where it has none to give: where its
/// threads are in more than one processor group, GetProcessAffinityMask gives no mask
/// (Microsoft, GetProcessAffinityMask).
#[cfg(windows)]
fn affinity_cpus() -> Option<usize> {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessAffinityMask};
    let (mut process, mut system) = (0usize, 0usize);
    // SAFETY: GetCurrentProcess's pseudo-handle is always valid, and both masks are this
    // frame's own.
    let got = unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) };
    if got == 0 || process == 0 {
        return None;
    }
    usize::try_from(process.count_ones()).ok()
}

/// The vCPUs every run's microVM has, whatever its run is given: the host's, all online, so
/// that one template serves every `--cpus` and an update gives a running workload more or
/// fewer of them (D191). Bringing a CPU online as a run starts cost 13 to 45 ms where a
/// microVM had the host's (PM M265), past what a run's start has; a run is held to what it
/// is given by its cgroup.
pub fn microvm_vcpus() -> u32 {
    vcpus_of(host_cpus())
}

/// The vCPUs a run's microVM has on a host that gives it `cpus` ([`microvm_vcpus`]), for a
/// caller that has read them already.
pub fn vcpus_of(cpus: usize) -> u32 {
    u32::try_from(cpus).unwrap_or(u32::MAX)
}

/// The memory a microVM of `vcpus` vCPUs needs, in MiB, for its workload to use `limit`
/// bytes: the least even size, as the VM takes it, whose memory less what its kernel keeps
/// ([`overhead_kib`]) holds the limit; at least `default_mib`; and at most the host's
/// memory, past which a limit limits nothing.
pub fn memory_mib(limit: i64, default_mib: u64, host: Option<u64>, vcpus: u32) -> u64 {
    // Without a limit, the least that holds what may be allocated in one vCPU's default,
    // its vCPUs past the first keeping theirs (D125).
    if limit <= 0 {
        if vcpus <= 1 {
            return default_mib;
        }
        let usable = default_mib
            .saturating_mul(1024)
            .saturating_sub(overhead_kib(default_mib, 1));
        let usable = i64::try_from(usable.saturating_mul(1024)).unwrap_or(i64::MAX);
        return memory_mib(usable, default_mib, host, vcpus);
    }
    let limit_kib = u64::try_from(limit).unwrap_or(0).div_ceil(1024);
    let even = |mib: u64| mib + mib % 2;
    let mut mib = even(limit_kib.div_ceil(1024).max(default_mib));
    // The overhead does not fall as the VM grows, so each step only grows the VM, and
    // the first size that holds the limit is the least.
    while mib.saturating_mul(1024).saturating_sub(overhead_kib(mib, vcpus)) < limit_kib {
        let need = limit_kib.saturating_add(overhead_kib(mib, vcpus)).div_ceil(1024);
        mib = even(need.max(mib + 2));
    }
    match host {
        Some(bytes) => mib.min((bytes >> 20) & !1),
        None => mib,
    }
}

/// What the guest kernel keeps for itself of a VM of `mib` MiB and `vcpus` vCPUs, in KiB,
/// on the path a run takes, as measured on this architecture's guest kernel
/// (platform-measurements.md M179, M183): the overhead of the next measured size up, and
/// past the last, its slope from 8 to 16 GiB; and what each vCPU past the first keeps.
pub fn overhead_kib(mib: u64, vcpus: u32) -> u64 {
    // On the path a run takes (M179): what the guest keeps, its VM's KiB less what may be
    // allocated (MemAvailable, and the free pages its CPUs' lists hold, which MemAvailable
    // leaves out), each size's most of 48 boots of its template on three GitHub x86_64
    // runners (AMD EPYC 9V74 and 9V45, Intel Xeon 8370C), as far as half their 16 GiB;
    // from 8 GiB, cold boots' MemAvailable alone (M123), which keeps those pages out too.
    #[cfg(target_arch = "x86_64")]
    const MEASURED: [(u64, u64); 27] = [
        (256, 48_480),
        (320, 51_616),
        (384, 52_340),
        (448, 54_808),
        (512, 55_416),
        (640, 100_828),
        (768, 113_160),
        (896, 125_176),
        (1024, 130_256),
        (1280, 138_396),
        (1536, 143_428),
        (1792, 148_752),
        (2048, 153_848),
        (2560, 168_196),
        (3072, 178_324),
        (3328, 284_092),
        (3584, 290_224),
        (3840, 296_340),
        (4096, 301_776),
        (4352, 318_280),
        (4608, 323_460),
        (5376, 338_764),
        (6144, 354_220),
        (7168, 374_704),
        (8192, 417_136),
        (12_288, 512_184),
        (16_384, 592_948),
    ];
    /// KiB a MiB past the last size: (592,948 − 417,136) / 8,192 is 21.5.
    #[cfg(target_arch = "x86_64")]
    const SLOPE: u64 = 22;
    /// KiB the second vCPU keeps, and each past it (M183): the most the second kept at any
    /// size (12 boots of each of 24 sizes on three runners), and the most each past it kept
    /// on average at 4, the runners' CPUs ((1,576 − 120) / 2); past 4, measured nowhere.
    #[cfg(target_arch = "x86_64")]
    const VCPUS: [u64; 2] = [120, 728];
    // On the path a run takes (M179): what the guest keeps, its VM's KiB less what may be
    // allocated (MemAvailable, and the free pages its CPU's list holds, 4 to 9 MiB here),
    // each size's most of 6 boots of its template (M179's 4, M183's 2), and of the sizes
    // between it and the last below, on an Apple M5 Max, guest kernel
    // kernel-6.18.48-4b8900bdffdd; the boots of a size differed by 156 KiB at most.
    #[cfg(not(target_arch = "x86_64"))]
    const MEASURED: [(u64, u64); 29] = [
        (256, 40_104),
        (320, 43_524),
        (384, 43_988),
        (448, 46_464),
        (512, 46_888),
        (640, 80_540),
        (768, 82_816),
        (896, 85_108),
        (1024, 87_456),
        (1280, 96_456),
        (1536, 101_088),
        (1792, 105_684),
        (2048, 110_292),
        (2560, 224_416),
        (3072, 235_648),
        (3328, 241_268),
        (3584, 246_896),
        (3840, 252_520),
        (4096, 258_132),
        (4352, 276_780),
        (4608, 282_404),
        (5376, 299_288),
        (6144, 316_160),
        (7168, 338_656),
        (8192, 361_160),
        (10_240, 423_968),
        (12_288, 470_904),
        (14_336, 519_972),
        (16_384, 569_116),
    ];
    /// KiB a MiB past the last size: (569,112 − 361,160) / 8,192 is 25.4.
    #[cfg(not(target_arch = "x86_64"))]
    const SLOPE: u64 = 26;
    /// KiB the second vCPU keeps, and each past it (M183): the most the second kept at any
    /// size (2 boots of each of 29 sizes to 16 GiB), and the most of what each past it kept
    /// on average, from 4, 8 and 18 vCPUs ((20,024 − 1,900) / 16 at 18).
    #[cfg(not(target_arch = "x86_64"))]
    const VCPUS: [u64; 2] = [1900, 1133];
    let past = |&(size, kib): &(u64, u64)| kib.saturating_add(mib.saturating_sub(size).saturating_mul(SLOPE));
    let one = match MEASURED.iter().find(|(size, _)| *size >= mib) {
        Some(&(_, kib)) => kib,
        None => MEASURED.last().map_or(0, past),
    };
    let [second, each] = VCPUS;
    let more = match vcpus {
        0 | 1 => 0,
        n => second.saturating_add(u64::from(n - 2).saturating_mul(each)),
    };
    one.saturating_add(more)
}
