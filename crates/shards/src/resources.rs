//! `run`'s resource flags in a microVM. dockerd checks them (moby docker-v29.8.1
//! daemon/daemon_unix.go, verifyPlatformContainerResources) as it would on a host whose
//! kernel is the guest's: cgroup v2, with every controller runc sets limits through, and
//! swap accounting. The VM is then sized to hold what the workload may use, and shards-init
//! sets the limits on the workload's cgroup as runc v1.5.1 does (opencontainers/cgroups
//! v0.0.6 fs2), after moby's translation to the OCI spec (getMemoryResources,
//! getCPUResources, getPidsLimit) and runc's to its own (libcontainer/specconv).
// Only Unix has the daemon, so far.
#![cfg_attr(not(unix), allow(dead_code))]

use shards_ipc::Resources;

/// dockerd's least memory limit and reservation: 6 MiB (linuxMinMemory).
const MIN_MEMORY: i64 = 6_291_456;

/// The CFS period dockerd gives `--cpus` (getCPUResources), and the kernel's default
/// (Documentation/scheduler/sched-bwc.rst), in microseconds.
const CPU_PERIOD: i64 = 100_000;

/// The CPUs a microVM may have, numbered from 0, as dockerd's `runtime.NumCPU()` and
/// `cpuset.cpus.effective` count the host's.
pub fn host_cpus() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// What dockerd says of `r` as it makes a container: an error, or its warnings. The
/// checks and their words are verifyPlatformContainerResources', in its order, for a
/// cgroup v2 kernel: swappiness and OomKillDisable are not v2's, and are discarded with
/// its warnings. One difference: `--cpuset-mems` is checked against the guest's memory
/// nodes (one, node 0); dockerd checks it against the host's CPUs
/// (pkg/sysinfo/cgroup2_linux.go parses `info.Cpus` into `MemSets`), and runc then fails
/// to write a node that is not there as the container starts.
/// `update` checks what an update asks for, which may give swap alone (dockerd's
/// update flag).
pub fn verify(r: &Resources, cpus: usize, update: bool) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();
    if r.memory != 0 && r.memory < MIN_MEMORY {
        return Err("Minimum memory limit allowed is 6MB".into());
    }
    if r.memory > 0 && r.memory_swap > 0 && r.memory_swap < r.memory {
        return Err("Minimum memoryswap limit should be larger than memory limit, see usage".into());
    }
    if r.memory == 0 && r.memory_swap > 0 && !update {
        return Err("You should always set the Memory limit when using Memoryswap limit, see usage".into());
    }
    if r.memory_swappiness.is_some() {
        warnings.push(
            "Your kernel does not support memory swappiness capabilities or the cgroup is not mounted. Memory swappiness discarded."
                .into(),
        );
    }
    if r.memory_reservation > 0 && r.memory_reservation < MIN_MEMORY {
        return Err("Minimum memory reservation allowed is 6MB".into());
    }
    if r.memory > 0 && r.memory_reservation > 0 && r.memory < r.memory_reservation {
        return Err("Minimum memory limit can not be less than memory reservation limit, see usage".into());
    }
    if r.oom_kill_disable {
        warnings.push("Your kernel does not support OomKillDisable. OomKillDisable discarded.".into());
    }
    if r.nano_cpus > 0 && r.cpu_period > 0 {
        return Err("Conflicting options: Nano CPUs and CPU Period cannot both be set".into());
    }
    if r.nano_cpus > 0 && r.cpu_quota > 0 {
        return Err("Conflicting options: Nano CPUs and CPU Quota cannot both be set".into());
    }
    let count = i64::try_from(cpus).unwrap_or(i64::MAX);
    if r.nano_cpus != 0 && (r.nano_cpus < 0 || r.nano_cpus > count.saturating_mul(1_000_000_000)) {
        return Err(format!(
            "range of CPUs is from 0.01 to {cpus}.00, as there are only {cpus} CPUs available"
        ));
    }
    if r.cpu_period != 0 && !(1000..=1_000_000).contains(&r.cpu_period) {
        return Err(
            "CPU cfs period can not be less than 1ms (i.e. 1000) or larger than 1s (i.e. 1000000)".into(),
        );
    }
    if r.cpu_quota > 0 && r.cpu_quota < 1000 {
        return Err("CPU cfs quota can not be less than 1ms (i.e. 1000)".into());
    }
    let available = |list: &str, size: usize, what: &str, words: &str, have: &str| {
        // isCpusetListAvailable: at least 8,192 may be named, so that a list is not
        // read into an excessive set (CVE-2018-20699).
        let named = parse_uint_list(list, 8192.max(size))
            .map_err(|e| format!("Invalid value {list} for cpuset {what}: {e}"))?;
        if named.iter().any(|&n| n >= size) {
            return Err(format!(
                "Requested {words} are not available - requested {list}, available: {have}"
            ));
        }
        Ok(())
    };
    let have = if cpus > 1 {
        format!("0-{}", cpus - 1)
    } else {
        "0".to_string()
    };
    available(&r.cpuset_cpus, cpus, "cpus", "CPUs", &have)?;
    available(&r.cpuset_mems, 1, "mems", "memory nodes", "0")?;
    if r.cpu_shares < 0 {
        return Err(format!(
            "invalid CPU shares ({}): value must be a positive integer",
            r.cpu_shares
        ));
    }
    // verifyPlatformContainerResources: a weight in range; the guest kernel has BFQ and
    // io.cost, so it is kept.
    if r.blkio_weight > 0 && !(10..=1000).contains(&r.blkio_weight) {
        return Err("Range of blkio weight is from 10 to 1000".into());
    }
    Ok(warnings)
}

/// moby's parseUintList (pkg/sysinfo/sysinfo_linux.go): `7`, `1-6`, `0,3-4,7`, each
/// number at most `maximum`; the numbers named.
fn parse_uint_list(val: &str, maximum: usize) -> Result<Vec<usize>, String> {
    if val.is_empty() {
        return Ok(Vec::new());
    }
    let invalid = || format!("invalid format: {val}");
    // strconv.Atoi: a sign, then decimal digits.
    let atoi = |s: &str| -> Result<usize, String> {
        let n = shards_cmdline::go::parse_int10(s).map_err(|_| invalid())?;
        usize::try_from(n).map_err(|_| invalid())
    };
    let too_big = || format!("value of out range, maximum is {maximum}");
    let mut named = Vec::new();
    for r in val.split(',') {
        match r.split_once('-') {
            None => {
                let v = atoi(r)?;
                if v > maximum {
                    return Err(too_big());
                }
                named.push(v);
            }
            Some((lo, hi)) => {
                let (lo, hi) = (atoi(lo)?, atoi(hi)?);
                if hi < lo {
                    return Err(invalid());
                }
                if hi > maximum {
                    return Err(too_big());
                }
                named.extend(lo..=hi);
            }
        }
    }
    Ok(named)
}

/// MemorySwap as dockerd keeps it (adaptContainerSettings): twice the memory, where a
/// memory limit is set and swap is not.
pub fn memory_swap(r: &Resources) -> i64 {
    if r.memory > 0 && r.memory_swap == 0 {
        r.memory.saturating_mul(2)
    } else {
        r.memory_swap
    }
}

/// The CPU time `r` allows per period, as cpu.max's quota and period, if it limits it.
fn cpu_max(r: &Resources) -> Option<(i64, i64)> {
    let (mut quota, mut period) = (0, 0);
    if r.nano_cpus > 0 {
        period = CPU_PERIOD;
        quota = r.nano_cpus.saturating_mul(CPU_PERIOD) / 1_000_000_000;
    }
    if r.cpu_period != 0 {
        period = r.cpu_period;
    }
    if r.cpu_quota != 0 {
        quota = r.cpu_quota;
    }
    (quota != 0 || period != 0).then_some((quota, if period == 0 { CPU_PERIOD } else { period }))
}

/// opencontainers/cgroups ConvertCPUSharesToCgroupV2Value: shares (2 to 262,144, 1,024
/// the default) to a weight (1 to 10,000, 100 the default), along the quadratic that
/// meets all three.
fn cpu_weight(shares: u64) -> u64 {
    if shares == 0 {
        return 0;
    }
    if shares <= 2 {
        return 1;
    }
    if shares >= 262_144 {
        return 10_000;
    }
    #[allow(clippy::cast_precision_loss)]
    let l = (shares as f64).log2();
    let exponent = (l * l + 125.0 * l) / 612.0 - 7.0 / 34.0;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let w = 10f64.powf(exponent).ceil() as u64;
    w
}

/// The workload's cgroup settings, `FILE=VALUE`, as runc's fs2 Manager.Set writes them,
/// in its order: pids, memory, io, cpu, cpuset.
pub fn cgroup(r: &Resources) -> Vec<Vec<u8>> {
    let mut out: Vec<String> = Vec::new();
    // getPidsLimit: none for 0 or less, which the API takes as unset.
    if r.pids_limit > 0 {
        out.push(format!("pids.max={}", r.pids_limit));
    }
    // setMemory, after getMemoryResources, which passes swap only above 0.
    let swap = memory_swap(r);
    if r.memory != 0 || r.memory_reservation != 0 || swap > 0 {
        let memory = r.memory.max(0);
        if swap > 0 {
            // ConvertMemorySwapToCgroupV2Value: swap alone, beside memory.
            let only = swap.saturating_sub(memory);
            out.push(format!("memory.swap.max={only}"));
        }
        if memory > 0 {
            out.push(format!("memory.max={memory}"));
        }
        if r.memory_reservation > 0 {
            out.push(format!("memory.low={}", r.memory_reservation));
        }
    }
    // setIo: BFQ's weight as given, where the guest has BFQ; init writes io.weight's
    // scale where it has io.cost alone.
    if r.blkio_weight != 0 {
        out.push(format!("io.bfq.weight={}", r.blkio_weight));
    }
    let weight = cpu_weight(u64::try_from(r.cpu_shares).unwrap_or(0));
    if weight != 0 {
        out.push(format!("cpu.weight={weight}"));
    }
    if let Some((quota, period)) = cpu_max(r) {
        let quota = if quota > 0 {
            quota.to_string()
        } else {
            "max".into()
        };
        out.push(format!("cpu.max={quota} {period}"));
    }
    if !r.cpuset_cpus.is_empty() {
        out.push(format!("cpuset.cpus={}", r.cpuset_cpus));
    }
    if !r.cpuset_mems.is_empty() {
        out.push(format!("cpuset.mems={}", r.cpuset_mems));
    }
    out.into_iter().map(String::into_bytes).collect()
}

/// The vCPUs a microVM has for `r`, of a host of `cpus`: as many as `--cpus` or the quota
/// uses, and every CPU `--cpuset-cpus` names; with none of them, every CPU of the host's,
/// as a Docker container sees and uses every one (D125); one at least, and no more than the
/// host's.
pub fn vcpus(r: &Resources, cpus: usize) -> u32 {
    let quota = cpu_max(r).filter(|&(quota, period)| quota > 0 && period > 0);
    let mut need: i64 = if quota.is_none() && r.cpuset_cpus.is_empty() {
        i64::try_from(cpus).unwrap_or(i64::MAX)
    } else {
        1
    };
    if let Some((quota, period)) = quota {
        // Both positive: the quotient rounded up.
        need = need.max(quota / period + i64::from(quota % period != 0));
    }
    if let Ok(named) = parse_uint_list(&r.cpuset_cpus, cpus) {
        let highest = named.iter().max().map_or(0, |&n| n + 1);
        need = need.max(i64::try_from(highest).unwrap_or(i64::MAX));
    }
    let need = need.min(i64::try_from(cpus).unwrap_or(i64::MAX));
    u32::try_from(need).unwrap_or(1)
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
fn overhead_kib(mib: u64, vcpus: u32) -> u64 {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn r() -> Resources {
        Resources::default()
    }

    /// dockerd 29.3.1's answers, on a cgroup v2 host of 18 CPUs.
    #[test]
    fn resources_are_checked_as_dockerd_checks_them() {
        let swappy = Resources {
            memory_swappiness: Some(50),
            oom_kill_disable: true,
            ..r()
        };
        assert_eq!(
            verify(&swappy, 18, false),
            Ok(vec![
                "Your kernel does not support memory swappiness capabilities or the cgroup is not mounted. Memory swappiness discarded.".to_string(),
                "Your kernel does not support OomKillDisable. OomKillDisable discarded.".to_string(),
            ])
        );
        let refused = |r: Resources| verify(&r, 18, false).unwrap_err();
        assert_eq!(
            refused(Resources {
                nano_cpus: 99_000_000_000,
                ..r()
            }),
            "range of CPUs is from 0.01 to 18.00, as there are only 18 CPUs available"
        );
        assert_eq!(
            refused(Resources {
                cpuset_cpus: "99".into(),
                ..r()
            }),
            "Requested CPUs are not available - requested 99, available: 0-17"
        );
        assert_eq!(
            refused(Resources {
                cpuset_cpus: "1-0".into(),
                ..r()
            }),
            "Invalid value 1-0 for cpuset cpus: invalid format: 1-0"
        );
        assert_eq!(
            refused(Resources {
                memory: 4 << 20,
                ..r()
            }),
            "Minimum memory limit allowed is 6MB"
        );
        assert_eq!(
            refused(Resources {
                memory_swap: 4 << 30,
                ..r()
            }),
            "You should always set the Memory limit when using Memoryswap limit, see usage"
        );
        assert_eq!(
            refused(Resources {
                cpuset_mems: "1".into(),
                ..r()
            }),
            "Requested memory nodes are not available - requested 1, available: 0"
        );
        assert_eq!(
            refused(Resources {
                cpuset_cpus: "9000".into(),
                ..r()
            }),
            "Invalid value 9000 for cpuset cpus: value of out range, maximum is 8192"
        );
        assert_eq!(
            verify(
                &Resources {
                    cpuset_cpus: "03,1-3".into(),
                    ..r()
                },
                18,
                false
            ),
            Ok(Vec::new())
        );
    }

    /// runc's writes for what moby passes it.
    #[test]
    fn limits_are_written_as_runc_writes_them() {
        let text = |r: &Resources| -> Vec<String> {
            cgroup(r)
                .into_iter()
                .map(|b| String::from_utf8(b).unwrap())
                .collect()
        };
        let all = Resources {
            memory: 64 << 20,
            memory_reservation: 32 << 20,
            nano_cpus: 1_500_000_000,
            cpu_shares: 512,
            cpuset_cpus: "0".into(),
            pids_limit: 50,
            ..r()
        };
        assert_eq!(
            text(&all),
            [
                "pids.max=50",
                "memory.swap.max=67108864",
                "memory.max=67108864",
                "memory.low=33554432",
                "cpu.weight=59",
                "cpu.max=150000 100000",
                "cpuset.cpus=0",
            ]
        );
        let unswapped = Resources {
            memory: 32 << 20,
            memory_swap: -1,
            cpu_period: 50_000,
            cpu_quota: 25_000,
            pids_limit: -1,
            ..r()
        };
        assert_eq!(text(&unswapped), ["memory.max=33554432", "cpu.max=25000 50000"]);
        assert_eq!(
            text(&Resources {
                cpu_period: 50_000,
                ..r()
            }),
            ["cpu.max=max 50000"]
        );
        assert!(text(&r()).is_empty());
        // ConvertCPUSharesToCgroupV2Value's fixed points, and its default.
        assert_eq!(
            (cpu_weight(2), cpu_weight(1024), cpu_weight(262_144)),
            (1, 100, 10_000)
        );
    }

    #[test]
    fn microvms_are_sized_for_their_limits() {
        // No limit: every CPU of the host's, as Docker's container has (D125).
        assert_eq!(vcpus(&r(), 8), 8);
        assert_eq!(
            vcpus(
                &Resources {
                    nano_cpus: 500_000_000,
                    ..r()
                },
                8
            ),
            1
        );
        assert_eq!(
            vcpus(
                &Resources {
                    nano_cpus: 1_500_000_000,
                    ..r()
                },
                8
            ),
            2
        );
        assert_eq!(
            vcpus(
                &Resources {
                    cpuset_cpus: "3".into(),
                    ..r()
                },
                8
            ),
            4
        );
        assert_eq!(
            vcpus(
                &Resources {
                    cpu_quota: 1_000_000,
                    cpu_period: 100_000,
                    ..r()
                },
                8
            ),
            8
        );
        assert_eq!(memory_mib(0, 256, None, 1), 256);
        assert_eq!(memory_mib(1, 256, None, 1), 256);
        // The default, grown by what its vCPUs keep: as much may be allocated in it as in
        // one vCPU's.
        for vcpus in [2, 8, 18, 64] {
            let mib = memory_mib(0, 256, None, vcpus);
            assert_eq!(mib % 2, 0);
            assert!(
                mib * 1024 - overhead_kib(mib, vcpus) >= 256 * 1024 - overhead_kib(256, 1),
                "{vcpus}: {mib}"
            );
        }
        assert_eq!(memory_mib(1 << 50, 256, Some(16 << 30), 1), 16 << 10);
        // Each measured VM holds the limit it is chosen for, and the least that does, as
        // many vCPUs as it has.
        for vcpus in [1, 2, 18] {
            for limit_mib in [64u64, 255, 256, 400, 700, 1000, 2000, 3000, 8000, 20_000] {
                let limit = i64::try_from(limit_mib << 20).unwrap();
                let mib = memory_mib(limit, 256, None, vcpus);
                assert_eq!(mib % 2, 0);
                let holds = |mib: u64| mib * 1024 - overhead_kib(mib, vcpus) >= limit_mib * 1024;
                assert!(holds(mib), "{limit_mib} with {vcpus}");
                let less = mib - 2;
                assert!(
                    less < 256 || !holds(less),
                    "{limit_mib} with {vcpus}: {mib} is not the least"
                );
            }
        }
    }

    /// What the guest keeps never falls as its VM grows, which `memory_mib`'s search
    /// takes for granted: each table's sizes in order, and its slope past them.
    #[test]
    fn the_guests_share_never_falls_as_its_vm_grows() {
        let mut last = 0;
        for mib in (2..40_000).step_by(2) {
            let kept = overhead_kib(mib, 1);
            assert!(kept >= last, "{mib} MiB: {kept} KiB, below {last}");
            last = kept;
        }
    }
}
