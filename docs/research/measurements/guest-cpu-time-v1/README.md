# Guest CPU time: each vCPU's, and its microVM's, in a VM's stats

Implementer for ROOT coordinator 01a148884a4770fdaf0111f3f792a248, 2026-11-11, for Hex (a runtime that runs
fleets of microVMs): report what each vCPU has run of its guest, so that a runtime can work out the VMM's own CPU
as the VM process's CPU time less its guest time. XNU does not split a thread's time between guest and host from
outside. POLICY_READ: origin/main bff3015182ad754274f1048ce393ac12cb6cd91f, docs/development-rules.md sha256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3, role implementer ([POLICY_READ.json](POLICY_READ.json)).
Branch p/guest-cpu-time-v1, base origin/p/integrate-engine-20261111 at 032b63b1c (ROOT's vmm compile fix
fast-forwarded in). ROOT's scope note: every vCPU's time plus the VM's total as their sum; sampled on the vCPU's own
thread where the contract requires it; an idle vCPU reports its last value; a sample never blocks a running vCPU;
keyed by vCPU index, not a fixed array; a departed vCPU stays in a monotone total.

## Sources, by backend

| Backend | Source | Unit, resolution | Thread | Cost to the vCPU |
|---|---|---|---|---|
| HVF (macOS arm64) | `hv_vcpu_get_exec_time` | mach_absolute_time ticks (41.67 ns on Apple silicon), read as of the vCPU's last exit | the owning thread alone | at each exit: one call, two loads (below) |
| KVM (Linux x86_64, arm64) | the vCPU thread's `gtime`, /proc/self/task/TID/stat field 43 | clock ticks: 10 ms steps (USER_HZ 100), charged in whole scheduler ticks (CONFIG_HZ) | any thread | none |
| WHP (Windows x86_64, arm64) | `WHvGetVirtualProcessorCounters`, `WHvProcessorCounterSetRuntime`: `TotalRuntime100ns - HypervisorRuntime100ns` | 100 ns | any thread (no restriction documented) | none |

### HVF

- **Contract.** The macOS 26.4 SDK's arm64 header (its `#ifdef __arm64__` holds lines 10-455),
  `Hypervisor.framework/Headers/hv_vcpu.h:383-392`
  (sha256 5f5b8060fb21a70e800291dbd1069f7405c6354c93fab884475329ce136a4094), says: "Returns the cumulative execution
  time of a vCPU in units of mach_absolute_time()" and "Must be called by the owning thread". `hv.h:433-442` says
  "in nanoseconds", but that is Intel's API, inside the header's `#ifdef __x86_64__` (lines 11-988). The 26.5 SDK in
  Xcode declares the same.
- **The framework enforces it.** `dyld_info -disassemble` of the framework in macOS 26.4.1 (25E253), dylib UUID
  DE03F9CF-CA32-31F3-AC7B-607F2C78C0E7, gives:
  - `_hv_vcpu_get_exec_time` calls `Hv::Vcpu::get_if_owning_thread(id)`. That compares the caller's thread-local
    vCPU with the id and fails otherwise.
  - It then calls `Hv::Vcpu::get_execution_time() const`, which is `ldr x8,[x0,#0x10]; ldr x0,[x8,#0x790]; ret`:
    two loads from the vCPU's context, no trap into the kernel.

  So the thread itself reads it, at every exit, for the price of a call and two loads (the call cost is ROOT's to
  measure, below). It publishes the value for other threads (`hv::hvf::Ran`). Units are converted by
  mach_timebase_info (`platform::mach_ns`, now shared with `process_usage`).
- **Freshness.** A guest that never exits never publishes. PM M288 measured 0 exits in 6.9 s of a guest spinning
  with its timer unarmed, and with the in-kernel GIC a guest's timer ticks reach no user exit either. A reader
  (`GuestClock::read`) therefore makes a vCPU that is in its guest leave once with `hv_vcpus_exit` ("Forces an
  immediate exit"; a cancel to a vCPU about to enter is sticky, hv_vcpu.h:371-381). It then waits for that exit
  alone. The vCPU publishes at the exit, before it handles it, so a reader never waits on device emulation, and a
  vCPU blocked in a device access is already published. A vCPU that is out of its guest (parked, held by D191's
  hold, paused, powered off by PSCI, not yet released) published as it left, and is read without a wait. Readers
  never block a vCPU: the vCPU thread takes no lock that a reader holds, and the forced exit costs it one trip
  through the run loop (`park_for_snapshot` returns at once when no snapshot is asked for).
- **The protocol** (`Ran`): `stints` is odd while the vCPU is in its guest. The vCPU's thread bumps it before
  `hv_vcpu_run` and again after it, storing the read ticks before the second bump. A reader that sees it odd counts
  itself in `waiting`, kicks, and waits on a condvar until `stints` changes. The vCPU bumps (SeqCst) then loads
  `waiting` (SeqCst), and the reader increments `waiting` (SeqCst) then loads `stints` (SeqCst). Either the reader
  sees the bump, or the vCPU sees the waiter and notifies under the lock. No wakeup is lost. A failed read keeps
  the framework's code (`failed`), and readers get it as a typed `Error::Call`.

### KVM

- **No guest-time statistic in KVM.** In Linux v6.17 the binary stats (`KVM_GET_STATS_FD`) count exits and halts,
  not time in the guest:
  - `include/linux/kvm_types.h:104-116`, `kvm_vcpu_stat_generic`: `halt_poll_success_ns`, `halt_poll_fail_ns`,
    `halt_wait_ns` and histograms;
  - `arch/x86/include/asm/kvm_host.h:1624-1661` and `arch/arm64/include/asm/kvm_host.h:1170-1179`, `kvm_vcpu_stat`:
    exit counts.
- **The scheduler's per-thread guest time.** Linux keeps it, `p->gtime`.
  - Tick-based accounting charges a whole tick to it when the tick lands while the thread runs a guest (`PF_VCPU`):
    `kernel/sched/cputime.c:144-152` `account_guest_time`, from `account_system_time` (:193) and
    `irqtime_account_process_tick` (:406).
  - With CONFIG_VIRT_CPU_ACCOUNTING_GEN on nohz_full CPUs it is kept in ns (`vtime_account_guest`, :686-702) but
    flushed per tick.
  - /proc says it as field 43 in clock ticks, `nsec_to_clock_t(gtime)` (`fs/proc/array.c:646`, `do_task_stat`;
    the field order is checked against :594-647).
- **Other sources, and why not.**
  - The thread's CPU clock around `KVM_RUN`: ns-precise, but it counts KVM's in-kernel exit handling and halt
    polling as guest time, and costs two syscalls each time KVM_RUN returns.
  - `perf_event_open` with `exclude_host`: counts cycles, not time; needs a PMU and perf permissions, and takes a
    hardware counter per vCPU.
  - BPF: privileged.
  - `/proc/PID/task/TID/schedstat`, taskstats and cgroup `cpu.stat`: none of them splits guest from host.
- **Chosen:** field 43 of the vCPU thread's stat (`platform::thread_guest_ns`).
  - The file is opened by the vCPU's TID and read once per sample, from any thread, then closed. The TID comes
    from the thread record `hv::kvm` already keeps for its kicker, which is cleared as the vCPU drops.
  - Nothing is opened or held at vCPU setup, so the start path is unchanged.
  - The registry reads under the lock a leaving vCPU must take, so every vCPU read is alive, and so is its thread
    and TID, for the whole sample.
  - It costs the vCPU nothing.
  - The confined VM process may read beneath /proc/self (its Landlock rule, `confine.rs`), and its seccomp filter
    allows `openat`, `read` and `close`.
  - Resolution: 1/USER_HZ = 10 ms steps, `sysconf(_SC_CLK_TCK)` = 100 on adacachy (Linux 7.2.9-2-cachyos,
    CONFIG_HZ=1000, CONFIG_VIRT_CPU_ACCOUNTING_GEN=y, no nohz_full CPUs asked for, so tick accounting). Each
    thread's value can run up to one scheduler tick ahead of its precise runtime (whole-tick charges), and the
    ticks are sampled, so short windows are coarse. ROOT's real-VM test reads the step size live.

### WHP

- **The counters.** Windows SDK 10.0.26100's `um/WinHvPlatformDefs.h:3546-3564` (sha256
  91a864b6ec2d932bcc7ce17f503f72a7813759cde81be2d1ac86cba9abb61c4a) defines `WHvProcessorCounterSetRuntime = 0` and
  `WHV_PROCESSOR_RUNTIME_COUNTERS { UINT64 TotalRuntime100ns; UINT64 HypervisorRuntime100ns; }`, with a 16-byte
  C_ASSERT, outside the `_AMD64_`/`_ARM64_` branches. `um/WinHvPlatform.h:305-314` declares
  `WHvGetVirtualProcessorCounters`.
- **Microsoft Learn.** The function's page (git d1b0f1e9, updated 2026-06-11) gives the signature and
  requirements: Windows 10 1809 on x64, and Windows 11 24H2 build 26100.3915 on Arm64. It states no thread or
  processor-state requirement, and does not define the two fields.
- **The TLFS.** Its "Virtual Processor Run Time Register" section (vp-properties, git e7762aa4, updated
  2025-12-16) defines a VP's run time as "a combination of the time the virtual processor consumes running guest
  code, and the time the associated logical processor spends running hypervisor code on behalf of that guest", in
  100 ns units. So guest = Total - Hypervisor (`hv::whp_guest_ns`).
- **Reads.** From any thread. The clock holds the partition as the kicker does, so a deleted VP fails the call.
  windows-sys 0.61.2 binds the struct and the set, and arm64's FFI takes them from it as it takes the
  VPCI types (`sizes` holds the 16 bytes).
- **Unverified.** That a read while the VP runs succeeds, and what it costs. No Windows runner was reachable.

## What it is and where it shows

- **The hv seam** (each backend exports the same names): `hv::Vcpu::guest_clock() -> GuestClock`, and
  `GuestClock::read() -> nanoseconds`, from any thread.
- **vm** (`vm/guest_time.rs`, shared by all backends):
  - `GuestClocks` keeps each vCPU's clock by index, in a `BTreeMap`, not an array sized at start, so vCPUs may come
    and go (elastic `--cpus`).
  - A vCPU joins on its own thread before its machine is reported started, and leaves as its thread ends. Its time
    is read a last time on its own thread, before the vCPU is dropped, and added to `departed`. The machine's total
    is `departed + sum of the present vCPUs`, and never falls:
    - each vCPU's value is the most it has read (`last`), so a read that comes back lower, or fails, cannot lower it;
    - a departure adds at least that value to `departed`;
    - a vCPU that joins starts at 0;
    - a vCPU of an index already present (none today) folds the older one's time into `departed`, and a per-join
      id stops the older one's guard from removing the newer one.
  - A failed read gives the vCPU's last value and why (`unread`). A sample reads under the registry's lock. Only
    joining and leaving vCPU threads take that lock, and they are out of their guests.
  - `Handle::guest_times() -> GuestTimes { vcpus: [VcpuTime { index, guest_ns, unread }], departed_ns }`, with
    `total_ns()`. Its text form (`encode`/`decode`) is a line each, `departed NS`, then `vcpu INDEX NS [unread
    WHY]`, in the style of init's STATS lines, so that clients that do not link the vmm crate can read it.
- **VM process.** `vm_run::guest_times()` reads the running VM's handle. A daemon's ask, `GUEST_TIME` (kind 88,
  numbered), arrives on the run's link or a joiner's, and `warm.rs` answers `GUEST_TIMED` (89) with 0 and the
  text, or 1 and why. It answers at once, because a sample waits on no vCPU exit's handling, including this
  connection's.
- **Daemon.** `guest_times_of(id)` asks the run's VM and waits for its numbered answer through the run's inbox,
  as `pause_vm` does, for at most `TAKE_TIMEOUT`, the bound `usage_of` already uses for the same stats sample. The
  VM answers on receipt. The inbox fails pending asks as the VM ends.
- **Engine API** `GET /containers/{id}/stats` (`engine/watch.rs`). The `microvm` object, which already carried the
  VM process's `footprint`, `resident` and `cpu_ns`, now also carries:
  - `guest_ns`, the total;
  - `departed_guest_ns`;
  - `vcpus: [{"index", "guest_ns", "unread"?}]`.

  The guest times are read before the process's CPU time, so `cpu_ns - guest_ns` is the VMM's own and is never
  less than none past the clocks' steps.

### Not changed, and why

- **`read metrics` (OTLP).** init writes it from inside the guest, straight onto the asker's descriptor
  (`daemon/metrics.rs`, `init/src/metrics`). It holds cgroup observations init makes itself. Putting host-measured
  vCPU time in it would carry host-authoritative numbers through the guest, or change its one-document contract.
  The microVM-level document (`microvm`) carries them instead.
- **`shards stats`.** Its piped text is Docker's table, byte for byte (`shards_cmdline::format::stats`, held to
  docker/cli's goldens), and has no column for this.
- **`shards top`.** It lists the guest's processes, and vCPU time is not one of them.
- **`shards run --kernel` VMs, and the image-boot API's runs.**
  - A `run --kernel` guest has no daemon link. `vm_run::guest_times()` is what any channel to its VM process can
    serve, and clients that do not link `shards_vmm` can read the text form.
  - The image-boot API's runs (D231, on main as this merged) are hosted by `shards api-run`
    (`crates/shards/src/api_run.rs`), which plays the daemon's side of the VM's link. Its `stats()` answers
    `shards_api::image::wire::StatsAsk` with init's in-guest STATS lines. To carry guest time too, it can ask
    `GUEST_TIME` on the same link, which `warm.rs` answers on any daemon-side link. Its wire `Stats` would then
    need a field. That wire belongs to its owner, so this branch does not change it: it is a follow-up for that
    lane.

## Rules (R01-R16)

- **R01.** No unwrap, expect or indexing in production code:
  - checked and saturating arithmetic;
  - the /proc read takes one page and refuses a line that fills it (`STAT_LINE_MOST`, derived below);
  - `u32::try_from(index)`;
  - poison-tolerant locks (`crate::sync`).
- **R02.** Sources above, with alternatives per backend.
- **R03.** Constants:
  - `STAT_LINE_MOST` = 4096: a page, seq_file's first buffer for a single record (`fs/seq_file.c`,
    `seq_read_iter`), above the line's bound of 52 fields of at most 21 bytes and a name of at most 63
    (`proc_task_name`'s 64-byte buffer).
  - Kinds 88 and 89: the next free, after main's `MEMBER_ENDED` took 87 (D231) as this merged.
  - `TAKE_TIMEOUT`: existing, reused for the same purpose as `usage_of`.
  - The real-VM test's spin, idle and sample period are test-only and explained in it. Its slack is the step of
    the process CPU clock it compares against, read live (`cpu_step`), times vCPUs + 1.
- **R04.**
  - HVF: in or out of the guest is `stints`' parity, with transitions on the vCPU thread alone, and readers wait
    for a transition.
  - A vCPU's life in the registry: join, samples, leave, folded into departed. The join id tells one incarnation
    from the next.
  - Failures are never dropped silently: the answer's 0/1 byte, `decode`'s refusal (`None`), per-vCPU `unread`,
    the JSON's `unread`, and HVF's framework code as `Error::Call`.
  - **Gap, for ROOT's review:** these failures travel as text, as DEVICED's do, not as typed variants. Today no
    caller branches on them; they are logged or shown.
- **R05.**
  - New waits: the HVF reader's condvar wait for the forced exit (a completion, not a probe), and the daemon's
    numbered answer (an existing deadline).
  - State kept beside the authoritative counters:
    - HVF's `Ran.ticks`: the only cross-thread view of an owning-thread value, refreshed at every exit and forced
      fresh by the reader.
    - `Clock.last`: a monotonicity floor, one u64 per vCPU, dropped as it leaves.
    - KVM holds nothing: each read opens the thread's stat.
- **R06.** Small functions, and no new nesting past what is there.
- **R07.** Comments carry the contracts, sources and orderings.
- **R08.** Comparable VMMs report no guest time per vCPU:
  - Firecracker's vcpu metrics are exit counters;
  - QEMU's query-stats passes KVM's binary stats, which have none (above);
  - libvirt's `vcpu.N.time` is the thread's whole CPU time.

  Shards would report what none do, so there is no ratio to claim. The cost to the hot path is ROOT's to measure,
  and is owed.
- **R09.** A feature, not a fix: there is no defect to reproduce.
- **R10.** Unit tests and guard mutations (below). The live examples are ROOT's (below).
- **R11/R12.**
  - The registry, wire format and serving are shared. Each backend's adapter is one clock. The OS read is behind
    `platform` (`thread_guest_ns` on Linux, `mach_ns` on macOS).
  - The HVF-only protocol exists because its contract and code forbid another thread. For KVM and WHP, any thread
    may read, and one protocol for all three would cost KVM a /proc read at every exit.
- **R13.** Smallest complete change. The record is this file, and no D-number is taken: ROOT places the entry.
- **R14/R15.** Profiling, benchmarks and real-VM runs are ROOT's (below).
- **R16.** The runners' synced copies and target directories are regenerable.

## Light tests and gates (implementer)

See [gates.md](gates.md) for each command, runner, revision and result. On the merged tree (9c2a671cb):
- the unit tests relevant here pass on Linux x86_64;
- clippy `-D warnings` is clean for every target the change compiles for: x86_64 and aarch64 Linux (gnu, and
  musl for the test guest); x86_64 and aarch64 Windows (WHP, shards-vmm); aarch64-apple-darwin (HVF,
  shards-vmm, type-checked from Linux).

## Owed to ROOT

1. **Real-VM test, every backend:** `crates/shards/tests/guest_time.rs`,
   `each_vcpus_guest_time_only_grows_and_stays_within_its_processs_cpu_time`. It runs machines of 1, 2 and the
   host's CPUs (capped at the pinned kernel's NR_CPUS and the hypervisor's most), one after another in its process.
   The test guest's new `spin` mode spins a pinned thread per CPU for 2 s, then idles for 1 s, then powers off.
   - **Every 50 ms:** each vCPU is present by index and read; no vCPU's time falls, nor the total, nor
     `departed_ns`; and the total is at most this process's CPU time, read after it, plus (vCPUs + 1) of that
     clock's steps.
   - **After the spin:** every vCPU ran some guest.
   - **After the end:** no vCPU is left, `departed_ns` is at least the last total, and that total is within the
     process's CPU.
   - It prints the samples, totals and each vCPU's time.
   - Run it on HVF (M-series Mac), KVM x86_64 (Linux), KVM arm64 (the L1 nested path, `common::linux_tests`),
     WHP x86_64 and WHP arm64:
     `cargo test --profile test-release -p shards --test guest_time -- --nocapture`.
2. **HVF ignored test:**
   `hv::hvf::tests::a_vcpus_guest_time_is_read_from_another_thread_as_it_spins`, run alone:
   `cargo test --profile test-release -p shards-vmm --lib -- --ignored --exact hv::hvf::tests::a_vcpus_guest_time_is_read_from_another_thread_as_it_spins --nocapture`.
   It checks:
   - the mach-tick units against the thread's CPU time (they should agree within the forced-exit lag);
   - monotone reads, each no more than the thread's CPU time;
   - the cross-thread read latency (n/p50/p90/p99/max);
   - `hv_vcpu_get_exec_time`'s cost per call on its own thread, which is the per-exit hot-path cost of this
     change. Record it, and compare it with an HVF exit's cost.
3. **The idle phase.** Whether HVF's exec time stays flat while vCPUs idle in WFI, which the real-VM test's
   process-CPU bound will catch if not. Whether KVM's gtime and WHP's runtime counters do the same.
4. **WHP:**
   - that `WHvGetVirtualProcessorCounters` succeeds from another thread while the VP runs, which the real-VM test
     exercises;
   - that `Total - Hypervisor` tracks the guest's spin;
   - the call's cost;
   - (compile and clippy of both Windows targets: done from Linux, gates.md.)
5. **Hot-path cost** on an exit-heavy workload (HVF), before and after: n, p50/p90/p99/max (R14).
6. **macOS natively.** Run the unit tests (`mach_ns`'s test; `guest_time` and the stat parser run there too) and
   a native clippy of shards-vmm, shards and shards-vm-process. The HVF backend was type-checked and linted from
   Linux, with its C shim stood in for. The Mac Studio runner was reachable only through kiln, whose server kept
   timing out.
7. **Main's own clippy failure.** `spec::has_vm_flag` is dead in shards-vm-process's binary (Hex GPU patches;
   gates.md). It is not this branch's.
8. **Start and teardown.**
   - Start: a vCPU joins the registry with an Arc clone and an uncontended lock before it reports set up, and
     opens nothing.
   - Teardown: each vCPU makes one final read under the registry's lock, serialized as vCPUs leave together.
     On KVM that is a /proc open and read each; on HVF, a load; on WHP, one counter call.
   - Measure a machine's teardown with many vCPUs, before and after.
