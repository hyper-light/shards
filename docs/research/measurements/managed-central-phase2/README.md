# Coordinator phase 2: actual effects, resource limits and reporting failures

POLICY_READ coordinator: main f5cec365d2e435afae5e6f613c92d9d3759ef60f,
rules SHA256 d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242.
These are actual coordinator executions on identified revisions, not completion
of the managed-runtime plan. The main coordinator owns these heavy campaigns,
audits, profiling and integration. Scoped implementers do light checks only.

## Exact outcomes and reproducible records

Commands, inputs, source/kernel/config identities, stdout/stderr, traces and exits
are retained beneath the named directory. Deadlines and resource allocations are
caller-selected finite experiment envelopes, never copied into production policy.
Earlier source gates are not passing gates for later revisions.

| Source / root run | Actual outcome | Raw directory beneath linux/ unless stated |
| --- | --- | --- |
| 22ea / r227 | Removing the entire selected PREROUTING rule lets the loopback client reach the backend: accepted2, relay0, joinedtrue, cargo101. Own-address also receives the second backend bytes, but ENOTCONN107 loses the old worker completion record; wrapper1 honestly rejects incomplete evidence. Source byte-identically restored in finally. | managed-intercept-22ea-local-mutations-v1 |
| 22ea / r230 | Independently rerun restored loopback and own-address cases pass. The prior driver assertion had prevented this green stage. | same directory, restored artifacts |
| 02542 / r273 | Actual task fixture fails before Fill because control endpoints have no host publications. This is a fixture error, not an exhaustion or isolation finding. | managed-limits-0254-v1 |
| e982 / r276 | Actual finite task exhaustion/recovery plus focused units7/0. EAGAIN after held2; fresh healthy-tenant and main nonces. Release reaps3, replacementPID6, held0. | managed-limits-e982-v1 |
| 37a62 / r279 | Four present IPv4 TCP origin cases pass; absent listener without fallback gives actual direct backend effect, accepted2/receipts2/relay0, joinedStoppedtrue, cargo101. Task recovery and cancellation batch8/0, including both actual guests. | managed-combined-37a6-runtime-v1 |
| 2cc8 / r284 | Four present IPv4 origins pass. With separate selected DROP fallback and no listener, client ConnectCall expires at the original absolute deadline; backend remains baseline1/1, joinedtrue. Old strict refusal predicate fails101, honestly retained. | managed-intercept-drop-2cc8-v1 |
| ba28 / r286 | Corrected typed bounded predicate passes. Removing ONLY appended fallback DROP produces clientSuccessBackend, actual backend2/2/relay0, joinedStoppedtrue, cargo101. Byte-identical source restoration and repeated guarded case pass. | managed-absence-ba28-mutation-v1 |
| ba28 / r289 | Real command-line helper safely returns BootError; its exact production map_err(to_string) operation aborts when the thread-local allocator refuses the next allocation. Cargo101/SIGABRT. Diagnostic driver0 means the expected abort was captured, not that product is safe. Restored ordinary composer4/0. | managed-cmdline-report-oom-ba28-v1 |
| 37a62 / macOS r280 | Full supported seven-target scripts/lint0, lockf and cargo-j4. No later source inherits it. | macos/managed-seven-target-37a6-v1 |
| bce / macOS r275 | Independent native test-only pthread prototype build/clippy/event exchange/expired wait/join/destroy0 under main-thread Rust global allocation refusal. No production adapter or native malloc failure proof. | macos/root-managed-pthread-probe-launch-v1 and root-managed-pthread-probe-evidence |

Network cases are fresh guest namespaces on the ordinary-user Fedora KVM host,
using the retained experimental kernel; no host network configuration or root
operation. They demonstrate transparent responders and independent observations,
not an owned bidirectional forwarding relay or full route-policy integration.
The missing-listener baseline and fallback experiment are not a production fix.
Actual namespace IDs, source/destination tuples and exact binary bytes are recorded.

The ba28 bounded verdict is specifically **NoNewBackendEffectBeforeDeadline**.
It is distinct from ContainedRefusal. Its proof requires known baseline1/1,
ConnectCall/Deadline and a joined worker with the expected terminal state. Any
second backend accept/receipt or successful backend client is DirectBackendEffect.
Unknown/error stages cannot be relabelled a pass. The observation record explicitly
marks behavior after the original window Unverified. DROP can cause timeout rather
than a prompt refusal; no stronger claim follows from this campaign.

The initial local mutation's ENOTCONN record loss motivated preserving a worker's
actual Completion report separately from its I/O Result. Both workers are joined
before propagating failure. That does not suppress errors or invent counters.
The initial root wrapper1 remains intact rather than overwritten by later results.

## Finite task admission, recovery and held cancellation

The fixture uses the same actor tuple across Ready, Refused and Recovered. It
observes exactly one visible actor for that tenant plus its owned held children;
trusted internal reapers are intentionally absent from top. A source reservation
prediction is not an observed population or cgroup membership count. Earlier
pre-Fill hidden-reaper failure is retained in managed-central-followup, not called
a product bug or successful exhaustion proof.

Ready reports FD5. Refused reports held2, reaped0, errno11, FD10 and matching
visible children. Independent main/HARNESS nonces progress after refusal. Release
then reports held0, reaped3, replacementPID6, FD6, with the same actor identity and
fresh neighbor/main replies. The held-cancel case sends NO Release, retains
control streams through whole-run stop, and observes no Recovered publication.
No terminal actor publication is required after cancellation. Stop/remove status
alone does not establish tenant-only cancellation, complete kernel resource
return, cgroup task totals, process/thread fairness or host descriptor reclamation.
Meaningful guard-removal and repeated resource-return campaigns remain owed.

## Reporting allocation failure: real helper, unsafe caller

The r289 probe constructs the overlong input before allocation refusal, uses the
real cmdline composer/BootError, then exercises the exact production conversion
into String under fail_after0. The typed helper error exists before injection.
The formatter aborts; the asserted returned-result path is never reached. Source
and diagnostic patch, cargo command, stdout/stderr, original/restored hashes and
restored diff are retained. It is not a whole VM constructor, stderr cold-start,
or every startup error reproduction. The approved next implementation propagates
the typed cause through actual startup callers; its validation is still owed.

Do not warm a mutex/stdio, inflate a buffer/deadline, skip the fault test, replace
the error with an empty String or move the formatting allocation later to claim
this fixed. Existing legacy error paths and the separate MR-OOM-MAC-1 owned API
mutex abort remain open. The separately signed main Held::find repro also aborts
in its outer reporter; it is not silently included in this root r289 claim.

## Reader method comparison, no adoption

Root r282 uses the exact f26 reader and unadopted dff preferred-size prototype
through the committed exact-source three-way harness. Each method is sampled4096
times, interleaved std/candidate/preferred/preferred/candidate/std over four rounds,
512 reads per process. Raw samples, generated byte-identical extracted sources,
compiler/probe identities and per-process accounting are retained in
linux/managed-proc-reader-three-way-v1. No production code is selected by this run.

| Method | p50 ns | p90 ns | p99 ns | max ns | minor / major faults |
| --- | ---: | ---: | ---: | ---: | ---: |
| Rust1.98.1 std | 6538 | 11135 | 16114 | 89996 | 50 / 0 |
| f26 fallible | 6141.5 | 12146 | 14761 | 57616 | 48 / 0 |
| metadata preferred-size prototype | 8434.5 | 10365 | 16166 | 141669 | 32 / 0 |

For the actual325-byte records, std uses1 allocation/4 reallocations/cumulative
requested992 bytes/retained capacity512; f26 uses1/6/1016/512; preferred uses
1/0/1024/1024. Cumulative requested bytes are replacement layouts, not simultaneous
retained memory. Timing disables allocation instrumentation. The preferred size
comes from that opened file's metadata on each read, not a cache or a measured
constant, but it retains twice the capacity and no speed win is established here.
No confidence bound, significant winner, allocator/kernel-resident cost, cold
filesystem claim, competitive4–5x result or resource policy follows. Strace is
unavailable; syscall counts are unmeasured, not inferred. Independent host/noise,
contention, parent/kernel charges and complete API/VM behavior remain owed.

## Native synchronization proof limits

The selected source is a standalone test-only, publish=false prototype. It pins
fallibly allocated pthread objects before initialization, models init/wait/signal/
destroy errors and keeps uncertain destruction explicit. The root macOS r275
launcher differs from the original only in owned output prefixes; both source
hashes are retained. Actual Rust global refusal does not inject native libc
malloc failure, prove every cancellation race or establish cost/whole-client
safety. No warming or production replacement was adopted. The original owned
API cold std Mutex allocation abort remains open.

## Transfer integrity, policy and remaining gates

Archives remain immutable on their originating hosts and in local transfer
copies. They contain regular evidence/source files, excluding executable/object/
PDB artifacts, whose original hashes remain recorded. Extraction rejects absolute
and parent paths, links and executable magic; no opaque cache/source was removed.
All committed regular files are covered by SHA256SUMS; transfer.json names hashes.

R01/R04/R06/R07/R13: explicit fixture results, preserved I/O failures and live
reporting abort; no whole-path panic-free claim. R02/R03/R05/R12: linked pinned
TPROXY contracts/alternatives and test-only sync/reader designs; actual events and
one owner; no cache, arbitrary product cap, duplicate installed binary, host root
or crypto weakening. R08/R14: comparative distributions and misses retained; full
parent/kernel/competitive costs open. R09/R10/R15: exact roots, live effects,
meaningful fallback removal/restoration, durable raw traces and coordinator role.
R11: only named KVM/native hosts and exact37a seven-target build gate; later
source/full hardware VM combinations unverified, Windows ARM explicitly paused.
R16: admitted private resources, active caches/source/evidence preserved and Mac
floor respected. Full P0–P13 plan, hard aggregate guarantees, mandatory routes,
semantic broker/adapters, credential and generation-safe restore gates remain open.
