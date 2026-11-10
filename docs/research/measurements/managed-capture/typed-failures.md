# Capture allocation refusal and tenant containment

Implementer `/root/managed_capture`, policy `c921a0b1`, rules SHA-256
`1ed0b1ad00e9228d813d1892d6f55cf330a5485647aff19527843ff8bf616b3d`.
This follows [the core checkpoint](core.md), signed `9151296a`, and the approved
local dependency merge `14f80a7d` of lifecycle `213c61f5`/`df47bc06`. Original
lifecycle commits remain ancestors. Baseline fixture commit: `982f8ff6`.
The commit containing this record identifies the fixed source.

## Actual failure and fixture correction

Repeat on a native Linux host:

```sh
cargo test -p shards-init capture::tests::allocation_refusal_keeps_its_resource_cause \
  -j 4 -- --exact --nocapture
```

Only an isolated child lowers its own soft RLIMIT_AS to zero after creating the
capture fixture. Its request is the existing finite protocol maximum, not an
unbounded allocation. The first limit-only example (Fedora `8fadc`, kiln r10)
still allocated successfully; [its log](allocation-limit-only.log) records child
exit 3. That run is a fixture confound, not evidence of a resource-refusal defect.

The child now additionally starts with glibc `MALLOC_ARENA_MAX=1` and
`MALLOC_MMAP_THRESHOLD_=0`. These are test-only allocator settings, never runtime
policy. The Linux [address-space limit contract](https://man7.org/linux/man-pages/man2/getrlimit.2.html)
describes ENOMEM from new mappings beyond the soft limit; the
[glibc allocator contract](https://man7.org/linux/man-pages/man3/mallopt.3.html)
describes its arena and mapping controls. Existing mapped arenas can otherwise
service a request despite the new limit, as the actual first run showed.

With that setup, kiln r11 at the same source produced **SIGABRT** and
`memory allocation of 16 bytes failed`: [actual red](allocation-red.log).
`Capture::space` received a fallible Vec reservation failure, then boxed it through
`io::Error::other`, making another allocation in its failure path. No production
change preceded this actual red. The fixture now disables this child's core dumps
before applying its memory limit; the original red log retains its observed
`core dumped` status. No claim is made about removing an external crash artifact.

## Fixed behavior and state

Guest capture returns a fixed typed `Failure`:

- `Protocol(managed::Error)` identifies checked framing/admission/state failures.
- `Transport(io::Error)` retains the actual connection failure.
- `Member { member, channel, cause }` identifies the affected trusted capture pipe.
  `AllocationRefused(TryReserveError)`, `AllocatorCapacity { allocated, admitted }`
  and `ReadPreparation { errno }` remain distinct causes.

The actual failed allocation returns its original reserve error by value. It
does not allocate a String or boxed I/O error. Checked credit/protocol mismatch
does not masquerade as physical OOM. Allocator refusal and allocator capacity
overshoot both map to ENOMEM only at the existing errno-based tenant abort wire
boundary; their typed internal causes remain distinct. No new wire value, grant,
quota, retry timer or buffer default was introduced.

The real run consumer checks the trusted member against its owned `Started` and
uses lifecycle's existing held-cgroup/pidfd
`quarantine(QuarantineCause::Capture { errno })`. It does not duplicate population
kill logic. It then calls `abort_output(errno)` independently: lifecycle's already
Ended case retains its population proof while output still terminates. Pending
output control prevents an older readiness snapshot from processing the same
tenant again. Shared protocol/transport corruption remains a boundary failure.
No actor payload chooses this control identity or supplies a trusted error tag.

Control queue, frame state and scheduler errors now retain the shared ABI enum
instead of allocating/parsing error strings. Startup `Capture::prepare`,
`ControlQueue::prepare`, domains startup and outer setup/host presentation still
use legacy String boundaries. This checkpoint does not claim those prelaunch or
shared-boundary low-memory paths are all allocation-free. Actual whole-VM
quarantine/endpoint/control behavior under resource exhaustion remains a live
combined gate; this test only proves the specific error construction and native
scope classification.

## Scoped results

Environment: same native Linux runner and local arm64 Mac/Rust toolchain described
in [core.md](core.md). Mac checks used nice 19, lockf and cargo -j4 above the disk
floor. No profiling or heavy VM campaign was performed by this lane.

| Command | Actual result |
| --- | --- |
| `cargo test -p shards-init capture::tests -j 4 -- --test-threads=1 --nocapture` | Fedora kiln r12: 6/6, [log](allocation-green.log); constrained child matches its exact typed member/channel/allocation cause |
| `cargo test -p shards-init managed::tests -j 4 -- --test-threads=1` | Native Linux combined source: 12/12, including first-cause/ended-proof/shared-fault preservation; [output excerpt](typed-transition-results.log) |
| `cargo test -p shards-init run::cut_tests -j 4 -- --test-threads=1` | Native Linux combined source: 4/4, including partial/rechecked cut replies; [output excerpt](typed-transition-results.log) |
| `cargo clippy -p shards-init -p shards-vm-process --target x86_64-unknown-linux-gnu --all-targets -j 4 -- -D warnings` | Clean on combined source; the real Capture cause caller satisfies the lifecycle dependency; [output excerpt](typed-transition-results.log) |
| `cargo test -p shards-vm-process --bin shards-vm -j 4 -- --test-threads=1` | Native arm64 Mac: 28/28, [raw log](typed-host-unit.log) |

The new actual native `/dev/null` ioctl example gets ENOTTY and returns only its
trusted member/channel's `ReadPreparation` cause, preserving its captured offset
and a sibling's issued credit. Another test proves an oversized requested frame
is Protocol(Capacity) before allocation. Neither substitutes for an actual VM
tenant-containment or malicious sibling takeover test.

## R01–R16 update and remaining proof

The rule-by-rule record in [core.md](core.md) applies to unchanged code. R01 gains
an actual red/green for the allocating failure fallback; R04 gains typed guest
capture errors and real shared lifecycle cause handling; R09/R10 gain the exact
native constrained-child logs and the explicitly recorded fixture confound.
R03/R05/R12 add no tuning constants, caches, polling loop, alternate OS manager or
installed binary. R06/R07/R13 keep the scope at existing capture/control consumers.
R08/R14 remain unmeasured: no performance or leak claim. R11 remains partial with
native Windows, arm64 Linux/musl and real VM matrix owed. R15/R16 preserve root's
combined review/merge authority, active lanes, source/evidence and storage floor.

Still owed: guard-removal mutation that restores the boxed fallback; actual
VM-local allocation failure with sibling/main/control progress and resource
return; endpoint abort ordering after in-flight records; native Windows and all
seven targets; restore-generation/cursor semantics; hostile takeover and sustained
fairness/deadlock campaigns; combined profiling and comparative results. The
startup String boundaries and implicit allocations in existing outer error paths
are explicit remaining work. No whole-P3 or product completion is claimed.
