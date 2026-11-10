# Fault counters — primary-source proposal, no measurements

Source inspected: `24f48ee5ebf198485117aa93bf47685ad7d7d53f`, 2026-11-10.
Implementer/policy/ownership and root-only campaign constraints are in
[README.md](README.md). This is the next proposal, not platform implementation,
an audit of unrelated source, a defect reproduction or an executed fault result.

## Existing seams and native contracts

`shards_vmm::platform::process_usage(pid)` already returns `Usage::faults` on all
supported hosts. `platform/mod.rs` reexports the Unix implementation or
`windows_process.rs` as one seam. macOS also exposes `platform::task_events()`
for the current task only. A new per-OS framework is unnecessary for a first
native-counter observation. These interfaces do not promise equivalent fault
categories, held identity or a coherent snapshot across all fields.

| Host | Existing source | Native information to retain | Limits |
| --- | --- | --- | --- |
| Linux GNU/musl, amd64/arm64 | `platform/unix.rs::process_usage`, `/proc/PID/stat` | `minflt` field10 and `majflt` field12 separately; PID/start field22; original bytes | `Usage::faults` currently exposes only their sum. Child `cminflt`/`cmajflt` are different fields, not complete live-tree accounting. |
| macOS arm64 | `platform/unix.rs::process_usage`, `PROC_PIDTASKINFO` | `pti_faults`, explicitly labelled native process page faults | Apple exposes separate pageins/COW fields; no Linux major/minor equivalence is established. The native `pti_faults` field is signed32, while the existing usage conversion maps negatives to0. |
| macOS arm64, observer itself | `platform/unix.rs::task_events`, `TASK_EVENTS_INFO` | Existing four values: faults, actual pageins, COW faults, context switches | Source fields are 32-bit integers widened by the helper. No daemon/VM task is named by this current-task call. A widened value is not a wider producer counter. |
| Windows amd64/arm64 | `platform/windows_process.rs::process_usage`, `GetProcessMemoryInfo` EX2/EX | Native `PageFaultCount` and which producer/fallback supplied it | DWORD32 count, even though Usage stores64. No separate hard/soft count is returned here. Working-set counters and private-memory fallbacks are separate semantics. |

Primary sources and inspected revisions:

* [Linux v6.18 proc producer](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/proc/array.c):
  `do_task_stat` computes the whole thread group's own minor/major faults and
  emits child counters separately. Its concurrent reads are not a freeze of
  process or tree state. Actual runner kernel identity still must be retained.
* [Kernel proc documentation](https://docs.kernel.org/filesystems/proc.html):
  table1-4 distinguishes own and child faults and start identity. Pinned open
  proc descriptors do not redirect operations to a replacement PID after exit.
* [Linux man-pages proc_pid_stat(5)](https://man7.org/linux/man-pages/man5/proc_pid_stat.5.html):
  field10 minor faults need no page load from disk; field12 major faults require
  one. Waited-for-child fields are not a live-descendant census.
* [Apple XNU xnu-12377.1.9 proc_info.h](https://raw.githubusercontent.com/apple-oss-distributions/xnu/xnu-12377.1.9/bsd/sys/proc_info.h):
  `proc_taskinfo` defines distinct signed32 faults, actual pageins and COW fields.
  The header alone does not establish a cross-host category mapping or justify
  treating a negative/wrapped value as zero observed faults.
* [Same XNU task_info.h](https://raw.githubusercontent.com/apple-oss-distributions/xnu/xnu-12377.1.9/osfmk/mach/task_info.h):
  TASK_EVENTS_INFO defines these as distinct event counts alongside context
  switches. Neither pageins nor COW is inferred by subtracting other categories.
* [Microsoft PROCESS_MEMORY_COUNTERS](https://learn.microsoft.com/en-us/windows/win32/api/psapi/ns-psapi-process_memory_counters):
  PageFaultCount is DWORD, separate from working-set and commit byte fields.
  Documentation inspected last-updated2024-02-22.
* [Microsoft working-set contract](https://learn.microsoft.com/en-us/windows/win32/memory/working-set):
  soft faults can be resolved without backing-store access; hard faults require
  it. A fault count is not a disk-I/O or resident-byte count, and nonpageable
  allocation references have different behavior.
* [Microsoft GetProcessMemoryInfo](https://learn.microsoft.com/en-us/windows/win32/api/psapi/nf-psapi-getprocessmemoryinfo):
  queries a held process HANDLE with query rights, accepts the documented
  counter structures and reports failure through its return/GetLastError.

## Minimal next implementation to discuss with root

Do **not** change any platform API in this allocation batch. For the actual
Linux-only cost caller, its already pinned stat-before/stat-after raw records
can support a strict offline decode of each sample's own minor/major counters
under its recorded PID/start validation. This adds no observations or live loop.
It cannot retrospectively produce launch-to-ready phase deltas from a lone
ready checkpoint, nor turn N0/N1/N2 into paired measurements.

A later scoped phase implementation can reuse the existing pinned process
holder and finite phase checkpoints, retaining raw native values, source name,
native width, process identity, phase, read success/failure and non-atomic
semantics. It must strictly reject malformed/missing/overflow fields, rather
than reuse generic progress-report fallbacks as exact evidence. That work needs
root-approved parser/fixture ownership; neither parser nor platform source is
owned or changed here. No timer, periodic collector or replacement manager.

On other hosts, start from the existing `process_usage` seam for explicitly
labelled best-effort native totals, with identity checks around each numeric-PID
query and normal error handling. The present interface discards details needed
for stronger exact evidence, notably Apple's signed32 raw value and Windows'
native counter/fallback identity. A future raw/typed seam must be justified and
approved separately; no duplicated syscall adapter is smuggled into tests.
The macOS current-task `task_events` seam may supplement observer records,
without purporting to describe a different task or Linux fault categories.

Compare only the same source/category/width/same validated identity and ordered
phase pair. A decrease, unknown state, identity mismatch or arithmetic failure
makes a delta Unknown. Even a nondecreasing 32-bit pair cannot exclude one or
more full wraps without independent evidence; retain the pair and mark any
computed difference conditional, never silently use saturating or modular
subtraction as an exact count. A single aggregate sum never becomes separate
hard/minor/COW/pagein counts. Do not add overlapping child/process categories.

## Alternatives and root gates

[POSIX getrusage](https://man7.org/linux/man-pages/man3/getrusage.3p.html)
(IEEE1003.1-2017 reproduction) is a distinct self/waited-child interface and not a held arbitrary
daemon/VM-process identity seam. It supplies no portable live process-tree or
kernel-queue census. Perf/ETW/DTrace/BPF, system-wide fault collectors and native
profiler frameworks add different privileges, scope and overhead; none is
introduced or run by this lane. Mapped-page/resident probes answer a different
question and cannot replace fault events.

Root owes exact native OS/kernel/toolchain/source identities, serial caller
compile, actual supported-host validation, same-identity phase pairs, known
in-memory/disk/COW workloads where appropriate and independent failure/overflow/
identity mutations with restoration. Preserve all owned-exit/FD/byte/protocol
witnesses and root cleanup evidence. Attribution still requires separate
observer, daemon, VM/guest and kernel cost records; native faults alone cannot
prove hard aggregate budgets, leak freedom or an allocation optimization.
Any comparisons require paired/interleaved repetitions and distributions with
noise/observer overhead, not cross-OS equality of the raw native counters.
