# Does WHP time a vCPU's halt? (whp-halt-clock-v1)

#36, own-35i, ROOT's ruling of 2026-11-18. Policy: origin/main 4ff00b72a8, rules sha256
df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3, role implementer.

## Question

D215's judge holds a party whose host kept it from a CPU. For a microVM whose CPU is its
liveness alone (`Cpu::Liveness`: a storm, a guest of more vCPUs than its host has CPUs), the
exact hold is each vCPU's time wanting to run less its time run. On WHP the time run is exact:
`WHvGetVirtualProcessorCounters(WHvProcessorCounterSetRuntime)`'s `TotalRuntime100ns`, which
the Hyper-V TLFS ("Virtual Processor Run Time Register") defines as guest plus hypervisor time
on the VP's behalf. The time wanting to run is not: shards' partitions emulate the local APIC
(`WHvX64LocalApicEmulationModeXApic`, hv/whp/mod.rs `partition`), so the hypervisor runs HLT
itself and a halted vCPU stays inside `WHvRunVirtualProcessor` with no exit to the VMM
(`Vcpu::run` treats `WHvRunVpExitReasonX64Halt` as an error). Microsoft Learn's page for
`WHvGetVirtualProcessorCounters` (git d1b0f1e9, updated 2026-06-11) lists the intercept
counter set, `WHV_PROCESSOR_INTERCEPT_COUNTER { UINT64 Count; UINT64 Time100ns; }` per
intercept, `HaltInstructions` among them, and defines none of the fields. If
`HaltInstructions.Time100ns` grows by the span a vCPU is halted, the VMM has an exact halt
clock: wanted = its time inside `WHvRunVirtualProcessor` less that span, and its host's hold
= wanted less `TotalRuntime100ns`. If it grows only by the intercept's handling, WHP has none.

## Method

`cargo test -p shards-vmm --lib -- hv::whp::tests::halt_clock_probe --ignored` (run.ps1),
on a Windows x86_64 host with the Windows Hypervisor Platform, nothing else loading it:

- One partition of one vCPU, made as every shards partition is (`Vm::new`), three pages of
  memory at guest address 0, the vCPU in real mode (CS base 0) on the test's own thread.
- Three phases, each run `RUNS` = 5 times in turn, each run cancelled by the host
  (`WHvCancelRunVirtualProcessor`, the backend's `Kicker`) 2 s after it began:
  - `halted`: `cli; hlt` at 0x1000 (a `jmp` back to the `hlt` after it), halted throughout;
  - `spinning`: `jmp $` at 0x2000, running throughout;
  - `spinning-loaded`: the same beside a busy loop of normal priority on every host CPU
    (`available_parallelism` threads), so that its thread wants a CPU it does not always get.
- Before and after each run, on the vCPU's thread: `TotalRuntime100ns` and
  `HypervisorRuntime100ns`, `HaltInstructions.Count` and `.Time100ns`, the thread's cycles
  (`QueryThreadCycleTime`), and the wall time (`Instant`). And once, before the phases, this
  host's thread cycles a second: a thread spinning alone for 2 s.

## Output: halt-clock.csv, one row a run

| column | what |
|---|---|
| `phase`, `run` | the phase, and the run within it (0 to 4) |
| `wall_ns` | wall time between the run's two reads |
| `total_runtime_100ns`, `hypervisor_runtime_100ns` | the runtime counters' growth |
| `halt_count`, `halt_time_100ns` | `HaltInstructions`' growth |
| `thread_cycles` | the vCPU thread's cycles' growth |
| `busy_threads`, `host_cpus` | the busy loops beside it, and the host's logical CPUs |
| `thread_cycles_per_s` | the host's thread cycles a second, measured alone |

host.txt holds the revision, host, CPU, OS build and rustc.

## How to read it

For each run, compare each time with `wall_ns` (100 ns units times 100 are ns), and
`thread_cycles / thread_cycles_per_s` with the wall in seconds:

- `halted`: the runtime counters and the thread's cycles should barely grow. If
  `halt_time_100ns` × 100 ≈ `wall_ns` (and `halt_count` ≥ 1), the counter times the halt.
  If it grows by µs, it times the intercept's handling alone.
- `spinning`: `total_runtime_100ns` × 100 ≈ `wall_ns`, `halt_time_100ns` ≈ 0. This checks the
  runtime counter against the wall on this host.
- `spinning-loaded`: `total_runtime_100ns` × 100 < `wall_ns` by what its host withheld. With
  `busy_threads` = `host_cpus`, a fair share would give the vCPU's thread about
  `host_cpus / (host_cpus + 1)` of a CPU. Its thread's cycles over its host's rate should agree
  with `total_runtime_100ns` to within what its own code ran outside the guest.

The decision it feeds: with a halt clock, the WHP backend declares each vCPU's time inside
`WHvRunVirtualProcessor`, its halt time and its run time (commit 3 of own-35i's lane), and
the judge subtracts the withheld time from a stillness, as it does on Linux from each vCPU
thread's `run_delay`. Without one, the choice between a guest-kernel steal clock and a stated
Windows gap is a decision record of its own (ROOT's ruling of 2026-11-18).

## Results

None yet: ROOT runs it on jupiter (bare-metal WHP) and records n, the per-phase distribution
and the reading here, with platform-measurements.md's entry.
