# Backend work return with duplicate or missing completion notification

IMPLEMENTER `01a1c5d2ee0e7248982c95a59f1fd905`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Own branch `p/managed-relay-backend-notice`
starts at signed/pushed `015f047b9ffb95851115af4436865b59e3c1ed9e`. Earlier
branches/checkpoints remain preserved. Full canonical policy, evidence, CLAUDE,
managed plan/handoff readback loaded at main69dbc3180; policy files unchanged
sincef5cec365, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Proposal `relay-backend-notice-proposal-v1` and POLICY_READ preceded source edits.
This continues the approved duplicate/missing boundary and early-completion
fixture queue; it is not a new production remedy.

ROOT owns all actual descriptor/socket/namespace/cgroup/BPF/VM/failure/mutation
campaigns, audits, profiles, combined gates and main merges. No child session or
goal expansion. Same exclusive worktree/index, no local build/new full worktree.
The separate6b native pipe/eventfd proposal remains unimplemented pending review;
this fixture uses existing descriptor/signal seams only.

## Four finite selected cases

```
sock-addr-backend-notice4 {connected|unconnected} WORK_MS BUFFER_BYTES {duplicate-complete|missing-complete}
```

Explicit BackendNotification controller/cancellation variants reuse the same
Work::CompleteAfterReceipt, scripted decoder and existing BoundaryNotice enum.
Ordinary backend-end and extra cases explicitly choose Once and retain their
original single uint64 signal with no negative-notice record. Only the new cases
select one aggregate word2 or omit the write after controlled work return.

The baseline Binary+Empty completes first. Actual third Binary backend receipt
is counted/recorded/validated, no forwarded reply is issued, and scripted()
returns BackendComplete while dropping its native sockets. BackendWorkReturned
continues to identify work-function return, with thread termination explicitly
unobserved until join. The selected notification follows that record. A record/
notification/OS failure remains typed I/O with retained facts and cannot satisfy
joined BackendComplete.

The controller FIRST consumes existing relay receive-stage entry, THEN observes
the completion notice under the same original absolute deadline. Thus an early
duplicate rejection cannot initiate teardown before UpstreamReceive entry.
This is not an observed kernel-sleep claim. Existing BoundaryObservation states
remain Unobserved/Accepted/Rejected(ErrorKind); the new BackendReturnObservation
renders this phase. Accepted/other-kind/unobserved cannot pass.

| Notice | Controller rejection | Actual joined backend | Actual relay after owned teardown |
| --- | --- | --- | --- |
| Duplicate | InvalidInput from exact-one reader consuming2 | BackendComplete, consumed3/validated3/empty1/replies2 | Cancelled, UpstreamReceive/KernelAccepted, ingress1/sent1/no replies |
| Missing | TimedOut at original deadline | Same completed-work facts | Io/TimedOut, same stage/progress/retained counters |

Duplicate uses the existing cleanup wake; CancelledByOwnedTeardown does not
rewrite the controller's InvalidInput into successful cancellation. Missing
retains original expiry even if cleanup later writes cancel. Neither emits a
successful CancelBoundary or PreSendRelease. If original time expires before
duplicate teardown, its strict required Cancelled terminal fails; no alternate
outcome, grace, replacement deadline or picked wait hides that failure.

The new expired_upstream_receive predicate checks exact typed expiry/stage/
progress/step/counts. The controller requires the actual completed backend plus
observer rejection and selected relay terminal, then the existing owned child
reap, joins, every acquired link detach and cgroup removal. No new manager,
cache, polling scheduler, decoder or synthetic WouldBlock. The original trusted
pre-send barrier alone retains cancel=None/same absolute at; real I/O keeps cancel.

The ignored host case requires independent actual baseline+third backend bytes,
source/original tuple/ordinal/metadata and distinct owned upstream identity;
actual work return and joined terminal; rejection/relay counters; actual owned
cleanup. It accepts owned-child SIGKILL or its original code1 receive-expiry only
with matching records. An aggregate write can wake the observer before its
sender logs, so no cross-thread sender/observer log order is assumed. Only causal
worker/control ordering is asserted. UDP has no stream EOF; delivered/sent facts
remain retained with no rollback claim.

## Basis, alternatives and literal bounds

The aggregate-counter fixture input follows
[Linux v6.18 eventfd_ctx_do_read](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/eventfd.c)
and [eventfd(2)](https://man7.org/linux/man-pages/man2/eventfd.2.html).
Kiln research `01a1c5d332c07528b80098796389b351` findingf7: **B/76**, both quoted
source spans found; inference from pinned code/official docs, not actual new
runtime proof. One aggregate2 input avoids two-write scheduling dependence.
The source is intentionally versioned; its grade is not a measured kernel result.

Existing [notification alternatives](../managed-relay-precommit/BOUNDARY-DESIGN.md)
and [controlled completion](../managed-relay-backend-end/README.md) retain primary
Linux contracts, pinned Tokio/Envoy comparisons and current ownership limits.
An extra queue/pipe/thread/timer/manager adds scope without serving this finite
case. Join-only after cleanup would not exercise the earlier completion signal.
UDP completion must remain work/terminal evidence rather than invented EOF
(prior primary UDP inferenceC/68); actual new cases remain ROOT-unrun.

Counts derive from Binary+Empty+one forwarded request and the eventfd uint64 ABI.
Caller supplies work/boot time, envelope, vCPU/memory, kernel/config and fresh
records. Ports remain OS-assigned; no production default/threshold/grant or host
setting changes. PayloadCapacityOnly excludes allocator/parent/kernel/console/FD
costs. Logical counters/headroom admission do not prove a hard kernel budget.

## Actual implementer light evidence

Own r20, Fedora amd64 `8fadc1ec213332e16cb0d915a3fb913f`, rustc1.98.1, NEW private
`~/managed-relay-backend-notice-light-v1/{source,guest/target,host/target}`,
Kiln1CPU/1GiB/2GiB sync:false/jobs1. Policy/owned rustfmt0; GNU46+musl46 pure
passed; both scoped production panic/unsafe lints0; copied host compile0; nine
exact pure filters each1/pass1. Inert common stubs are compile scaffolding,
never execution artifacts. Actual shared parente1637959/module hashes are in
composition.json. Ignored VM bodies compiled and never executed.

[light](light/) retains52 raw text members, exact commands/logs/results, Cargo
locks/scaffolds, source manifests and three built binary identities. All52
member hashes and six final source/wrapper hashes verified on receipt.
In-memory compressed text13605B, SHA256
`4f31cb7aeec08bddcff05005abdfb6faf98d4808e33e71b10dcd0a751f632442`;
no executable/local archive transferred. Remote originals/source/targets/binaries
remain retained. No runtime source changed after checks.

[source-invariants.json](source-invariants.json) identifies unchanged Deadline,
forward real I/O, top-level scripted decoder, notification reader, worker
cancel/join source blocks and all32656 prior host-case bytes as an exact prefix.
Every old strict assertion remains; shared host parent/header/helper untouched.

No actual descriptor/socket/VM/failure/mutation/stress/profile/benchmark ran here.
No local target/cache/heavy build/archive/full worktree/deletion or protected
ROOT cache/paused Windows ARM access. ROOT rebuilds matching normal guest/host/
carried helpers before actual tests; old artifacts prove no new-source gate.
Composed seven-target/hardware and cost/cleanup/leak-free claims remain unverified.

Pure/light replay, NEW private admitted outputs only:

```
python3 docs/research/measurements/managed-relay-backend-notice/light.py guest NEW_PRIVATE_GUEST
python3 docs/research/measurements/managed-relay-backend-notice/light.py host NEW_PRIVATE_HOST
```

## ROOT work owed

[ROOT-CASES.json](ROOT-CASES.json) names four selected invocations. ROOT supplies
normal matching artifacts, experimental kernel/config and original caller
inputs, SHARDS_SOCK_ADDR_MODE and SHARDS_BACKEND_NOTICE:

```
ACTUAL_TEST_BINARY cancel::direct_guest_udp4_backend_return_notice_keeps_completed_work_and_typed_rejection --ignored --exact --nocapture --test-threads=1
```

ROOT retains actual third bytes/work return, typed InvalidInput versus original
expiry, joined backend/relay and owned cleanup. Existing backend-end/extra/ready-
notification, strict pre-send/receive and scripted/two-leg controls remain owed.
ROOT owns meaningful actual guard-removal/exact restore/fresh controls, record/
notification/OS/worker/child failure cases, combined matrix/hardware and syscall/
allocator/copy/fault/parent/kernel/neighbor cost. No new mutation is selected or
executed here. Further production remedies require actual ROOT failures first;
this signed progress checkpoint does not close the lane.

## R01–R16 delta evidence

| Rule | Evidence / remaining gate |
| --- | --- |
| R01 | Existing fallible signal/record/checked-count/owned-cleanup paths; both scoped denied panic/unsafe lints0. OOM/OS/report failures ROOT-unverified. |
| R02 | Proposal-first primary counter contracts/graded inference and pinned alternative records above; actual effects/cost unrun. |
| R03 | Script/ABI-derived counts, one aggregate duplicate/omission, original caller budgets; no arbitrary production constant/default/threshold. |
| R04 | Explicit variants/observation states, typed error/expiry versus actual work return/join/cleanup cancel. Pure negatives pass; actual transitions owed. |
| R05 | Existing worker/decoder/deadline/signal owner, no cache/poll scheduler/new manager. ROOT audits actual waits. |
| R06 | One finite selected phase at current seams, no framework/parallel manager. ROOT combined source review owed. |
| R07 | Comments explain receive-before-teardown and log-order obligations, no kernel sleep/EOF/rollback claim. |
| R08 | No4–5x/comparative performance result; ROOT benchmarks owed. |
| R09 | New fixture coverage only; no source-predicted production fix. Prior pending-cancel actual red/correction separately retained on main. |
| R10 | Source invariants/raw logs/manifests/binary IDs/root case list retained; new actual red/green/mutation/restoration owed. |
| R11 | amd64 GNU/musl pure/light only; composed seven-target/actual supported hardware owed. macOS arm64 only; Windows ARM paused. |
| R12 | Existing native seams, no installed binary/OS duplication/root/security/grant/trust/crypto change. |
| R13 | Old strict bytes preserved, no grace/picked wait, completed/sent facts survive observer failure. |
| R14 | No implementer audit/profile/benchmark; syscall/allocation/copy/fault/parent/kernel/neighbor costs unmeasured. |
| R15 | Scoped source/pure checks/signed push only, ROOT actual/combined/review/mainmerge. Active goal and actor inventory unchanged, no delegation or closure. |
| R16 | Same worktree/index, NEW remote admitted proof, no local heavy work/deletion, protected ROOT/evidence/paused VM ownership preserved. |
