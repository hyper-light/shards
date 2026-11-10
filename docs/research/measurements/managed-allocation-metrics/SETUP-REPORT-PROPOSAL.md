# Pure setup-aggregate reporting proposal — unimplemented

Implementer `01a1c5be192972d8b05dc7c207cc6fca`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Rules SHA-256 remains
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
This responds to root's next finite PURE-proposal request after native validation.
Implementation awaits root authorization; paired source remains
`dcac1f33d3b19877832bf091661d838f4c59bb91ece58fdcb46b0b5bdb671b4c`.

## Observed basis and limited change

The [root record](../managed-coordinator-allocation/README.md), exact commit
`eb5654d6f5356cacaca536ff51f14a632fa7e801`, retains the f53f native N1 functional
pilot and explicitly leaves the substantial pre-fixture allocation aggregate's
cause unknown. It establishes neither an artifact-hashing cause nor a selected
optimization. No profile or new interpretation of raw numeric samples was run here.

Current `allocation_metrics` reads `before_fixture` counters only as subtraction
origins. Its normalized output omits the initial absolute cumulative counters
and initial live/lifetime-peak gauges, although the original record retains them.
The proposal makes that existing aggregate visible without adding observation.

| Proposed output | Exact source and meaning |
| --- | --- |
| `observer_rust.before_fixture.<name>.process_cumulative` | Each existing `CUMULATIVE` field, unchanged and unreset from the first fixture snapshot. Process-lifetime wrapper bookkeeping up to that independent load. |
| `observer_rust.before_fixture.<name>.absolute` | Each existing `GAUGES` field, including recorded process-lifetime peak; no subtraction or phase-peak inference. |
| `before_fixture_attribution` in the summary | `Unknown / ProcessCumulativeBeforeFixtureDoesNotIsolateCallSites`, separately from whether a counter value is known. |

The [22-line unapplied patch](setup-report.proposed.patch) adds three statements
to existing loops/summary construction. All existing interval/native metrics,
validation, manifests, serial ordering, n/deadlines and cleanup control remain.
The existing generic distribution code would retain coverage and descriptive
quantiles for the additional fields; no new statistics framework is introduced.

This cannot split contributors or recover a test-entry interval. Values include
observer-process work preceding the first fixture phase across all wrapper-using
threads. Fields remain ConcurrentNonAtomic. Known counters do not imply a known
cause, coherent event boundary, usable allocation size, physical retention, daemon/
VM/guest/foreign-C/kernel completeness, leaked memory or zero observation cost.

## Finite proposed implementation and acceptance

Owned changes would be limited to `paired.py`, `test_paired.py` and this evidence
directory. The synthetic checks are:

1. Known initial values appear unchanged under the new cumulative/absolute names;
   every existing since-before-fixture/native metric remains identical.
2. Unknown initial values remain visible with unknown coverage and continue to
   propagate into affected differences; they are never converted to zero.
3. Initial live and lifetime peak stay absolute. The report never creates a
   since-before-fixture peak, phase peak, attribution or leak-freedom result.

The current14 pure regression methods would also run through one new private
remote source/target/log set, serial Kiln1CPU/1GiB/2GiB, `sync:false`. No actual
records, paired executor, fixture, native fault query, VM, socket, namespace,
BPF, profile, benchmark or mutation campaign would execute. No n/default,
performance allowance or production constant is selected.

Native fixture4ff remains frozen at
`31df9e2bc83270fc4fee35caaa34b885b5e12dcae551bbee9238cbdbf6e232b7`.
The separate startup/per-artifact snapshot proposal remains unimplemented.
No artifact resolver/read/hash, allocator registration, platform adapter,
warmup, reset, cache or production optimization is part of this proposal.

## Readiness and root ownership

[Source-only record](records/setup-report-proposal-source-checks.json) pins base
`4103ca3ddd9ca0dab39a7c58d40a4fad88f1325f`, original/draft hashes and successful
`git apply --check`. It is not a test or implementation result. No actual paired
source or frozen fixture changed. Root must authorize the finite batch first.

Root's exact native source `f53f7750eb6c74ce6f7de290f6977c08a401f774` now has
normal carried-helper/caller build0, ordinary16pass/2ignored, strictN1 functional0
with native best-effort/conditional observations and actual owner/exit/FD7→12→7,
plus full seven-target scripts/lint0. Those are source-specific functional gates,
not adoption of this proposal, the paired runner, or a comparative benchmark.
Root still owns separate full paired review/adoption, actual release pairing/
interleaving with caller-supplied n, perturbation/noise/physical accounting,
native error/race/guard-removal/restoration and cleanup/matrix campaigns.

R01/R04/R06/R07/R13: small existing pure report seam, explicit Unknown cause,
no new control or unsafe path. R02/R03/R05/R12: actual sampled aggregate and
existing code, no guessed cause/adapter/cache/production literal. R08/R14:
reporting proposal only, no measured improvement or completeness claim.
R09/R10: observation-visibility enhancement, not a speculative product fix;
unapplied source proof and owed tests retained. R11: no new platform result.
R15/R16: coordinator approval/execution boundary, no children/goal dispatch,
main merge, local build/new worktree/large copy or target/cache deletion.
Frozen root f53f and all five eaf0 red artifacts remain protected.
