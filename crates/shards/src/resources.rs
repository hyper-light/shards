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

/// The vCPUs every run's microVM has, whatever its run is given: the host's, all online, so
/// that one template serves every `--cpus` and an update gives a running workload more or
/// fewer of them (D191). Bringing a CPU online as a run starts cost 13 to 45 ms where a
/// microVM had the host's (PM M265), past what a run's start has; a run is held to what it
/// is given by its cgroup ([`cgroup`]).
pub fn microvm_vcpus() -> u32 {
    u32::try_from(host_cpus()).unwrap_or(u32::MAX)
}

/// What a run's limits may ask (D155, D159), checked as it is made, in the order moby's
/// verifyPlatformContainerResources checks them, and refused in shards' words: an error,
/// or what to warn of. Its microVM's kernel is the run's own, so what a host shared by
/// containers cannot give one, it gives: swappiness, the microVM's own, and the OOM
/// killer kept off, the limit a throttle (`cgroup`). `--cpuset-mems` names the guest's
/// memory nodes (one, node 0). `update` checks what an update asks for, which may give
/// swap alone.
pub fn verify(r: &Resources, cpus: usize, update: bool) -> Result<Vec<String>, String> {
    let warnings = Vec::new();
    let mib = |bytes: i64| bytes / (1 << 20);
    if r.memory != 0 && r.memory < MIN_MEMORY {
        return Err(format!(
            "--memory {}: a run's memory is at least {} MiB",
            r.memory,
            mib(MIN_MEMORY)
        ));
    }
    if r.memory > 0 && r.memory_swap > 0 && r.memory_swap < r.memory {
        return Err(format!(
            "--memory-swap {} is less than --memory {}: it is the memory and the swap together",
            r.memory_swap, r.memory
        ));
    }
    if r.memory == 0 && r.memory_swap > 0 && !update {
        return Err(
            "--memory-swap without --memory: swap is what a run may use past its memory: give --memory too"
                .into(),
        );
    }
    if let Some(s) = r.memory_swappiness
        && !(0..=100).contains(&s)
    {
        return Err(format!("--memory-swappiness {s}: from 0 to 100"));
    }
    if r.memory_reservation > 0 && r.memory_reservation < MIN_MEMORY {
        return Err(format!(
            "--memory-reservation {}: at least {} MiB",
            r.memory_reservation,
            mib(MIN_MEMORY)
        ));
    }
    if r.memory > 0 && r.memory_reservation > 0 && r.memory < r.memory_reservation {
        return Err(format!(
            "--memory {} is less than --memory-reservation {}",
            r.memory, r.memory_reservation
        ));
    }
    if r.nano_cpus > 0 && r.cpu_period > 0 {
        return Err("--cpus and --cpu-period each set the CPU period: give one".into());
    }
    if r.nano_cpus > 0 && r.cpu_quota > 0 {
        return Err("--cpus and --cpu-quota each cap the CPU time: give one".into());
    }
    let count = i64::try_from(cpus).unwrap_or(i64::MAX);
    if r.nano_cpus != 0 && (r.nano_cpus < 0 || r.nano_cpus > count.saturating_mul(1_000_000_000)) {
        return Err(format!(
            "--cpus {}.{:02}: from 0.01 to {cpus}, the host's CPUs",
            r.nano_cpus / 1_000_000_000,
            (r.nano_cpus % 1_000_000_000).abs() / 10_000_000
        ));
    }
    if r.cpu_period != 0 && !(1000..=1_000_000).contains(&r.cpu_period) {
        return Err(format!(
            "--cpu-period {}: from 1000 to 1000000 microseconds",
            r.cpu_period
        ));
    }
    if r.cpu_quota > 0 && r.cpu_quota < 1000 {
        return Err(format!(
            "--cpu-quota {}: at least 1000 microseconds a period",
            r.cpu_quota
        ));
    }
    microvm_cpus(r, cpus)?;
    let available = |list: &str, size: usize, what: &str, words: &str, have: &str| {
        // isCpusetListAvailable: at least 8,192 may be named, so that a list is not
        // read into an excessive set (CVE-2018-20699).
        let named =
            parse_uint_list(list, 8192.max(size)).map_err(|e| format!("--cpuset-{what} {list}: {e}"))?;
        if named.iter().any(|&n| n >= size) {
            return Err(format!(
                "--cpuset-{what} {list}: the microVM's {words} are {have}"
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
        return Err(format!("--cpu-shares {}: not below 0", r.cpu_shares));
    }
    // A weight in range; the guest kernel has BFQ and io.cost, so it is kept.
    if r.blkio_weight > 0 && !(10..=1000).contains(&r.blkio_weight) {
        return Err(format!("--blkio-weight {}: from 10 to 1000", r.blkio_weight));
    }
    Ok(warnings)
}

/// What a microVM's own CPU flags may ask (D155), in shards' words: from 1 vCPU to as many
/// as the host has CPUs, and only CPUs it has for `--cpuset-cpus`; a share of the host's CPU
/// time from 1 to 100 percent, one cap of its CPU time at most; and a real-time budget the
/// guest kernel takes, its runtime within its period where both are given (what the guest
/// keeps of either, its kernel's own, init checks as the run starts).
fn microvm_cpus(r: &Resources, cpus: usize) -> Result<(), String> {
    let host = i64::try_from(cpus).unwrap_or(i64::MAX);
    if !(0..=host).contains(&r.cpu_count) {
        return Err(format!(
            "--cpu-count {}: a microVM has from 1 vCPU to as many as its host has CPUs, {cpus}",
            r.cpu_count
        ));
    }
    if r.cpu_count > 0
        && let Ok(named) = parse_uint_list(&r.cpuset_cpus, cpus)
        && let Some(&highest) = named.iter().max()
        && i64::try_from(highest).unwrap_or(i64::MAX) >= r.cpu_count
    {
        return Err(format!(
            "--cpuset-cpus {}: a microVM of {} vCPUs has CPUs 0 to {}",
            r.cpuset_cpus,
            r.cpu_count,
            r.cpu_count - 1
        ));
    }
    if !(0..=100).contains(&r.cpu_percent) {
        return Err(format!(
            "--cpu-percent {}: a share of the host's CPU time is from 1 to 100 percent",
            r.cpu_percent
        ));
    }
    if r.cpu_percent > 0 {
        for (given, flag) in [(r.nano_cpus, "--cpus"), (r.cpu_quota, "--cpu-quota")] {
            if given != 0 {
                return Err(format!(
                    "--cpu-percent and {flag} each cap the microVM's CPU time: give one"
                ));
            }
        }
    }
    // The kernel's ranges (kernel/sched/rt.c, sched_rt_period_us and sched_rt_runtime_us):
    // a period of 1 microsecond or more, a runtime of 0 or more, each an int's. No runtime
    // without a limit (-1): the guest's init and the run's agents share its CPUs.
    let most = i64::from(i32::MAX);
    if !(0..=most).contains(&r.cpu_realtime_period) {
        return Err(format!(
            "--cpu-rt-period {}: a real-time period is from 1 to {most} microseconds",
            r.cpu_realtime_period
        ));
    }
    if !(0..=most).contains(&r.cpu_realtime_runtime) {
        return Err(format!(
            "--cpu-rt-runtime {}: real-time time is from 0 to {most} microseconds a period",
            r.cpu_realtime_runtime
        ));
    }
    if r.cpu_realtime_period > 0 && r.cpu_realtime_runtime > r.cpu_realtime_period {
        return Err(format!(
            "--cpu-rt-runtime {} is more than its period, --cpu-rt-period {}",
            r.cpu_realtime_runtime, r.cpu_realtime_period
        ));
    }
    Ok(())
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

/// The swap a run's limits give its microVM (D159), in bytes, whole pages: what
/// `--memory-swap` allows past `--memory`, as much again by default, as a host's container
/// has. Unlimited (-1), as much again too, the cgroup holding it to none less: a device as
/// large as the host's memory cost the guest's kernel about three bytes a page of it
/// (swap_map, and swap_cgroup's two), 108 MiB of a 226 MiB guest for 128 GiB, and its
/// workload was killed under that pressure (PM M229), so a run's swap that grows as it
/// fills is what unlimited is to become. None without a memory limit, where the microVM
/// grows to the host's memory, which its host swaps as it would any process's; or where
/// it is less than a swap area's header page and one page to swap to (mm/swapfile.c,
/// read_swap_header).
pub fn swap_bytes(r: &Resources) -> Option<u64> {
    const PAGE: u64 = 4096;
    if r.memory <= 0 {
        return None;
    }
    let total = memory_swap(r);
    let bytes = if total == -1 {
        u64::try_from(r.memory).ok()?
    } else {
        u64::try_from(total.checked_sub(r.memory)?).ok()?
    };
    let bytes = bytes / PAGE * PAGE;
    (bytes >= 2 * PAGE).then_some(bytes)
}

/// The size of the swap device that holds `allowance` for a microVM of `vcpus` (D159): the
/// allowance, which the workload's cgroup holds it to (`memory.swap.max`), and room for the
/// cluster each vCPU may keep partly used for each order of folio it swaps out
/// (mm/swapfile.c, percpu_swap_cluster: SWAP_NR_ORDERS each, PMD_ORDER + 1 with THP swap,
/// which the guest kernel has; a cluster HPAGE_PMD_NR pages, 2 MiB), without which the
/// device ran out of clusters with the allowance half used (`memory.swap.events` fail, PM
/// M229); in whole 2 MiB, as the swap window maps it (vmm `pmem::ALIGN`). The file is
/// sparse: the room costs the host's disk nothing until written.
pub fn swap_device_bytes(allowance: u64, vcpus: u32) -> u64 {
    const ORDERS: u64 = 10;
    const CLUSTER: u64 = 2 << 20;
    let bytes = allowance.saturating_add(u64::from(vcpus).saturating_mul(ORDERS).saturating_mul(CLUSTER));
    bytes
        .checked_next_multiple_of(CLUSTER)
        .unwrap_or(u64::MAX / CLUSTER * CLUSTER)
}

/// What a swap device of `device` bytes costs its guest's kernel, in bytes (D159): its
/// pages' swap_map byte and swap_cgroup's two, its zero map's bit and its clusters' records
/// (mm/swapfile.c, mm/swap_cgroup.c), 0.80 to 0.81 KiB a MiB of device measured past a
/// fixed part, 0.98 KiB a MiB in all for the smallest (VmallocUsed, PM M229): 128 KiB, and
/// 13 bytes for each 16 KiB.
pub fn swap_kernel_bytes(device: u64) -> u64 {
    (128 << 10) + device / 16384 * 13
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

/// The CPU time `r` allows per period, as cpu.max's quota and period, if it limits it, of
/// a host of `cpus` CPUs.
fn cpu_max(r: &Resources, cpus: usize) -> Option<(i64, i64)> {
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
    let period = if period == 0 { CPU_PERIOD } else { period };
    // `--cpu-percent` (D155): that share of all the host's CPUs' time in each period.
    if r.cpu_percent > 0 {
        let host = i64::try_from(cpus).unwrap_or(i64::MAX);
        return Some((
            r.cpu_percent.saturating_mul(host).saturating_mul(period) / 100,
            period,
        ));
    }
    (quota != 0 || r.nano_cpus > 0 || r.cpu_period != 0).then_some((quota, period))
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

/// The workload's cgroup settings, `FILE=VALUE`, in the order runc's fs2 Manager.Set
/// writes them: pids, memory, io, cpu, cpuset; of a host of `cpus` CPUs.
pub fn cgroup(r: &Resources, cpus: usize) -> Vec<Vec<u8>> {
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
        // `--oom-kill-disable` (D159): the limit a throttle, never the OOM killer's: past
        // it the workload is put under reclaim and slowed, as cgroup v1's OOM control held
        // a container's tasks, where cgroup v2 has no such control.
        if memory > 0 && r.oom_kill_disable {
            out.push(format!("memory.high={memory}"));
        } else if memory > 0 {
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
    if let Some((quota, period)) = cpu_max(r, cpus) {
        let quota = if quota > 0 {
            quota.to_string()
        } else {
            "max".into()
        };
        out.push(format!("cpu.max={quota} {period}"));
    }
    // The CPUs it runs on: `--cpuset-cpus`'s; else the first of its microVM's as many as it
    // is given ([`run_cpus`]), its microVM having every one of the host's (D191), so that
    // it sees and runs on what it was given as a microVM of its CPUs alone did; all of them
    // named, for an update that gives it more to undo one that gave it fewer.
    if !r.cpuset_cpus.is_empty() {
        out.push(format!("cpuset.cpus={}", r.cpuset_cpus));
    } else {
        out.push(match run_cpus(r, cpus) {
            0 | 1 => "cpuset.cpus=0".to_string(),
            n => format!("cpuset.cpus=0-{}", n - 1),
        });
    }
    if !r.cpuset_mems.is_empty() {
        out.push(format!("cpuset.mems={}", r.cpuset_mems));
    }
    out.into_iter().map(String::into_bytes).collect()
}

/// The CPUs a run's workload is given, of a host of `cpus`: `--cpu-count`'s (D155); else
/// as many as `--cpus`, `--cpu-percent` or the quota uses, and every CPU `--cpuset-cpus`
/// names; with none of them, every CPU of the host's, as a container sees and uses every
/// one (D125); one at least, and no more than the host's. Its microVM has every one of the
/// host's ([`microvm_vcpus`]); its cgroup's cpuset names these ([`cgroup`]).
pub fn run_cpus(r: &Resources, cpus: usize) -> u32 {
    let host = i64::try_from(cpus).unwrap_or(i64::MAX);
    if r.cpu_count > 0 {
        return u32::try_from(r.cpu_count.min(host)).unwrap_or(1);
    }
    let quota = cpu_max(r, cpus).filter(|&(quota, period)| quota > 0 && period > 0);
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

/// The CPUs whose time a run's microVM may use of a host of `cpus` together, the host
/// holding its vCPUs to it (D191): as many as its workload's limits let it use at once,
/// whole, the fewest of `--cpu-count`'s, its quota's rounded up and the CPUs
/// `--cpuset-cpus` names; 0, none held, where that is every CPU of the host's. Its
/// workload is held inside its microVM to what it is given ([`cgroup`]); this holds the
/// rest of its guest, kernel threads and interrupts, and a workload past its cgroup (a
/// guest's root may leave it), to what a microVM of those CPUs alone could use.
pub fn held_cpus(r: &Resources, cpus: usize) -> u32 {
    let host = i64::try_from(cpus.max(1)).unwrap_or(i64::MAX);
    let mut most = host;
    if r.cpu_count > 0 {
        most = most.min(r.cpu_count);
    }
    if let Some((quota, period)) = cpu_max(r, cpus).filter(|&(quota, period)| quota > 0 && period > 0) {
        most = most.min(quota / period + i64::from(quota % period != 0));
    }
    if let Ok(mut named) = parse_uint_list(&r.cpuset_cpus, cpus)
        && !named.is_empty()
    {
        named.sort_unstable();
        named.dedup();
        most = most.min(i64::try_from(named.len()).unwrap_or(i64::MAX));
    }
    if most >= host {
        0
    } else {
        u32::try_from(most.max(1)).unwrap_or(u32::MAX)
    }
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

    /// What a run's limits may ask, on a host of 18 CPUs, refused in shards' words (D158):
    /// swappiness and the OOM killer kept off are the microVM's own (D159).
    #[test]
    fn resources_are_checked() {
        let swappy = Resources {
            memory_swappiness: Some(50),
            oom_kill_disable: true,
            ..r()
        };
        assert_eq!(verify(&swappy, 18, false), Ok(Vec::new()));
        let refused = |r: Resources| verify(&r, 18, false).unwrap_err();
        assert_eq!(
            refused(Resources {
                memory_swappiness: Some(101),
                ..r()
            }),
            "--memory-swappiness 101: from 0 to 100"
        );
        assert_eq!(
            refused(Resources {
                nano_cpus: 99_000_000_000,
                ..r()
            }),
            "--cpus 99.00: from 0.01 to 18, the host's CPUs"
        );
        assert_eq!(
            refused(Resources {
                cpuset_cpus: "99".into(),
                ..r()
            }),
            "--cpuset-cpus 99: the microVM's CPUs are 0-17"
        );
        assert_eq!(
            refused(Resources {
                cpuset_cpus: "1-0".into(),
                ..r()
            }),
            "--cpuset-cpus 1-0: invalid format: 1-0"
        );
        assert_eq!(
            refused(Resources {
                memory: 4 << 20,
                ..r()
            }),
            "--memory 4194304: a run's memory is at least 6 MiB"
        );
        assert_eq!(
            refused(Resources {
                memory_swap: 4 << 30,
                ..r()
            }),
            "--memory-swap without --memory: swap is what a run may use past its memory: give --memory too"
        );
        assert_eq!(
            refused(Resources {
                memory: 64 << 20,
                memory_swap: 32 << 20,
                ..r()
            }),
            "--memory-swap 33554432 is less than --memory 67108864: it is the memory and the swap together"
        );
        assert_eq!(
            refused(Resources {
                cpuset_mems: "1".into(),
                ..r()
            }),
            "--cpuset-mems 1: the microVM's memory nodes are 0"
        );
        assert_eq!(
            refused(Resources {
                cpuset_cpus: "9000".into(),
                ..r()
            }),
            "--cpuset-cpus 9000: value of out range, maximum is 8192"
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
    fn workload_limits_are_written() {
        let text = |r: &Resources| -> Vec<String> {
            cgroup(r, 8)
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
        assert_eq!(
            text(&unswapped),
            ["memory.max=33554432", "cpu.max=25000 50000", "cpuset.cpus=0"]
        );
        assert_eq!(
            text(&Resources {
                cpu_period: 50_000,
                ..r()
            }),
            ["cpu.max=max 50000", "cpuset.cpus=0-7"]
        );
        // Every CPU of its microVM's, which has all the host's, named: an update that gives
        // a run fewer is undone by one that gives it them all (D191).
        assert_eq!(text(&r()), ["cpuset.cpus=0-7"]);
        // `--cpu-percent` (D155): that share of all 8 of the host's CPUs' time, on as many.
        assert_eq!(
            text(&Resources {
                cpu_percent: 50,
                ..r()
            }),
            ["cpu.max=400000 100000", "cpuset.cpus=0-3"]
        );
        // `--cpu-count`'s CPUs, the first of its microVM's.
        assert_eq!(text(&Resources { cpu_count: 3, ..r() }), ["cpuset.cpus=0-2"]);
        // ConvertCPUSharesToCgroupV2Value's fixed points, and its default.
        assert_eq!(
            (cpu_weight(2), cpu_weight(1024), cpu_weight(262_144)),
            (1, 100, 10_000)
        );
    }

    /// A microVM's own CPU flags, as shards checks them (D155): vCPUs from 1 to the host's
    /// CPUs and only CPUs it has named; a share of the host's CPU time from 1 to 100
    /// percent, one cap at most; a real-time budget the guest kernel takes.
    #[test]
    fn a_microvms_own_cpu_flags_are_checked() {
        let checked = |r: Resources| verify(&r, 8, false);
        assert!(checked(Resources { cpu_count: 8, ..r() }).is_ok());
        assert_eq!(
            checked(Resources { cpu_count: 9, ..r() }),
            Err("--cpu-count 9: a microVM has from 1 vCPU to as many as its host has CPUs, 8".into())
        );
        assert_eq!(
            checked(Resources { cpu_count: -1, ..r() }),
            Err("--cpu-count -1: a microVM has from 1 vCPU to as many as its host has CPUs, 8".into())
        );
        assert!(
            checked(Resources {
                cpu_count: 2,
                cpuset_cpus: "0-1".into(),
                ..r()
            })
            .is_ok()
        );
        assert_eq!(
            checked(Resources {
                cpu_count: 2,
                cpuset_cpus: "1,3".into(),
                ..r()
            }),
            Err("--cpuset-cpus 1,3: a microVM of 2 vCPUs has CPUs 0 to 1".into())
        );
        assert!(
            checked(Resources {
                cpu_percent: 100,
                ..r()
            })
            .is_ok()
        );
        assert_eq!(
            checked(Resources {
                cpu_percent: 101,
                ..r()
            }),
            Err("--cpu-percent 101: a share of the host's CPU time is from 1 to 100 percent".into())
        );
        assert_eq!(
            checked(Resources {
                cpu_percent: 50,
                nano_cpus: 1_000_000_000,
                ..r()
            }),
            Err("--cpu-percent and --cpus each cap the microVM's CPU time: give one".into())
        );
        assert_eq!(
            checked(Resources {
                cpu_percent: 50,
                cpu_quota: 50_000,
                ..r()
            }),
            Err("--cpu-percent and --cpu-quota each cap the microVM's CPU time: give one".into())
        );
        assert!(
            checked(Resources {
                cpu_realtime_period: 100_000,
                cpu_realtime_runtime: 50_000,
                ..r()
            })
            .is_ok()
        );
        assert_eq!(
            checked(Resources {
                cpu_realtime_period: 100_000,
                cpu_realtime_runtime: 150_000,
                ..r()
            }),
            Err("--cpu-rt-runtime 150000 is more than its period, --cpu-rt-period 100000".into())
        );
        assert_eq!(
            checked(Resources {
                cpu_realtime_runtime: -1,
                ..r()
            }),
            Err("--cpu-rt-runtime -1: real-time time is from 0 to 2147483647 microseconds a period".into())
        );
        assert_eq!(
            checked(Resources {
                cpu_realtime_period: 1 << 31,
                ..r()
            }),
            Err("--cpu-rt-period 2147483648: a real-time period is from 1 to 2147483647 microseconds".into())
        );
    }

    /// The CPUs a run's microVM is held to by its host (D191): the fewest its limits let
    /// its workload use at once, whole; none where that is every one.
    #[test]
    fn microvms_are_held_to_their_runs_cpus() {
        let host = 8;
        assert_eq!(held_cpus(&r(), host), 0, "no limit, no hold");
        let nano = |nano_cpus| Resources { nano_cpus, ..r() };
        assert_eq!(held_cpus(&nano(1_000_000_000), host), 1);
        assert_eq!(held_cpus(&nano(1_500_000_000), host), 2);
        assert_eq!(held_cpus(&nano(500_000_000), host), 1);
        assert_eq!(held_cpus(&nano(8_000_000_000), host), 0, "every CPU");
        let quota = Resources {
            cpu_quota: 250_000,
            cpu_period: 100_000,
            ..r()
        };
        assert_eq!(held_cpus(&quota, host), 3);
        // An unlimited quota, as Docker takes -1, and a period alone, hold nothing.
        let unlimited = Resources { cpu_quota: -1, ..r() };
        assert_eq!(held_cpus(&unlimited, host), 0);
        let period = Resources {
            cpu_period: 50_000,
            ..r()
        };
        assert_eq!(held_cpus(&period, host), 0);
        // The CPUs a cpuset names, however it names them, not its highest's.
        let set = |cpuset_cpus: &str| Resources {
            cpuset_cpus: cpuset_cpus.into(),
            ..r()
        };
        assert_eq!(held_cpus(&set("3"), host), 1);
        assert_eq!(held_cpus(&set("0,0-1"), host), 2);
        assert_eq!(held_cpus(&set("0-7"), host), 0);
        let count = Resources { cpu_count: 3, ..r() };
        assert_eq!(held_cpus(&count, host), 3);
        let percent = Resources {
            cpu_percent: 50,
            ..r()
        };
        assert_eq!(held_cpus(&percent, host), 4);
        // Both a quota and a cpuset: the fewer.
        let both = Resources {
            nano_cpus: 4_000_000_000,
            cpuset_cpus: "0-1".into(),
            ..r()
        };
        assert_eq!(held_cpus(&both, host), 2);
    }

    #[test]
    fn microvms_are_sized_for_their_limits() {
        // No limit: every CPU of the host's, as Docker's container has (D125).
        assert_eq!(run_cpus(&r(), 8), 8);
        assert_eq!(
            run_cpus(
                &Resources {
                    nano_cpus: 500_000_000,
                    ..r()
                },
                8
            ),
            1
        );
        assert_eq!(
            run_cpus(
                &Resources {
                    nano_cpus: 1_500_000_000,
                    ..r()
                },
                8
            ),
            2
        );
        assert_eq!(
            run_cpus(
                &Resources {
                    cpuset_cpus: "3".into(),
                    ..r()
                },
                8
            ),
            4
        );
        assert_eq!(
            run_cpus(
                &Resources {
                    cpu_quota: 1_000_000,
                    cpu_period: 100_000,
                    ..r()
                },
                8
            ),
            8
        );
        // `--cpu-count`'s vCPUs, whatever else is asked; `--cpu-percent`'s as many as its
        // share of the host's CPU time needs (D155).
        let count = |cpu_count, nano_cpus| Resources {
            cpu_count,
            nano_cpus,
            ..r()
        };
        assert_eq!(run_cpus(&count(3, 0), 8), 3);
        assert_eq!(run_cpus(&count(3, 1_500_000_000), 8), 3);
        assert_eq!(run_cpus(&count(1, 0), 8), 1);
        let percent = |cpu_percent| Resources { cpu_percent, ..r() };
        assert_eq!(run_cpus(&percent(50), 8), 4);
        assert_eq!(run_cpus(&percent(33), 18), 6);
        assert_eq!(run_cpus(&percent(100), 18), 18);
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
