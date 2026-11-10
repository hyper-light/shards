# Controlled backend work return, without a UDP reply

IMPLEMENTER `01a1c5d2ee0e7248982c95a59f1fd905`, coordinator
`01a148884a4770fdaf0111f3f792a248`. Branch `p/managed-relay-backend-end` reuses the
existing worktree from strict `200089e47bc125037a350e0a3954863a86bb6d47`.
Receive `7d01dcaf`, strict200, notification `f8d643c59` and correction
`cd5d7212cc61e9196b5afe7a3e2891ba60a74529` branches/checkpoints stay preserved.
Full canonical policy/plan/handoff readback preceded edits; policy files
`f5cec365`, rules SHA256
`d634fcb944959ce1e40a9277d12b0905fcfcebf82d9da9832e826f790a349242`.
ROOT owns all actual event/socket/namespace/cgroup/BPF/VM/failure/mutation runs,
audits, profiles/benchmarks, combined gates and main merges. No subagents.

This is approved finite fixture coverage after the concrete proposal was sent
with key `relay-controlled-backend-end-design-v3`. It fixes no inferred product
bug and reports no new actual UDP outcome. Deadline is byte-identical to
strict200 on this independent branch; ROOT composes the separately validated
pending-I/O correction. This is not a production relay or a managed-plan closure.

## Explicit case, ownership and facts

```
sock-addr-backend-end4 {connected|unconnected} WORK_MS BUFFER_BYTES
```

The existing baseline Binary+Empty, one retained forwarded Binary, original
absolute work deadline, explicit envelope, private namespace/cgroup, native
sockets, existing worker/decoder and selected FixedOriginalPeer hooks remain.
No new grant, actor FD, installed binary, source-port repair or manager.

New finite ControllerKind/Cancellation/Work variants select backend completion.
The existing ReplyPlan goes CompleteAfterBaseline → CompleteAfterForwardedBinary
after the two actual baseline replies. It consumes, records and validates the
actual third Binary receipt, then returns typed BackendComplete at BackendComplete
stage without issuing a reply. Report facts remain arrivals3/matched3/one Empty/
two baseline replies. The two owned native sockets drop when scripted() returns.

Only afterward, the existing worker closure records BackendWorkReturned and
signals the work-return eventfd. A successful signal means the **work function
returned**, not that its thread exited. Both that record and the parent's
BackendReturnBoundary explicitly retain thread_termination UnobservedUntilJoin.
The parent waits this signal plus the separate ReceiveStageEntered signal under
the same original deadline, then requests the existing relay cancellation.
The actual third backend bytes/tuple are an independent witness; neither signal
alone proves delivery, thread exit, a kernel sleep or all resources returned.

The same owned child is killed/reaped; existing backend/relay joins then confirm
the actual BackendComplete terminal and forward Cancelled at UpstreamReceive /
KernelAccepted with ingress1/sent1/no reply. All selected link detaches and cgroup
removal follow. Positive original time after joins remains mandatory. No early
parent join/double-finish, parallel lifecycle owner or polling manager is added.

Notification/report failure becomes a typed I/O error, including TimedOut;
consumed/validated/sent report facts are retained. The earlier work-return record,
if successfully written, remains distinct from a later notification failure.
Missing or malformed records fail. Cancellation and expiry cannot satisfy the new
BackendComplete predicate. Existing child unknown-reap pending behavior and
OS/report-error cleanup still require ROOT fault cases, not an invented grace.

The host module adds one selected ignored case while preserving the original
receive/pre-send strict predicates and their case-specific checks. It retains
actual exact baseline and third backend bytes/source, actor namespace/cgroup and
original endpoint predicates, work-return/receive-entry before cancel, real joined
terminals, no forwarded reply/extra datagram, both child reaps and shared strict
hook/cleanup assertions. ROOT wires the existing cancel module; no shared host
parent/header/helper was edited.

## Primary basis, alternatives and finite bounds

The [receive record](../managed-relay-cancel/README.md),
[strict pre-send record](../managed-relay-precommit/README.md) and signed
conditional design on `p/managed-relay-boundaries` at `756352582` retain the
primary [Linux v6.18 eventfd](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/eventfd.c),
[poll](https://raw.githubusercontent.com/torvalds/linux/v6.18/fs/select.c),
[eventfd(2)](https://man7.org/linux/man-pages/man2/eventfd.2.html) and
[UDP contract](https://man7.org/linux/man-pages/man7/udp.7.html), with established
Envoy/Cilium alternatives in the original forwarding record. Prior ROOT actual
forwarding/receive results justify this finite boundary coverage; they do not
prove it. UDP has no reliable stream EOF, and a missing reply is not proof of
remote death or permanent nondelivery.

Reuse the current decoder/work owner and two one-shot signals. A separate thread
monitor/manager, pipe/channel adapter or repeated state query adds unnecessary
ownership/wait machinery. A sleep or is_finished probe does not establish the
work boundary. Counts derive from Binary+Empty plus one held Binary; notification
words are uint64 ABI values. Work/envelope are caller-supplied, ports OS-assigned.
No production default, arbitrary capacity, extra timeout or authority is added.
PayloadCapacityOnly still excludes allocator/parent/kernel/console costs.

## Actual light checks and exact evidence

Own r10, Fedora amd64 `8fadc1ec213332e16cb0d915a3fb913f`, rustc1.98.1, NEW
`~/managed-relay-backend-end-light-v1/source`, private guest/host targets, Kiln
1CPU/1GiB/2GiB admission. Policy/owned rustfmt0, GNU36/musl36 pure tests0, both
scoped production panic/unsafe clippy subsets0. Source-linked host composition
with actual shared-assertion parent `e1637959af1dbae631aa6f91c0d4d227f4c7dfac`
and copied cancel module compiles0 using inert type-matching common stubs;
six exact pure filters each1/pass1. Ignored VM bodies compiled, never executed.

Pure additions reject unknown/missing arguments, wrong work terminal/cancel/
expiry/notification stage, unjoined results and missing validations while
preserving receipt/reply counts. No eventfd/socket/namespace/cgroup/BPF/guest mode,
profile, benchmark or heavy validation ran. These are not normal actual helpers,
the full workspace/seven-target gate or hardware validation.

[light/](light/) retains all exact command/log/result JSON, Cargo locks/scaffolds,
source manifests/composition and three test-binary identities. All46 text members
and six final source paths were independently hash-matched locally. In-memory
text transfer13604B SHA256
`eccebea9a9a92bb089552568a49141508e762743fef4f56f5918add7a4e6e787`;
no executable/local archive transferred. Remote originals/source/targets/binaries
remain intact. [source-identities.json](source-identities.json) also verifies
the unchanged original Deadline body. No runtime source changed after these checks.

Replay uses the same source-linked light scaffolds, NEW private output paths and
admitted envelope:

```
python3 docs/research/measurements/managed-relay-backend-end/light.py guest NEW_PRIVATE_GUEST
python3 docs/research/measurements/managed-relay-backend-end/light.py host NEW_PRIVATE_HOST
```

## Owed ROOT actual gates and continuation

With explicit source-matching normal artifacts/kernel/config/budgets, ROOT runs:

```
SHARDS_SOCK_ADDR_MODE=connected SHARDS_SOCK_ADDR_BUFFER_BYTES=18 \
cargo +1.98.1 test -p shards --test managed_network --profile test-release \
  cancel::direct_guest_udp4_backend_completes_without_reply_before_relay_cancel \
  -- --ignored --exact --nocapture --test-threads=1
```

Repeat unconnected, fresh records.18 is the existing9-byte request+9-byte reply
envelope, not a production default. ROOT owes actual independent receipts,
work-return→cancel→real joins and complete owned cleanup, original expiry versus
error/cancel, report/notification/OS/worker/child failures, meaningful actual
reply/terminal guard-removal effects and exact restoration, original strict
receive/pre-send/ordinary regressions, combined matrix and supported real hosts.
No new actual case or performance/hard-budget result is claimed here.

ROOT reported r626 for the **separate correction** complete0: corrected pre-send
both0, precheck-only mutant connected strict101 with actual Backend3, exact
restoration/hash/empty diff, restored pre-send/receive/forward/scripted both0.
Ten positive0 and one expected mutant101/all cleanuptrue; seven-target correction
gate still owed at this receipt. Those results do not become a passing gate for
this new backend fixture. Next independent queue item is one actual extra
datagram through the existing held decoder with independent bytes and typed
UnexpectedDatagram/work-return/join witnesses; proposal sent before source edits.
Only ROOT can close/release the ongoing lane.

## R01–R16 delta record

| Rule | Evidence / remaining gate |
| --- | --- |
| R01 | Fallible existing eventfd/I/O/worker/owned cleanup; no new panic/unchecked index/arithmetic in runtime changes. Scoped denied GNU/musl lints0; actual OS/OOM failures owed. |
| R02 | Concrete existing-seam proposal and primary/established alternatives above; actual baseline basis retained, new effect/cost proof owed. |
| R03 | Two signals, uint64 word, Binary+Empty+one counts derive from scenario/ABI; no runtime default/grace/capacity. |
| R04 | Explicit plan/stage/terminal and work-return versus thread-join distinction. Pure wrong-terminal/expiry/receipt guards pass; actual transitions unverified. |
| R05 | Existing one authoritative worker/decoder/deadline owner; one-shot signals, no cache/repeated state query/manager. ROOT audits actual waits. |
| R06 | One finite Work variant and explicit terminal, small shared predicate at two actual call sites. No general framework. |
| R07 | Comments state socket ownership/work return and unobserved thread termination until join; no EOF/kernel-return/rollback claim. |
| R08 | No comparative4–5× result; ROOT measurements owed. |
| R09 | New fixture coverage, not an inferred production fix. Separate actual pending-cancel red/correction stays on its own branch. |
| R10 | Exact source/light/command/binary identities retained; new actual effect/mutation/restoration unrun. |
| R11 | amd64 GNU/musl pure/light only; seven-target/actual supported VM hosts owed. macOS ARM64 only, Windows ARM pause preserved. |
| R12 | Existing native seams, no installed binary/platform adapter/host root/grant/kernel/trust/crypto change. |
| R13 | Strict original predicates preserved; existing cleanup owner/joins retained, no double join or timing trick. |
| R14 | No implementer audit/profile/benchmark. Parent/kernel/allocator/copy/fault/neighbor costs unknown. |
| R15 | Scoped implementation/pure checks only, signed named-path push; ROOT actual gates/main merges, no lane closure at checkpoint. |
| R16 | Same worktree, NEW remote1CPU/1GiB/2GiB source/targets; no local build/cache/full copy/deletion, all ROOT targets/evidence/paused VM preserved. |
