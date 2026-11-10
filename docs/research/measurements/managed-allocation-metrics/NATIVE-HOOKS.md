# Native fault phase hooks — implementer checkpoint

The later [coordinator f53f record](../managed-coordinator-allocation/README.md#native-phase-hooks-exact-f53f-gates) adds actual normal-caller/strict N1 and seven-target gates. Remaining error, native-platform, mutation and comparative work below is still owed.

The coordinator approved the existing-seam native-fault queue after the v2 code
proposal. `managed-output-cost/fixture.rs` now includes the pure `native_faults`
model and fixed stack phase samples for observer/test parent, owned daemon and
VM host. This does not change a production platform API or implement an OS adapter.

Each held process query uses the original `PinnedProcess` identity check before
and after existing `platform::process_usage`. The observer temporarily opens
its own pinned directory and drops it on return. No temporary object is retained
in the phase record. Query failures and subsequent identity failures are recorded
independently; neither can become an observed count. Expected identity remains
separate from a valid observation.

The four live phases are after start/watch acquisition, at owner readiness before
record serialization, after proc records, and after observer completion. After
unchanged owner stop/join and watch release, only the observer is queried again;
daemon/VM fields are explicitly ProcessExitedOrHandleReleased Unknown. This
cleanup probe precedes the original total-FD sample, so the unchanged exact FD
assertion includes its effects. No observation object is added to a baseline or
given slack. Original identity/exit/deadline/protocol/shutdown assertions remain.

The native JSON record is rendered after original teardown and before original
strict assertions/rethrow, including partial/NotReached states. It records
source name, native width, expected identity, query/identity errors and caveats.
Live phases and Rust allocation snapshots occur at distinct ordered call points;
they are not one atomic cut, frozen population or exact quiescence. Probes and
their allocations/I/O are fixture costs, not subtracted overhead.

Source/range and delta semantics are in [FAULT-PROPOSAL.md](FAULT-PROPOSAL.md).
Linux exposes the existing Usage minor-plus-major sum (with its fallback/saturation
limits). macOS signed32 conversion and Windows DWORD32 widening do not establish
wider producers or equivalent fault categories. Invalid/decreased/source/identity/
order/unknown states yield Unknown; otherwise differences remain conditional on
absence of wrap/reset/lossy fallback. No native zero after exit is invented.

The unchanged pure model at `f78c3c8919285bc067bae6006b35bf57e6acb472`, SHA-256
`3946267e81571fcfd1c0853b1033bf170ef45632a7d9d99d747bef9ffd710513`, passed
GNU5 and musl5 plus Clippy0 at the admitted 1CPU/1GiB/2GiB private Fedora target.
[Raw records](records/faults-f78c3c891) retain original commands/compiler/source/
binary identities and SHA-256 archive
`a171873aac422f09552760a200e35e99f34d4ffb642cd6a3e4a6e1fc00e6146f`.
These five tests are synthetic metadata/state checks: no platform query, native
process sample, VM, profile or measurement was executed by the implementer.

Root reports actual allocation-only source033c gates on main
`7c26e5292161fc891a7c6c6525305464a3a044be`: ordinary11 pass/2 ignored;
N0/N1/N2 strict0 with allocation phase/final records and FD7→12→7;
full seven-target/format/GPU lint0. These are **not** validation of these new
native hooks or the paired runner. Each pilot has n1 and is not a comparative
distribution. Startup aggregate requested/recorded-peak observations do not
identify their allocation cause; root owns isolation before optimization.

Root still owes this changed actual normal-helper caller compile, all supported
target checks, real serial phase/identity/FD/exit/cleanup and I/O/identity/error
paths, native source/width validation on real hosts, independent meaningful
guard mutations/restorations and paired/interleaved profiles with full component
and instrumentation costs. Ordinary synchronous platform reads are not given
a new interruptible I/O deadline. No physical fault completeness, parent/kernel
budget, leak freedom, performance ratio or full management-plan closure follows.
