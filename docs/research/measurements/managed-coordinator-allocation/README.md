# Coordinator validation of observer allocation instrumentation

POLICY_READ coordinator main3ea2164dd98878426936c880f0e8d542ef67d98f;
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
Root reviewed the complete403-line recorder,251-line pure fault interpretation
and concrete phase hooks, composed signed f78c3c891 with root's single caller
registration at033c621c4ad3066460dfc1c2c1edf4a094b86ef2, and owns these actual
normal builds, VM pilots and full matrix gates. This is test instrumentation,
not a production allocator or a selected final managed-output topology.

## Actual gates at033c

Fedora amd64 rootr610, normal carried helpers and real KVM:
[executed driver](allocation033c/driver.py), [environment](allocation033c/environment.json),
[artifact identity](allocation033c/test-artifact.json), all commands and raw logs
are retained. Ordinary managed_output tests:11 passed,0 failed,2 ignored,
including five recorder tests and the existing VM-backed output cases.
Both ignored measurements remain ignored in that ordinary invocation.

The explicit owner-output fixture was then invoked separately for N0,N1,N2;
each returned0. Each is one functional instrumentation pilot, not a distribution
or paired performance comparison. All three produced phase and final allocation
records, exact controlled output/main progress, owner stop and observer joins,
held validated daemon/VM exit readiness, and exact owned observation FD
baseline7 → held12 → released7. Exit readiness is not wait/reap/status/time/
causality or full-population/kernel-return evidence.

Rootr611 on macOS arm64 runnerb24c5c951260dcdfd0972f2aeaf3cf51 ran full
scripts/lint at the same source033c under lockf /tmp/g121-mac.lock, jobs4.
Format, all seven supported targets and GPU checks returned0. The managed_output
caller is Linux-only, so this does not falsely claim a native Windows/macOS
recorder run or that its Linux-gated body was compiled there.
The pure native-fault model is retained source/research; it is not yet connected
to native sampling and gains no OS-counter validation from this gate.

## What the records mean

The System wrapper counts events routed through this one Rust global allocator
in the observer/test-parent process. It delegates each valid native operation
once and performs checked atomic bookkeeping without heap allocation, formatting,
locks, callbacks, TLS or unwind. Successful and failed alloc/zeroed/realloc,
requested/growth/shrink/deallocation and live/requested-peak fields remain separate.
Overflow, reservedMAX and arithmetic underflow propagate sticky Unknown,
including the recorded peak. Counter tests use only valid System operations;
synthetic failure bookkeeping is not a native OOM or allocator-abort proof.

Snapshots are independent Relaxed loads, not coherent transactions or quiescence.
The peak is process-lifetime bookkeeping, not a phase peak or native realloc
transient. Initial artifact identity reads/libtest setup already contribute
before the first fixture phase. Final records can retain inputs/results/libtest
state and include earlier instrumentation. Do not subtract peaks or present
requested bytes as usable allocations, RSS, daemon/VM/guest/C/native-direct
allocations, kernel charges, copies or complete process-tree costs.

The pilot raw records expose substantial setup allocation before before_fixture.
No cause or optimization is inferred from that aggregate alone; isolating actual
artifact hashing/setup call sites and profiling the equivalent workload remains
root work. No observed number is a runtime constant or overhead allowance.

## Provenance and policy

[ARCHIVES.json](ARCHIVES.json) indexes both privately transferred text archives:
54058B/131regular members and3482B/8members. Every member's size/hash was verified.
Originals/source/artifacts remain on their runners; local originals are under
/Users/adalundhe/g121-retained-archives. No executable/credential was committed.
Raw logs are byte-identical; whitespace classification is separate.

The [implementer record](../managed-allocation-metrics/README.md) pins the primary
GlobalAlloc/System and native platform sources, alternatives, light evidence,
overflow/failure/concurrency limitations and owed work. Root confirmed the exact
real caller and phase ownership; it adds no production manager, cache, grant,
polling loop, crypto/host change or installed artifact. R01/R04/R06/R07/R13 apply
to the explicit counter/phase states and legal native allocation preconditions.
R02/R03/R05/R09/R10/R12 are satisfied for this instrumentation batch by the pinned
API contract, scoped proposal, actual source-matching caller and retained raw
results; no production bug/performance improvement is claimed.

R08/R14 comparison, allocation/copy/fault profiling and credible pinned baseline
distributions are still open. R11 real non-KVM/native-counter coverage is unrun;
Windows ARM remains paused. R15/R16 root owns all heavy gates/review/main landing;
agents supply scoped code/pure/light tests and ongoing independent queue work.
Protected targets/evidence are preserved and the local30GiB floor remains binding.

Next: source-matching paired/interleaved runner and native fault hooks need root
review/actual execution, overhead isolation and n/p50/p90/p99/max at equivalent
member counts. Hard aggregate/parent/kernel accounting, final topology,4–5×,
allocation-free and leak-free results are not established by these pilots.
