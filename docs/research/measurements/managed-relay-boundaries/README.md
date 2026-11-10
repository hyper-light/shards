# Finite duplicate and missing relay boundary notifications

IMPLEMENTER `01a1c5d2ee0e7248982c95a59f1fd905`; coordinator
`01a148884a4770fdaf0111f3f792a248`. Existing worktree/branch
`managed-relay-cancel` / `p/managed-relay-boundaries`, base docs checkpoint
`756352582` on unchanged strict source `200089e4`. Original receive `7d01dcaf`
and pre-send `200089e4` branches remain preserved. Full canonical policy/plan/
handoff read before edits; policy files `f5cec365`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
Current coordinator-reported main is `096c9c7be1752a5eaf99eda2c7eb4723c2a8f6db`;
policy bytes are unchanged. ROOT owns all audits, actual descriptor/socket/VM/
namespace/cgroup/BPF/failure/mutation campaigns, profiles, combined gates and
main merges. No subagents or local build.

## Scope and actual state

This implements the user-approved duplicate/missing notification queue after
the concrete [proposal and alternatives](../managed-relay-precommit/BOUNDARY-DESIGN.md)
were sent to ROOT. It does **not** change Deadline, actual send, cancellation
observation, worker finish or either original strict cancellation predicate.
The new command has explicit arguments and no default:

```
sock-addr-boundary4 {connected|unconnected} WORK_MS BUFFER_BYTES {duplicate-ready|missing-ready}
```

The ordinary Binary+Empty baseline and selected FixedOriginalPeer hooks remain.
Existing Control/ControllerKind/Cancellation variants carry a finite BoundaryNotice
enum. Once preserves the old pre-send signal; Duplicate writes one aggregate
eventfd word of two; Missing performs no write. The latter two issue a typed
BoundaryNoticeIssued record. One aggregate write avoids a scheduler-dependent
pair of separate writes. No new manager, decoder, cache or repeated state probe.

After actual ingress bytes/tuple are recorded and validated, the worker retains
ingress1/NotAttempted and waits at the existing trusted barrier. Its cancellation
is explicitly None **only at that test barrier**, with the same original absolute
`deadline.at`. The controller NEVER releases this gate for notification negatives.
It records actual BoundaryObservation as Accepted or Rejected plus a separate
io::ErrorKind; Unobserved/Accepted/other errors cannot pass the internal predicate.
Duplicate requires actual InvalidInput from the existing exact-one reader;
Missing requires original TimedOut. Neither becomes successful cancellation.

Both selected negatives deliberately leave the worker until original expiry;
its joined ForwardCompletion must be Error / BeforeUpstreamSend / NotAttempted,
ingress1, all send/reply/Empty counts zero, with ForwardError Io/TimedOut. There is
no positive-remaining-time requirement after deliberate expiry and no grace.
The controller ends/reaps its owned child, joins relay before backend and then
attempts every link detach/cgroup removal. If a send unexpectedly completes, the
same independent backend receipt path is retained before ending its observer;
missing receipt/report cannot become proof of nondelivery.

Backend completion requires exactly two independent baseline receipts and
validations, one Empty, two baseline replies and ReceiveDatagram stage. It may
actually finish Cancelled or Error/TimedOut; those remain separate records. The
new host case checks the exact native outcomes rather than recasting expiry as
Cancel. It requires both actual child reaps and accepts only the owned child's
SIGKILL or its natural code1 receive-expiry path, with corresponding actual
records. Original receive and strict pre-send cases retain their previous exact
assertions. Existing child unknown-reap pending behavior and OS/report/cleanup
failure handling still require ROOT's bounded failure runs; this is not proof of
full population/kernel return or cleanup success under every OS error.

The focused host case reuses the shared strict hook/cleanup helper and checks
actual baseline/backend/retained-ingress bytes, namespace/cgroup identities,
original endpoint presentation, payload envelope and typed error/terminal/counts.
No forwarded reply, held backend request, successful CancelBoundary/PreSendRelease,
extra backend datagram or ForwardExchange can pass. ROOT wires the existing
cancel.rs module; this lane does not edit the shared host parent/header/helper.

## Actual implementer checks

Fedora amd64 `8fadc1ec213332e16cb0d915a3fb913f`, rustc1.98.1, NEW private source and
targets, Kiln admission **1CPU / 1GiB / 2GiB**; admission is not a hard cgroup
budget. No eventfd, socket, namespace, cgroup, BPF or VM fixture executed.

| Own run | Result |
| --- | --- |
| r4, `~/managed-relay-boundaries-light-v1` | Policy/format0, GNU37/musl37 pure tests0. GNU scoped clippy101 on single_match in the new notifier; musl lint/host commands were unrun. Source/log/targets retained. |
| r5, `~/managed-relay-boundaries-light-v2` | After only changing that match to if-let, fresh policy/format0, GNU37/musl37 pure tests0, both scoped production panic/unsafe lints0. Actual sibling parent + copied cancel module compiles0 with inert type-matching common stubs; six exact pure filters each1/pass1. |

Ignored real VM bodies compile but are **not passing runtime tests**. The copied
parent is exact shared-assertion revision `e1637959af1dbae631aa6f91c0d4d227f4c7dfac`;
the supplied copy path and parent/module hashes are recorded in composition.json.
No tracked host parent was edited. No full normal helper/workspace/seven-target
or actual supported hardware result is claimed for this checkpoint.

[light/](light/) retains all exact command JSON/logs/results, source manifests,
Cargo locks, scaffold source and five built test-binary identities. All 68 text
members and six final source paths were independently hash-matched locally.
Transport was compressed in-memory text JSON, 17368B SHA256
`d0e8f1c7a126a599a8727f61ed1bf2480f3ce17015be803962165edd8dfb44b0`;
no executable or local archive. Remote originals/targets/binaries remain preserved.
[source-identities.json](source-identities.json) identifies the final runtime
and wrapper bytes. This record was added after those source checks; no runtime
source changed afterward.

Local free space fell below30GiB and was reported. ROOT later restored headroom;
this implementer created no local target/cache/full worktree/mirror and deleted
nothing. Protected ROOT Fedora/Mac targets and paused Windows ARM remain untouched.

Pure/light replay, NEW private directory arguments and admitted envelope:

```
python3 docs/research/measurements/managed-relay-boundaries/light.py guest NEW_PRIVATE_GUEST
python3 docs/research/measurements/managed-relay-boundaries/light.py host NEW_PRIVATE_HOST
```

## ROOT evidence and next independent work

Coordinator supplied an **actual pre-send red** while this independent checkpoint
finished: r597 exact probe `0456e99ef475e9e374af0685232c236a3d7b0a4c`, source-matching
normal host/helpers compile0, both pre-send modes strict101. After Requested and
ReleasedAfterCancelRequest, independent Backend ordinal3 records actual Binary
`5245515545535400ff` from the owned upstream; ForwardCompletion is Cancelled at
UpstreamReceive / KernelAccepted with ingress1/sent1/no replies. Both worker
joins/link detaches/cgroup removal are true, original time remaining about29.9s.
ROOT retains raw `~/root-managed-boundary-0456-v1/precommit-{connected,unconnected}/`
observed.json/test.log/stdout files. This summary is coordinator-provided, not a
new implementer run. It establishes the earlier inference's effect and permits
the separately proposed narrow pending-cancel correction. **This checkpoint
does not repair that red.** The strict pre-send test is not relaxed.

Next is an isolated correction branch from the preserved strict200 source:
check the existing cancel FD without consumption once before each actual I/O
attempt, after the original deadline check. Pending-before-check priority is
distinct from the unavoidable cancel-after-check race. ROOT owns actual mutation/
restoration, pre-send green and receive/ordinary regressions. Early controlled
backend completion and finite peer-end/extra-datagram proposals remain independent
approved queue work; their source/runtime results are not claimed here.

ROOT invocation for this notification batch, with its explicit experimental
kernel/config/source-matching normal artifacts and original supplied budgets:

```
SHARDS_SOCK_ADDR_MODE=connected SHARDS_SOCK_ADDR_BUFFER_BYTES=18 \
SHARDS_BOUNDARY_NOTICE=duplicate-ready \
cargo +1.98.1 test -p shards --test managed_network --profile test-release \
  cancel::direct_guest_udp4_notification_failure_retains_ingress_until_original_expiry \
  -- --ignored --exact --nocapture --test-threads=1
```

Repeat both modes and both explicit notices, fresh records each.18 is the
existing9-byte request+9-byte reply envelope, not a production default. ROOT owes
actual notification words/rejection versus expiry, retained byte/count facts,
child/worker/link/cgroup cleanup, OS/report/error cases, meaningful guard removal
and exact restoration plus combined/actual supported hardware gates. No actual
new notification effect, performance or hard-budget guarantee is reported.

## R01–R16 evidence

| Rule | Delta evidence / remaining gate |
| --- | --- |
| R01 | Fallible existing eventfd/I/O/owned cleanup, checked counters/slots; scoped GNU/musl denied panic/unsafe lints0. Actual OOM/OS/report failures unverified. |
| R02 | Concrete proposal-first record and primary Linux/eventfd/Tokio/Envoy alternatives linked above. Runtime/cost choices remain ROOT work. |
| R03 | One retained ingress, Binary+Empty baseline, uint64 word2 and one omitted write derive from the finite script/ABI. Caller deadline/envelope unchanged; no production threshold. |
| R04 | Explicit Notice/Observation/Control/Completion variants. Pure guards reject accepted/missing/cross-kind/cancel-as-expiry and preserve consumed/sent facts. Actual transitions owed. |
| R05 | Existing worker/decoder/eventfd/deadline owner, one-shot notifications, original expiry, no probes/cache/parallel manager. No implementer audit. |
| R06 | Two finite selected cases at existing seams, shared owner/cleanup; no general failure framework. ROOT source review owed. |
| R07 | Comments explain deterministic aggregate write, unreleased error gate and no observed kernel sleep/rollback. |
| R08 | No comparative4–5× or benchmark result; ROOT measurements owed. |
| R09 | New fixture coverage; known pre-send live red now supplied by ROOT, but its correction is a separate next change. No guessed production fix in this batch. |
| R10 | Exact source/commands/logs/manifests/binaries/red retained; scoped final source hash-matched. New notification live mutation/restoration remains unrun. |
| R11 | amd64 GNU/musl pure/light only. Full seven-target/real supported hardware owed; macOS ARM64 only, Windows ARM remains paused. |
| R12 | Existing native seams only; no new installed artifact, OS adapter, host root, policy/grant/trust/crypto/kernel change. |
| R13 | Original strict commands/predicates preserved; explicit opt-in negative command only. PayloadCapacityOnly still excludes allocator/parent/kernel/console costs. |
| R14 | No implementer profile/audit/benchmark. Allocations/copies/faults/parent/kernel/neighbor costs unmeasured. |
| R15 | Scoped implementer checks, signed named-path checkpoint/push, ROOT actual gates/main merge only; checkpoint does not close the ongoing lane. |
| R16 | Same worktree, no local build/cache/archive, NEW remote1CPU/1GiB/2GiB snapshots/targets. All evidence/active ROOT caches/VMs preserved. |
